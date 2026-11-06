//! The `tg` boundary: everything the server needs from Telegram, behind one trait.
//!
//! Tests drive the whole server through [`FakeTelegram`]; the grammers-backed
//! implementation arrives with real QR login.

mod fake;

pub use fake::FakeTelegram;

use schemars::JsonSchema;
use serde::Serialize;

/// The Telegram Account a Grant reads on behalf of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Account {
    /// Telegram user id.
    pub id: i64,
    pub first_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Public @username, without the `@`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    /// Telegram is unreachable or answered with something we can't use.
    #[error("telegram request failed: {0}")]
    Upstream(String),
}

/// Read-only access to one Account.
#[async_trait::async_trait]
pub trait Telegram: Send + Sync + 'static {
    async fn whoami(&self) -> Result<Account, TgError>;
}
