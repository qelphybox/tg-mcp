//! The real Telegram: QR login and the Client pool, on grammers (pinned exactly).

use std::{
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use grammers_client::{Client, InvocationError, SignInError, client::PasswordToken, tl};
use grammers_mtsender::{ConnectionParams, SenderPool};
use grammers_session::{Session, updates::UpdatesLike};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    pool::{Live, Pool},
    session::SessionStore,
};
use crate::sessions::{OpenedGrant, Vault};

/// Re-export the QR token this long before Telegram says it expires.
const QR_REFRESH_MARGIN: i64 = 5;

/// Our registered Telegram app.
#[derive(Clone)]
pub struct AppCredentials {
    pub api_id: i32,
    pub api_hash: String,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("api_id", &self.api_id)
            .field("api_hash", &"<redacted>")
            .finish()
    }
}

/// How this server shows up in the user's Telegram "Active sessions".
fn connection_params() -> ConnectionParams {
    ConnectionParams {
        device_model: "Unofficial MCP Server".into(),
        system_version: "tg-mcp".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        ..ConnectionParams::default()
    }
}

/// A connected grammers client over our [`SessionStore`].
struct Connected {
    client: Client,
    store: Arc<SessionStore>,
    updates: mpsc::Receiver<UpdatesLike>,
}

fn connect(credentials: &AppCredentials, store: SessionStore) -> Connected {
    let store = Arc::new(store);
    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::with_configuration(store.clone(), credentials.api_id, connection_params());
    let client = Client::new(handle);
    tokio::spawn(runner.run());
    Connected {
        client,
        store,
        updates,
    }
}

fn upstream(err: InvocationError) -> TgError {
    TgError::Upstream(err.to_string())
}

/// Like [`upstream`], but surfaces `FLOOD_WAIT_X` so the Login flow can honour it.
fn login_error(err: InvocationError) -> TgError {
    match &err {
        InvocationError::Rpc(rpc) if rpc.code == 420 && rpc.name == "FLOOD_WAIT" => {
            TgError::FloodWait {
                seconds: rpc.value.unwrap_or(60),
            }
        }
        _ => upstream(err),
    }
}

/// Errors meaning "this Telegram session is gone": logged out, revoked, banned.
fn is_dead_session(err: &InvocationError) -> bool {
    matches!(err, InvocationError::Rpc(rpc) if rpc.code == 401)
}

fn account_from(user: tl::enums::User) -> Result<Account, TgError> {
    match user {
        tl::enums::User::User(u) => Ok(Account {
            id: u.id,
            first_name: u.first_name.unwrap_or_default(),
            last_name: u.last_name,
            username: u.username,
        }),
        tl::enums::User::Empty(_) => {
            Err(TgError::Upstream("Telegram returned an empty user".into()))
        }
    }
}

fn qr_url(token: &[u8]) -> String {
    format!("tg://login?token={}", URL_SAFE_NO_PAD.encode(token))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Did Telegram tell us the QR code was scanned and accepted?
fn has_login_token_update(updates: &UpdatesLike) -> bool {
    let UpdatesLike::Updates(updates) = updates else {
        return false;
    };
    let is_login = |u: &tl::enums::Update| matches!(u, tl::enums::Update::LoginToken);
    match updates {
        tl::enums::Updates::UpdateShort(short) => is_login(&short.update),
        tl::enums::Updates::Updates(full) => full.updates.iter().any(is_login),
        tl::enums::Updates::Combined(combined) => combined.updates.iter().any(is_login),
        _ => false,
    }
}

// ── QR login ────────────────────────────────────────────────────────────────

pub struct GrammersLogin {
    credentials: AppCredentials,
}

impl GrammersLogin {
    pub fn new(credentials: AppCredentials) -> Self {
        Self { credentials }
    }
}

#[async_trait::async_trait]
impl Login for GrammersLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        Ok(Box::new(QrAttempt {
            credentials: self.credentials.clone(),
            conn: connect(&self.credentials, SessionStore::new()),
            current: None,
            password_hint: None,
        }))
    }
}

