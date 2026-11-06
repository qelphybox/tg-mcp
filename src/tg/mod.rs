//! The `tg` boundary: everything the server needs from Telegram, behind traits.
//!
//! - [`Login`] / [`LoginAttempt`]: the Telegram part of the Login flow (QR, later 2FA).
//! - [`Connector`]: turns an opened Grant into a Live client.
//! - [`Telegram`]: read-only calls on one Account.
//!
//! Tests drive the whole server through the fakes in [`fake`]; [`grammers`] is the
//! real Telegram (QR login + Client pool).

pub mod fake;
pub mod grammers;
mod pool;
mod session;

pub use fake::{FakeConnector, FakeLogin, FakeTelegram};

use std::{fmt, sync::Arc};

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
    /// The Account has a cloud password; the Login flow can't finish without it yet.
    #[error("two-step verification is enabled on this account")]
    TwoFactorRequired,
}

/// Read-only access to one Account.
#[async_trait::async_trait]
pub trait Telegram: Send + Sync + 'static {
    async fn whoami(&self) -> Result<Account, TgError>;
}

/// Turns a Grant opened with a valid Token into a Live client for its Account.
/// Takes the Grant: a new Live client keeps it (and its Session key) for as long
/// as it lives, to re-seal the Telegram session if it changes.
#[async_trait::async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError>;
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
}

pub enum LoginState {
    /// Show this `tg://login?token=…` URL as a QR code and keep polling.
    WaitingForScan { qr_url: String },
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
