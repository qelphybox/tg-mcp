use super::{Account, Telegram, TgError};

/// In-memory stand-in for a Live client. Used by tests and by `TG_MCP_FAKE_TELEGRAM=1`.
#[derive(Debug, Clone)]
pub struct FakeTelegram {
    account: Account,
}

impl FakeTelegram {
    pub fn new(account: Account) -> Self {
        Self { account }
    }
}

impl Default for FakeTelegram {
    fn default() -> Self {
        Self::new(Account {
            id: 777_000,
            first_name: "Fake".into(),
            last_name: Some("Account".into()),
            username: Some("fake_account".into()),
        })
    }
}

#[async_trait::async_trait]
impl Telegram for FakeTelegram {
    async fn whoami(&self) -> Result<Account, TgError> {
        Ok(self.account.clone())
    }
}
