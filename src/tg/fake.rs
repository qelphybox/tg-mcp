//! In-memory stand-ins for Telegram, used by tests and by `TG_MCP_FAKE_TELEGRAM=1`.
//!
//! A fake "Telegram session" is just the Account as JSON, so a Grant created by a
//! fake login connects back to exactly that Account.

use std::sync::Arc;

use zeroize::Zeroizing;

use super::{Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError};
use crate::sessions::OpenedGrant;

#[derive(Debug, Clone)]
pub struct FakeTelegram {
    account: Account,
}

impl FakeTelegram {
    pub fn new(account: Account) -> Self {
        Self { account }
    }
}

#[async_trait::async_trait]
impl Telegram for FakeTelegram {
    async fn whoami(&self) -> Result<Account, TgError> {
        Ok(self.account.clone())
    }
}

pub fn default_account() -> Account {
    Account {
        id: 777_000,
        first_name: "Fake".into(),
        last_name: Some("Account".into()),
        username: Some("fake_account".into()),
    }
}

/// Logs in as a fixed Account: shows a fake QR once, then succeeds.
#[derive(Debug, Clone)]
pub struct FakeLogin {
    account: Account,
}

impl FakeLogin {
    pub fn new(account: Account) -> Self {
        Self { account }
    }
}

impl Default for FakeLogin {
    fn default() -> Self {
        Self::new(default_account())
    }
}

#[async_trait::async_trait]
impl Login for FakeLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        Ok(Box::new(FakeAttempt {
            account: self.account.clone(),
            polled: false,
        }))
    }
}

struct FakeAttempt {
    account: Account,
    polled: bool,
}

#[async_trait::async_trait]
impl LoginAttempt for FakeAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if !self.polled {
            self.polled = true;
            return Ok(LoginState::WaitingForScan {
                qr_url: "tg://login?token=fake".into(),
            });
        }
        let session = serde_json::to_vec(&self.account).expect("Account serializes");
        Ok(LoginState::LoggedIn(LoggedIn {
            account: self.account.clone(),
            telegram_session: Zeroizing::new(session),
        }))
    }
}

/// Connects a Grant to the Account recorded in its fake Telegram session.
#[derive(Debug, Clone, Default)]
pub struct FakeConnector;

#[async_trait::async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let account: Account = serde_json::from_slice(&grant.telegram_session)
            .map_err(|e| TgError::BadSession(e.to_string()))?;
        Ok(Arc::new(FakeTelegram::new(account)))
    }
}
