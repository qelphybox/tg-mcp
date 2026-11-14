//! The Fake world: the real server, in-process, on a fake Telegram.
//!
//! The fake QR login stays `waiting` until the fake Driver accepts its token,
//! so a scenario proves the same hand-off it relies on in the Real world.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use tg_mcp::{
    Deps,
    auth::clients::ClientRegistry,
    config::Config,
    db,
    sessions::{TokenTtl, Vault},
    tg::{
        Account, FakeConnector, FakeLogin, FakePassword, LoggedIn, Login, LoginAttempt, LoginState,
        TgError, fake::FakeData,
    },
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::world::{Driver, Party, Peer, Server, World};

/// The fake Test account's id.
pub const TEST_ACCOUNT: i64 = 7_000_001;
/// The fake Peer's id.
pub const PEER: i64 = 7_000_002;
/// The fake Test account's Cloud password.
pub const CLOUD_PASSWORD: &str = "correct horse battery staple";

/// The fake Test account.
pub fn test_account() -> Account {
    Account {
        id: TEST_ACCOUNT,
        first_name: "Test".into(),
        last_name: None,
        username: Some("tgmcp_test".into()),
    }
}

/// The fake Peer.
pub fn peer() -> Account {
    Account {
        id: PEER,
        first_name: "Peer".into(),
        last_name: None,
        username: Some("tgmcp_peer".into()),
    }
}

/// A running Fake world and the fake Telegram behind it.
pub struct FakeWorld {
    pub world: Arc<World>,
    /// What the fake Telegram recorded (Drafts, sends, reactions, reads).
    pub connector: FakeConnector,
}

/// How to start a Fake world.
pub struct FakeWorldBuilder {
    test_data: FakeData,
    password: Option<FakePassword>,
    driver_accepts: bool,
    driver_claims: i64,
}

impl Default for FakeWorldBuilder {
    fn default() -> Self {
        Self {
            test_data: FakeData::new(test_account()),
            password: Some(FakePassword {
                password: CLOUD_PASSWORD.into(),
                hint: Some("a staple".into()),
                flood_after: None,
            }),
            driver_accepts: true,
            driver_claims: TEST_ACCOUNT,
        }
    }
}

impl FakeWorldBuilder {
    /// What the Driver's logins come out as (by default the Test account).
    /// The Driver still says it is the Test account: this is how a Grant
    /// whose `whoami` is someone else is made, for the interlock.
    pub fn login_as(mut self, data: FakeData) -> Self {
        self.test_data = data;
        self
    }

    /// A Driver logged into another account than the Test account.
    pub fn driver_is(mut self, account: i64) -> Self {
        self.driver_claims = account;
        self
    }

    /// A Driver that never accepts a QR login.
    pub fn idle_driver(mut self) -> Self {
        self.driver_accepts = false;
        self
    }

    /// Telegram answers the Test account's first Cloud password with
    /// `FLOOD_WAIT_30`.
    pub fn password_flood(mut self) -> Self {
        if let Some(p) = self.password.as_mut() {
            p.flood_after = Some(0);
        }
        self
    }

    pub async fn start(self) -> Result<FakeWorld> {
        let accepted: Accepted = Arc::new(Mutex::new(HashMap::new()));
        let test_login = match self.password {
            Some(p) => FakeLogin::with_password(self.test_data, p),
            None => FakeLogin::new(self.test_data),
        };
        let saved = Arc::new(Mutex::new(None));
        let driver = Arc::new(FakeParty {
            saved: saved.clone(),
            account: self.driver_claims,
            login: test_login,
            accepted: accepted.clone(),
            accepts: self.driver_accepts,
        });
        let peer = Arc::new(FakeParty {
            saved: Arc::new(Mutex::new(None)),
            account: PEER,
            login: FakeLogin::new(FakeData::new(peer())),
            accepted: accepted.clone(),
            accepts: true,
        });
        let login = Arc::new(GatedLogin { accepted });
        let connector = FakeConnector::default();
        let (server, keep) = serve(login, connector.clone()).await?;
        Ok(FakeWorld {
            world: Arc::new(World {
                name: "fake",
                server,
                driver,
                peer,
                test_account: TEST_ACCOUNT,
                peer_account: PEER,
                cloud_password: Some(Zeroizing::new(CLOUD_PASSWORD.into())),
                poll_every: Duration::from_millis(10),
                login_timeout: Duration::from_secs(3),
                lease: crate::lease::Terms {
                    ttl: Duration::from_secs(30),
                    wait: Duration::from_millis(300),
                    poll: Duration::from_millis(20),
                },
                _keep: keep,
            }),
            connector,
        })
    }
}

impl FakeWorld {
    pub fn builder() -> FakeWorldBuilder {
        FakeWorldBuilder::default()
    }

    /// The default Fake world: the Test account (with a Cloud password) and
    /// the Peer (without one).
    pub async fn start() -> Result<Self> {
        Self::builder().start().await
    }
}

/// Stops the in-process server when the world goes.
struct Shutdown(CancellationToken);