struct QrAttempt {
    credentials: AppCredentials,
    conn: Connected,
    /// The QR currently shown and when Telegram expires it.
    current: Option<(String, i64)>,
    /// Set once Telegram said the Account has a Cloud password: its hint, if any.
    password_hint: Option<Option<String>>,
}

impl Drop for QrAttempt {
    fn drop(&mut self) {
        // Abandoned or finished: close the connection. A finished login reconnects
        // from the sealed snapshot through the Client pool.
        self.conn.client.disconnect();
    }
}

#[async_trait::async_trait]
impl LoginAttempt for QrAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if let Some(hint) = &self.password_hint {
            return Ok(LoginState::PasswordNeeded { hint: hint.clone() });
        }
        let mut scanned = false;
        while let Ok(update) = self.conn.updates.try_recv() {
            scanned |= has_login_token_update(&update);
        }
        if let Some((url, expires)) = &self.current
            && !scanned
            && expires - unix_now() > QR_REFRESH_MARGIN
        {
            return Ok(LoginState::WaitingForScan {
                qr_url: url.clone(),
            });
        }

        let exported = self
            .conn
            .client
            .invoke(&tl::functions::auth::ExportLoginToken {
                api_id: self.credentials.api_id,
                api_hash: self.credentials.api_hash.clone(),
                except_ids: Vec::new(),
            })
            .await;
        let token = match exported {
            Ok(token) => token,
            Err(err) if err.is("SESSION_PASSWORD_NEEDED") => return self.need_password().await,
            Err(err) => return Err(login_error(err)),
        };
        self.handle(token).await
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        if self.password_hint.is_none() {
            return Err(TgError::Upstream("not waiting for a cloud password".into()));
        }
        // A fresh SRP challenge per attempt, so a retry never reuses a spent srp_id.
        let info = self.password_info().await?;
        // check_password unwraps these; a malformed answer must not panic the request.
        if info.current_algo.is_none() || info.srp_b.is_none() || info.srp_id.is_none() {
            return Err(TgError::Upstream(
                "telegram sent no SRP parameters for the cloud password".into(),
            ));
        }
        let checked = self
            .conn
            .client
            .check_password(PasswordToken::new(info), password.as_bytes())
            .await;
        drop(password);
        match checked {
            Ok(user) => self.logged_in(user.raw),
            Err(SignInError::InvalidPassword(_)) => Err(TgError::WrongPassword),
            Err(SignInError::Other(err)) => Err(login_error(err)),
            Err(other) => Err(TgError::Upstream(other.to_string())),
        }
    }
}

impl QrAttempt {
    async fn handle(&mut self, token: tl::enums::auth::LoginToken) -> Result<LoginState, TgError> {
        match token {
            tl::enums::auth::LoginToken::Token(t) => {
                let url = qr_url(&t.token);
                self.current = Some((url.clone(), i64::from(t.expires)));
                Ok(LoginState::WaitingForScan { qr_url: url })
            }
            tl::enums::auth::LoginToken::MigrateTo(m) => {
                // The account lives in another DC: make it home and finish there.
                let old_dc = self
                    .conn
                    .store
                    .home_dc_id()
                    .map_err(|e| TgError::Upstream(e.to_string()))?;
                self.conn
                    .store
                    .set_home_dc_id(m.dc_id)
                    .await
                    .map_err(|e| TgError::Upstream(e.to_string()))?;
                tracing::info!(from = old_dc, to = m.dc_id, "qr login migrates dc");
                let imported = self
                    .conn
                    .client
                    .invoke_in_dc(
                        m.dc_id,
                        &tl::functions::auth::ImportLoginToken { token: m.token },
                    )
                    .await;
                match imported {
                    Ok(tl::enums::auth::LoginToken::Success(s)) => self.finish(s.authorization),
                    Ok(_) => Err(TgError::Upstream(
                        "unexpected answer to importLoginToken".into(),
                    )),
                    Err(err) if err.is("SESSION_PASSWORD_NEEDED") => self.need_password().await,
                    Err(err) => Err(login_error(err)),
                }
            }
            tl::enums::auth::LoginToken::Success(s) => self.finish(s.authorization),
        }
    }

