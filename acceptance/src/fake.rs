//! The Fake world: the real server, in-process, on a fake Telegram.
//!
//! The fake QR login stays `waiting` until the fake Driver accepts its token,
//! so a scenario proves the same hand-off it relies on in the Real world.

use std::{
    collections::HashSet,
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
        Account, FakeConnector, FakeLogin, LoggedIn, Login, LoginAttempt, LoginState, TgError,
        fake::FakeData,
    },
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::world::{Driver, Server, World};

/// The fake Test account's id.
pub const TEST_ACCOUNT: i64 = 7_000_001;

/// The fake Test account.
pub fn test_account() -> Account {
    Account {
        id: TEST_ACCOUNT,
        first_name: "Test".into(),
        last_name: None,
        username: Some("tgmcp_test".into()),
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
    login_as: FakeData,
    test_account: i64,
    driver_accepts: bool,
}

impl Default for FakeWorldBuilder {
    fn default() -> Self {
        Self {
            login_as: FakeData::new(test_account()),
            test_account: TEST_ACCOUNT,
            driver_accepts: true,
        }
    }
}

impl FakeWorldBuilder {
    /// The account the fake Telegram logs in as (by default the Test account).
    pub fn login_as(mut self, data: FakeData) -> Self {
        self.login_as = data;
        self
    }

    /// A Driver that never accepts a QR login.
    pub fn idle_driver(mut self) -> Self {
        self.driver_accepts = false;
        self
    }

    pub async fn start(self) -> Result<FakeWorld> {
        let accepted = Arc::new(Mutex::new(HashSet::new()));
        let login = Arc::new(GatedLogin {
            inner: FakeLogin::new(self.login_as),
            accepted: accepted.clone(),
        });
        let driver = Arc::new(FakeDriver {
            accepted,
            accepts: self.driver_accepts,
        });
        let connector = FakeConnector::default();
        let (server, keep) = serve(login, connector.clone()).await?;
        Ok(FakeWorld {
            world: Arc::new(World {
                server,
                driver,
                test_account: self.test_account,
                poll_every: Duration::from_millis(10),
                login_timeout: Duration::from_secs(3),
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

    /// The default Fake world: the fake Telegram logs in as the Test account.
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

/// The QR tokens the fake Driver accepted.
type Accepted = Arc<Mutex<HashSet<String>>>;

/// A fake Telegram login that waits for the fake Driver.
struct GatedLogin {
    inner: FakeLogin,
    accepted: Accepted,
}

#[async_trait]
impl Login for GatedLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        let mut token = [0u8; 16];
        getrandom::fill(&mut token).expect("the OS has randomness");
        Ok(Box::new(GatedAttempt {
            token: URL_SAFE_NO_PAD.encode(token),
            inner: self.inner.start().await?,
            primed: false,
            accepted: self.accepted.clone(),
        }))
    }
}

struct GatedAttempt {
    token: String,
    inner: Box<dyn LoginAttempt>,
    /// Whether the inner fake showed its own QR already (it does once).
    primed: bool,
    accepted: Accepted,
}

#[async_trait]
impl LoginAttempt for GatedAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if !self.primed {
            self.primed = true;
            self.inner.poll().await?;
        }
        if !self.accepted.lock().unwrap().contains(&self.token) {
            return Ok(LoginState::WaitingForScan {
                qr_url: format!("tg://login?token={}", self.token),
            });
        }
        self.inner.poll().await
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        self.inner.submit_password(password).await
    }
}

/// The fake Driver: accepting a QR marks its token accepted.
struct FakeDriver {
    accepted: Accepted,
    accepts: bool,
}

#[async_trait]
impl Driver for FakeDriver {
    async fn accept_login(&self, qr_url: &str) -> Result<()> {
        let token = login_token(qr_url)?;
        if self.accepts {
            self.accepted.lock().unwrap().insert(token);
        }
        Ok(())
    }
}

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