impl Drop for Shutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Every request comes from 127.0.0.1, so the per-IP limits of the anonymous
/// endpoints are lifted, as in the server's own tests.
const RELAXED_LIMITS: &[(&str, &str)] = &[
    ("TG_MCP_AUTHORIZE_PER_MINUTE", "10000"),
    ("TG_MCP_AUTHORIZE_BURST", "10000"),
    ("TG_MCP_REGISTER_PER_MINUTE", "10000"),
    ("TG_MCP_REGISTER_BURST", "10000"),
    ("TG_MCP_TOKEN_PER_MINUTE", "10000"),
    ("TG_MCP_TOKEN_BURST", "10000"),
    ("TG_MCP_MEDIA_PER_MINUTE", "10000"),
    ("TG_MCP_MEDIA_BURST", "10000"),
    ("TG_MCP_CONSENT_CHATS_PER_MINUTE", "10000"),
    ("TG_MCP_CONSENT_CHATS_BURST", "10000"),
    ("TG_MCP_ACCOUNT_WRITES_PER_10_MIN", "1000"),
    ("TG_MCP_CHAT_SENDS_PER_MINUTE", "100"),
];

type Keep = Vec<Box<dyn std::any::Any + Send + Sync>>;

async fn serve(login: Arc<dyn Login>, connector: FakeConnector) -> Result<(Server, Keep)> {
    let dir = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let config = Config::from_lookup(
        |key| match key {
            "TG_MCP_PUBLIC_URL" => Some(base.clone()),
            "TG_MCP_FAKE_TELEGRAM" => Some("1".into()),
            _ => RELAXED_LIMITS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string()),
        },
        None,
    )?;
    let pool = db::open(&dir.path().join("tg-mcp.db")).await?;
    let vault = Arc::new(Vault::new(pool.clone(), TokenTtl::default()));
    let deps = Deps {
        vault,
        clients: ClientRegistry::new(pool),
        login,
        connector: Arc::new(connector),
    };
    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(&config, deps, shutdown.clone());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop.cancelled_owned())
        .await;
    });
    Ok((
        Server { base },
        vec![Box::new(Shutdown(shutdown)), Box::new(dir)],
    ))
}

/// The QR tokens accepted so far, each with the login of whoever accepted it.
type Accepted = Arc<Mutex<HashMap<String, FakeLogin>>>;

/// A fake Telegram login that waits until a fake party accepts its QR, and
/// then logs in as that party, as Telegram does.
struct GatedLogin {
    accepted: Accepted,
}

#[async_trait]
impl Login for GatedLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        let mut token = [0u8; 16];
        getrandom::fill(&mut token).expect("the OS has randomness");
        Ok(Box::new(GatedAttempt {
            token: URL_SAFE_NO_PAD.encode(token),
            inner: None,
            accepted: self.accepted.clone(),
        }))
    }
}

struct GatedAttempt {
    token: String,
    /// The login of whoever accepted the QR, once someone did.
    inner: Option<Box<dyn LoginAttempt>>,
    accepted: Accepted,
}

#[async_trait]
impl LoginAttempt for GatedAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if self.inner.is_none() {
            let by = self.accepted.lock().unwrap().get(&self.token).cloned();
            let Some(login) = by else {
                return Ok(LoginState::WaitingForScan {
                    qr_url: format!("tg://login?token={}", self.token),
                });
            };
            let mut inner = login.start().await?;
            // The fake shows its own QR once first; that was ours.
            inner.poll().await?;
            self.inner = Some(inner);
        }
        self.inner.as_mut().expect("set above").poll().await
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        match self.inner.as_mut() {
            Some(inner) => inner.submit_password(password).await,
            None => Err(TgError::Upstream("not waiting for a cloud password".into())),
        }
    }
}

/// A fake Test account (the Driver) or Peer: accepting a QR logs it in.
struct FakeParty {
    /// The Draft in this account's Saved Messages.
    saved: Arc<Mutex<Option<String>>>,
    account: i64,
    login: FakeLogin,
    accepted: Accepted,
    accepts: bool,
}

#[async_trait]
impl Party for FakeParty {
    async fn account_id(&self) -> Result<i64> {
        Ok(self.account)
    }

    async fn accept_login(&self, qr_url: &str) -> Result<()> {
        let token = login_token(qr_url)?;
        if self.accepts {
            self.accepted
                .lock()
                .unwrap()
                .insert(token, self.login.clone());
        }
        Ok(())
    }
}

#[async_trait]
impl Driver for FakeParty {
    async fn saved_draft(&self) -> Result<Option<String>> {
        Ok(self.saved.lock().unwrap().clone())
    }

    async fn set_saved_draft(&self, text: &str) -> Result<()> {
        *self.saved.lock().unwrap() = (!text.is_empty()).then(|| text.to_string());
        Ok(())
    }
}
impl Peer for FakeParty {}

/// The token of a `tg://login?token=…` URL, as Telegram's apps read it.
pub fn login_token(qr_url: &str) -> Result<String> {
    let url = url::Url::parse(qr_url).with_context(|| format!("not a URL: {qr_url}"))?;
    ensure!(
        url.scheme() == "tg" && url.host_str() == Some("login"),
        "not a tg://login URL"
    );
    url.query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .filter(|t| !t.is_empty())
        .context("tg://login URL without a token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_token_of_a_login_url() {
        assert_eq!(login_token("tg://login?token=AbC-_9").unwrap(), "AbC-_9");
        assert!(login_token("tg://resolve?domain=x").is_err());
        assert!(login_token("tg://login?token=").is_err());
        assert!(login_token("nonsense").is_err());
    }
}
