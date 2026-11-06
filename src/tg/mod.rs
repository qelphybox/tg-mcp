//! The `tg` boundary: everything the server needs from Telegram, behind traits.
//!
//! - [`Login`] / [`LoginAttempt`]: the Telegram part of the Login flow (QR login, then
//!   the Cloud password if the Account has one).
//! - [`Connector`]: turns an opened Grant into a Live client.
//! - [`Telegram`]: read-only calls on one Account.
//!
//! Tests drive the whole server through the fakes in [`fake`]; [`grammers`] is the
//! real Telegram (QR login + Client pool).

pub mod chats;
pub mod fake;
pub mod grammers;
pub mod messages;
mod pool;
pub mod sanitise;
mod session;

pub use fake::{FakeConnector, FakeLogin, FakePassword, FakeTelegram};

use std::{fmt, sync::Arc, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::sessions::OpenedGrant;

/// The Telegram Account a Grant reads on behalf of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    /// Telegram user id.
    pub id: i64,
    pub first_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Public @username, without the `@`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Account {
    pub fn display_name(&self) -> String {
        match &self.last_name {
            Some(last) => format!("{} {last}", self.first_name),
            None => self.first_name.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    /// Telegram is unreachable or answered with something we can't use.
    #[error("telegram request failed: {0}")]
    Upstream(String),
    /// The stored Telegram session can't be used (corrupt, or logged out elsewhere).
    #[error("telegram session is not usable: {0}")]
    BadSession(String),
    /// The Cloud password didn't match.
    #[error("the cloud password is wrong")]
    WrongPassword,
    /// Telegram's `FLOOD_WAIT`: nothing may be retried before `seconds` pass.
    #[error("telegram asks to wait {seconds} s")]
    FloodWait { seconds: u32 },
    /// Our own per-Account request budget is used up; retry after `seconds`.
    #[error("this account's request budget is used up; retry in {seconds} s")]
    RateLimited { seconds: u32 },
    /// The caller passed something malformed (bad chat id, foreign cursor).
    #[error("{0}")]
    InvalidInput(String),
}

/// Read-only access to one Account.
#[async_trait::async_trait]
pub trait Telegram: Send + Sync + 'static {
    async fn whoami(&self) -> Result<Account, TgError>;

    /// One page of the Account's Chats, most recent first. `cursor` is a
    /// `next_cursor` from a previous page (it also decides `archived`).
    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<chats::ChatPage, TgError>;

    /// Messages of one Chat, newest first, within `query`'s bounds.
    async fn get_messages(
        &self,
        chat: chats::ChatRef,
        query: messages::HistoryQuery,
    ) -> Result<messages::MessagePage, TgError>;

    /// Details of one Chat: title, type, @username, description/bio, member count.
    async fn get_chat_info(&self, chat: chats::ChatRef) -> Result<chats::ChatInfo, TgError>;
}

/// Turns a Grant opened with a valid Token into a Live client for its Account.
/// Takes the Grant: a new Live client keeps it (and its Session key) for as long
/// as it lives, to re-seal the Telegram session if it changes.
#[async_trait::async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError>;

    /// End the Grant's Telegram session (`auth.logOut`, so it leaves the Account's
    /// Active sessions) and drop its Live client. A session Telegram already
    /// considers dead counts as logged out.
    async fn log_out(&self, grant: OpenedGrant) -> Result<(), TgError>;

    /// Graceful shutdown, once the HTTP server has stopped: close every Live client
    /// and re-seal each Telegram session that changed while it was live. Failures
    /// are logged (Grant ids only), not returned: the process is exiting anyway.
    /// Call it through [`shut_down`], which bounds how long it may take.
    async fn shutdown(&self);
}

/// How long graceful shutdown waits for [`Connector::shutdown`]. Docker gives a
/// container 10 s between SIGTERM and SIGKILL unless told otherwise (Kamal's
/// `stop_timeout`); re-sealing normally takes milliseconds.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Run [`Connector::shutdown`], giving up after `within` so a hung save can't
/// block exit. Returns whether it finished in time.
pub async fn shut_down(connector: &dyn Connector, within: Duration) -> bool {
    let finished = tokio::time::timeout(within, connector.shutdown())
        .await
        .is_ok();
    if !finished {
        tracing::warn!(
            timeout_s = within.as_secs(),
            "flushing the client pool timed out; unsaved telegram session changes are lost"
        );
    }
    finished
}

/// Starts the Telegram part of a Login flow.
#[async_trait::async_trait]
pub trait Login: Send + Sync + 'static {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError>;
}

/// One in-progress Telegram login, driven by the login page polling it.
#[async_trait::async_trait]
pub trait LoginAttempt: Send + 'static {
    /// Advance as far as possible without user input and report where we are.
    async fn poll(&mut self) -> Result<LoginState, TgError>;

    /// Check the Cloud password once [`LoginState::PasswordNeeded`] was reported.
    /// Takes the password so it is wiped as soon as the check is over; it is never
    /// stored or logged. A wrong one is [`TgError::WrongPassword`] and may be retried.
    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError>;
}

pub enum LoginState {
    /// Show this `tg://login?token=…` URL as a QR code and keep polling.
    WaitingForScan { qr_url: String },
    /// Scanned, but the Account has a Cloud password: ask for it (with its hint).
    PasswordNeeded { hint: Option<String> },
    /// Done: the Account and its fresh Telegram session.
    LoggedIn(LoggedIn),
}

/// The result of a finished login. The session bytes are wiped on drop.
pub struct LoggedIn {
    pub account: Account,
    pub telegram_session: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for LoggedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggedIn")
            .field("account_id", &self.account.id)
            .field(
                "telegram_session",
                &format_args!("<{} bytes>", self.telegram_session.len()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector whose shutdown never finishes (e.g. the database hangs).
    struct Hung;

    #[async_trait::async_trait]
    impl Connector for Hung {
        async fn connect(&self, _: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
            unreachable!()
        }
        async fn log_out(&self, _: OpenedGrant) -> Result<(), TgError> {
            unreachable!()
        }
        async fn shutdown(&self) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_shutdown_is_cut_off() {
        let started = tokio::time::Instant::now();
        let finished =
            tokio::time::timeout(2 * SHUTDOWN_TIMEOUT, shut_down(&Hung, SHUTDOWN_TIMEOUT))
                .await
                .expect("shut_down gives up on its own");
        assert!(!finished);
        assert_eq!(started.elapsed(), SHUTDOWN_TIMEOUT);
    }

    #[tokio::test]
    async fn a_finished_shutdown_is_reported() {
        let connector = FakeConnector::default();
        assert!(shut_down(&connector, SHUTDOWN_TIMEOUT).await);
        assert_eq!(connector.shutdowns(), 1);
    }
}
