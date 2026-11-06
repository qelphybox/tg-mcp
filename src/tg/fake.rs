//! In-memory stand-ins for Telegram, used by tests and by `TG_MCP_FAKE_TELEGRAM=1`.
//!
//! A fake "Telegram session" is a [`FakeData`] as JSON (the Account and its Chats),
//! so a Grant created by a fake login connects back to exactly that data.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    chats::{ChatKind, ChatPage, ChatSummary, MessagePreview},
};
use crate::sessions::OpenedGrant;

/// Everything a fake Account has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeData {
    pub account: Account,
    #[serde(default)]
    pub chats: Vec<ChatSummary>,
}

impl From<Account> for FakeData {
    fn from(account: Account) -> Self {
        Self::new(account)
    }
}

impl FakeData {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            chats: Vec::new(),
        }
    }

    pub fn with_chats(mut self, chats: Vec<ChatSummary>) -> Self {
        self.chats = chats;
        self
    }
}

#[derive(Debug, Clone)]
pub struct FakeTelegram {
    data: FakeData,
}

impl FakeTelegram {
    pub fn new(data: FakeData) -> Self {
        Self { data }
    }
}

const CURSOR_PREFIX: &str = "fake:";

#[async_trait::async_trait]
impl Telegram for FakeTelegram {
    async fn whoami(&self) -> Result<Account, TgError> {
        Ok(self.data.account.clone())
    }

    /// Archived chats don't exist in the fake. Cursors are `fake:<offset>`.
    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<ChatPage, TgError> {
        let start = match cursor {
            None => 0,
            Some(c) => c
                .strip_prefix(CURSOR_PREFIX)
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| {
                    TgError::InvalidInput("cursor is not one this server returned".into())
                })?,
        };
        if archived {
            return Ok(ChatPage {
                chats: Vec::new(),
                next_cursor: None,
            });
        }
        let chats: Vec<ChatSummary> = self
            .data
            .chats
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        let end = start + chats.len();
        Ok(ChatPage {
            next_cursor: (end < self.data.chats.len()).then(|| format!("{CURSOR_PREFIX}{end}")),
            chats,
        })
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

/// A few Chats, so the fake server has something to show.
pub fn sample_chats() -> Vec<ChatSummary> {
    let chat = |id: &str, title: &str, kind, unread, text: &str| ChatSummary {
        id: id.into(),
        title: title.into(),
        kind,
        username: None,
        unread_count: unread,
        pinned: false,
        last_message: Some(MessagePreview {
            date: "2026-11-06T12:00:00Z".into(),
            text: text.into(),
        }),
    };
    vec![
        chat(
            "user:101",
            "Alice",
            ChatKind::Private,
            2,
            "see you tomorrow",
        ),
        chat(
            "channel:202",
            "Rust Weekly",
            ChatKind::Channel,
            14,
            "[photo]",
        ),
        chat(
            "channel:303",
            "Team",
            ChatKind::Supergroup,
            0,
            "deploy is green",
        ),
        chat("chat:404", "Family", ChatKind::Group, 5, "[voice]"),
    ]
}

/// Logs in as a fixed Account: shows a fake QR once, then succeeds, after the
/// Cloud password if [`FakeLogin::with_password`] set one.
#[derive(Debug, Clone)]
pub struct FakeLogin {
    data: FakeData,
    password: Option<FakePassword>,
}

/// A Cloud password (2FA) on a fake Account.
#[derive(Clone)]
pub struct FakePassword {
    pub password: String,
    pub hint: Option<String>,
    /// Like Telegram's limit on wrong attempts: the attempt that follows this many
    /// wrong passwords is answered `FLOOD_WAIT_30` (once) instead of being checked.
    pub flood_after: Option<u32>,
}

/// Seconds of the fake `FLOOD_WAIT`.
pub const FAKE_FLOOD_WAIT: u32 = 30;

impl fmt::Debug for FakePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakePassword")
            .field("hint", &self.hint)
            .field("flood_after", &self.flood_after)
            .finish_non_exhaustive()
    }
}

impl FakeLogin {
    pub fn new(data: impl Into<FakeData>) -> Self {
        Self {
            data: data.into(),
            password: None,
        }
    }

    /// An Account with two-step verification: asks for `password` after the QR.
    pub fn with_password(data: impl Into<FakeData>, password: FakePassword) -> Self {
        Self {
            data: data.into(),
            password: Some(password),
        }
    }
}

impl Default for FakeLogin {
    fn default() -> Self {
        Self::new(FakeData::new(default_account()).with_chats(sample_chats()))
    }
}

#[async_trait::async_trait]
impl Login for FakeLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        Ok(Box::new(FakeAttempt {
            data: self.data.clone(),
            polled: false,
            password: self.password.clone(),
            wrong: 0,
            flooded: false,
        }))
    }
}

struct FakeAttempt {
    data: FakeData,
    polled: bool,
    /// The Cloud password still to be entered; `None` once it was (or if there is none).
    password: Option<FakePassword>,
    wrong: u32,
    flooded: bool,
}

impl FakeAttempt {
    fn logged_in(&self) -> LoggedIn {
        let session = serde_json::to_vec(&self.data).expect("FakeData serializes");
        LoggedIn {
            account: self.data.account.clone(),
            telegram_session: Zeroizing::new(session),
        }
    }
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
        if let Some(password) = &self.password {
            return Ok(LoginState::PasswordNeeded {
                hint: password.hint.clone(),
            });
        }
        Ok(LoginState::LoggedIn(self.logged_in()))
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        let Some(expected) = self.password.as_ref().filter(|_| self.polled) else {
            return Err(TgError::Upstream("not waiting for a cloud password".into()));
        };
        if expected.flood_after == Some(self.wrong) && !self.flooded {
            self.flooded = true;
            return Err(TgError::FloodWait {
                seconds: FAKE_FLOOD_WAIT,
            });
        }
        if *password != expected.password {
            self.wrong += 1;
            return Err(TgError::WrongPassword);
        }
        self.password = None;
        Ok(self.logged_in())
    }
}

/// Connects a Grant to the data recorded in its fake Telegram session.
#[derive(Debug, Clone, Default)]
pub struct FakeConnector;

#[async_trait::async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let data: FakeData = serde_json::from_slice(&grant.telegram_session)
            .map_err(|e| TgError::BadSession(e.to_string()))?;
        Ok(Arc::new(FakeTelegram::new(data)))
    }
}