    fn finish(
        &mut self,
        authorization: tl::enums::auth::Authorization,
    ) -> Result<LoginState, TgError> {
        let tl::enums::auth::Authorization::Authorization(auth) = authorization else {
            return Err(TgError::Upstream(
                "this Telegram account needs to be signed up first".into(),
            ));
        };
        self.logged_in(auth.user).map(LoginState::LoggedIn)
    }

    fn logged_in(&self, user: tl::enums::User) -> Result<LoggedIn, TgError> {
        let account = account_from(user)?;
        let telegram_session = self.conn.store.snapshot()?;
        Ok(LoggedIn {
            account,
            telegram_session,
        })
    }

    /// The QR was accepted but the Account has a Cloud password: remember its hint.
    async fn need_password(&mut self) -> Result<LoginState, TgError> {
        let hint = self.password_info().await?.hint;
        tracing::info!("qr login needs the cloud password");
        self.password_hint = Some(hint.clone());
        Ok(LoginState::PasswordNeeded { hint })
    }

    /// `account.getPassword`: the hint and a fresh SRP challenge.
    async fn password_info(&self) -> Result<tl::types::account::Password, TgError> {
        let tl::enums::account::Password::Password(info) = self
            .conn
            .client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .map_err(login_error)?;
        Ok(info)
    }
}

// ── Client pool ─────────────────────────────────────────────────────────────

/// A Live client: a connected grammers client for one Grant, holding the opened
/// Grant (with its Session key) so an updated Telegram session can be re-sealed.
pub struct LiveClient {
    client: Client,
    store: Arc<SessionStore>,
    grant: OpenedGrant,
    /// Telegram said this session is gone (logged out / revoked from the app).
    dead: std::sync::atomic::AtomicBool,
}

impl Live for LiveClient {
    fn snapshot(&self) -> Result<zeroize::Zeroizing<Vec<u8>>, TgError> {
        self.store.snapshot()
    }

    fn shutdown(&self) {
        self.client.disconnect();
    }
}

impl LiveClient {
    fn classify(&self, err: InvocationError) -> TgError {
        if is_dead_session(&err) {
            self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
            TgError::BadSession(err.to_string())
        } else {
            upstream(err)
        }
    }

    fn is_dead(&self) -> bool {
        self.dead.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Telegram for LiveClient {
    async fn whoami(&self) -> Result<Account, TgError> {
        let me = self
            .client
            .invoke(&tl::functions::users::GetUsers {
                id: vec![tl::enums::InputUser::UserSelf],
            })
            .await
            .map_err(|err| self.classify(err))?;
        account_from(
            me.into_iter()
                .next()
                .ok_or_else(|| TgError::Upstream("no self user".into()))?,
        )
    }
}

pub struct GrammersConnector {
    credentials: AppCredentials,
    vault: Arc<Vault>,
    pool: Pool<LiveClient>,
}

impl GrammersConnector {
    pub fn new(credentials: AppCredentials, vault: Arc<Vault>, idle_ttl: Duration) -> Arc<Self> {
        let connector = Arc::new(Self {
            credentials,
            vault,
            pool: Pool::new(idle_ttl),
        });
        tokio::spawn(sweep(Arc::downgrade(&connector)));
        connector
    }

    async fn open(&self, grant: OpenedGrant) -> Result<LiveClient, TgError> {
        let store = SessionStore::from_snapshot(&grant.telegram_session)?;
        let Connected { client, store, .. } = connect(&self.credentials, store);
        // Prove the session still works before handing it out.
        let live = LiveClient {
            client,
            store,
            grant,
            dead: Default::default(),
        };
        match live.whoami().await {
            Ok(_) => Ok(live),
            Err(err) => {
                live.shutdown();
                Err(err)
            }
        }
    }
}

#[async_trait::async_trait]
impl Connector for GrammersConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let key = grant.grant_id.as_str().to_string();
        let snapshot = grant.telegram_session.clone();
        let live = self
            .pool
            .get_or_open(&key, &snapshot, || self.open(grant))
            .await?;
        if live.is_dead() {
            // Revoked in Telegram since it was opened: drop it so the client re-authorizes.
            self.pool.remove(&key);
            return Err(TgError::BadSession("telegram session was revoked".into()));
        }
        Ok(live)
    }
}

/// Every minute: evict idle Live clients and re-seal sessions that changed.
async fn sweep(connector: Weak<GrammersConnector>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    loop {
        every.tick().await;
        let Some(connector) = connector.upgrade() else {
            return;
        };
        for (_, live, snapshot) in connector.pool.evict_idle() {
            if let Err(err) = connector.vault.save_session(&live.grant, &snapshot).await {
                tracing::warn!(grant_id = %live.grant.grant_id, error = %err, "re-sealing telegram session failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_url_is_tg_login_with_base64url_token() {
        assert_eq!(qr_url(&[0xfb, 0xff, 0x01]), "tg://login?token=-_8B");
    }

    #[test]
    fn login_token_update_is_detected_in_every_container() {
        let short = tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
            update: tl::enums::Update::LoginToken,
            date: 0,
        });
        assert!(has_login_token_update(&UpdatesLike::Updates(short)));
        let full = tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![tl::enums::Update::LoginToken],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        });
        assert!(has_login_token_update(&UpdatesLike::Updates(full)));
        assert!(!has_login_token_update(&UpdatesLike::Updates(
            tl::enums::Updates::TooLong
        )));
    }

    #[test]
    fn flood_wait_is_surfaced_with_its_seconds() {
        let rpc = |code, message: &str| {
            InvocationError::Rpc(
                tl::types::RpcError {
                    error_code: code,
                    error_message: message.into(),
                }
                .into(),
            )
        };
        assert!(matches!(
            login_error(rpc(420, "FLOOD_WAIT_31")),
            TgError::FloodWait { seconds: 31 }
        ));
        assert!(matches!(
            login_error(rpc(400, "PASSWORD_HASH_INVALID")),
            TgError::Upstream(_)
        ));
    }

    #[test]
    fn credentials_debug_hides_api_hash() {
        let creds = AppCredentials {
            api_id: 1,
            api_hash: "0123456789abcdef".into(),
        };
        assert!(!format!("{creds:?}").contains("0123456789abcdef"));
    }
}

/// Talks to real Telegram over raw TCP (MTProto); run with `cargo test -- --ignored live_`
/// from a machine that can reach Telegram's DCs. Needs no credentials: auth-key
/// generation happens before api_id is checked.
#[cfg(test)]
mod live_tests {
    use grammers_session::Session;

    use super::*;

    #[tokio::test]
    #[ignore = "needs network access to Telegram"]
    async fn live_handshake_stores_an_auth_key_in_our_session() {
        let creds = AppCredentials {
            api_id: 0,
            api_hash: "invalid".into(),
        };
        let conn = connect(&creds, SessionStore::new());
        let home = conn.store.home_dc_id().unwrap();
        assert!(
            conn.store
                .dc_option(home)
                .unwrap()
                .unwrap()
                .auth_key
                .is_none()
        );

        let _ = tracing_subscriber::fmt()
            .with_env_filter("grammers=debug")
            .with_test_writer()
            .try_init();
        let err = tokio::time::timeout(
            Duration::from_secs(30),
            conn.client.invoke(&tl::functions::help::GetNearestDc {}),
        )
        .await
        .expect("Telegram answers within 30 s")
        .expect_err("api_id 0 must be rejected");
        assert!(err.is("API_ID_INVALID"), "unexpected error: {err}");

        // The MTProto handshake ran through SessionStore and its key is in the snapshot.
        let restored = SessionStore::from_snapshot(&conn.store.snapshot().unwrap()).unwrap();
        assert!(
            restored
                .dc_option(home)
                .unwrap()
                .unwrap()
                .auth_key
                .is_some()
        );
        conn.client.disconnect();
    }
}
