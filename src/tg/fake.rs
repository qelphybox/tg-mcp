//! In-memory stand-ins for Telegram, used by tests and by `TG_MCP_FAKE_TELEGRAM=1`.
//!
//! A fake "Telegram session" is a [`FakeData`] as JSON (the Account and its Chats),
//! so a Grant created by a fake login connects back to exactly that data.

use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    chats::{
        ChatId, ChatInfo, ChatKind, ChatPage, ChatRef, ChatSummary, MessagePreview, RawPeer,
        chat_info,
    },
    messages::{
        HistoryAnswer, HistoryQuery, HistoryRequest, MessagePage, RawHistoryMessage, read_history,
    },
};
use crate::{limits::MethodClass, sessions::OpenedGrant};

/// Everything a fake Account has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeData {
    pub account: Account,
    /// The Account's Chats. Their ids may be written without access hash
    /// (`user:101`); the fake hands them out with [`fake_access_hash`].
    #[serde(default)]
    pub chats: Vec<ChatSummary>,
    /// Each Chat's history, in any order.
    #[serde(default)]
    pub messages: std::collections::HashMap<ChatId, Vec<RawHistoryMessage>>,
    /// Descriptions and member counts, for get_chat_info.
    #[serde(default)]
    pub details: std::collections::HashMap<ChatId, FakeDetails>,
    /// When set, every call answers like Telegram's FLOOD_WAIT_<seconds>.
    #[serde(default)]
    pub flood_wait: Option<u32>,
    /// How getHistory answers a slice that reaches past the newest message.
    #[serde(default)]
    pub past_newest: PastNewest,
}

/// What Telegram does when `add_offset` puts a getHistory slice's start above
/// the newest message. Its docs don't say, so the server must cope with both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PastNewest {
    /// The slice keeps its older end and starts at the newest message: fewer
    /// than `limit` messages.
    #[default]
    Shorten,
    /// The slice starts at the newest message and still holds `limit`, so it
    /// reaches further down than asked.
    ShiftOlder,
}

/// Telegram's largest getHistory `limit`; larger ones are cut to it.
const HISTORY_MAX_LIMIT: i64 = 100;

/// `messages.getHistory` as Telegram answers it (core.telegram.org/api/offsets),
/// on a history given in any order: newest first, slice at
/// `position(offset_id) + add_offset` (`position` counts the messages down to
/// `offset_id` inclusive, whether or not that id exists; `offset_id` wins over
/// `offset_date`), keep `limit`, then drop ids outside `min_id`/`max_id`.
pub fn get_history(
    history: &[RawHistoryMessage],
    request: &HistoryRequest,
    past_newest: PastNewest,
) -> Vec<RawHistoryMessage> {
    let mut list = history.to_vec();
    list.sort_by_key(|m| std::cmp::Reverse(m.id));
    let position = if request.offset_id != 0 {
        list.iter()
            .take_while(|m| m.id >= request.offset_id)
            .count()
    } else if request.offset_date != 0 {
        list.iter()
            .take_while(|m| m.date >= request.offset_date)
            .count()
    } else {
        0
    };
    let limit = i64::from(request.limit).clamp(0, HISTORY_MAX_LIMIT);
    let mut start = position as i64 + i64::from(request.add_offset);
    let mut take = limit;
    if start < 0 {
        if past_newest == PastNewest::Shorten {
            take = (take + start).max(0);
        }
        start = 0;
    }
    list.into_iter()
        .skip(start as usize)
        .take(take as usize)
        .filter(|m| request.max_id == 0 || m.id < request.max_id)
        .filter(|m| request.min_id == 0 || m.id > request.min_id)
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeDetails {
    pub description: Option<String>,
    pub member_count: Option<i32>,
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
            messages: Default::default(),
            details: Default::default(),
            flood_wait: None,
            past_newest: PastNewest::default(),
        }
    }

    pub fn with_past_newest(mut self, past_newest: PastNewest) -> Self {
        self.past_newest = past_newest;
        self
    }

    pub fn with_details(mut self, chat: ChatId, details: FakeDetails) -> Self {
        self.details.insert(chat, details);
        self
    }

    pub fn with_messages(mut self, chat: ChatId, messages: Vec<RawHistoryMessage>) -> Self {
        self.messages.insert(chat, messages);
        self
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

/// The access hash the fake Telegram gives a user or channel: fixed per id (and
/// often negative, like real ones), so tests can predict the ids it issues.
/// Basic groups have none.
pub fn fake_access_hash(chat: ChatId) -> Option<i64> {
    match chat {
        ChatId::User(id) | ChatId::Channel(id) => {
            Some((id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64)
        }
        ChatId::Chat(_) => None,
    }
}

/// The id the fake hands out for `chat`, access hash included.
pub fn fake_issued_id(chat: ChatId) -> String {
    ChatRef::new(chat, fake_access_hash(chat)).to_string()
}

#[async_trait::async_trait]
impl Telegram for FakeTelegram {
    async fn whoami(&self) -> Result<Account, TgError> {
        self.flood()?;
        Ok(self.data.account.clone())
    }

    /// Archived chats don't exist in the fake. Cursors are `fake:<offset>`.
    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<ChatPage, TgError> {
        self.flood()?;
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
            .map(|c| ChatSummary {
                id: c
                    .id
                    .parse::<ChatRef>()
                    .map_or_else(|_| c.id.clone(), |id| fake_issued_id(id.chat())),
                ..c.clone()
            })
            .collect();
        let end = start + chats.len();
        Ok(ChatPage {
            next_cursor: (end < self.data.chats.len()).then(|| format!("{CURSOR_PREFIX}{end}")),
            chats,
        })
    }

    /// The same getHistory calls as the Live client, answered by [`get_history`].
    async fn get_messages(&self, id: ChatRef, query: HistoryQuery) -> Result<MessagePage, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        let history = self.data.messages.get(&chat).map_or(&[][..], Vec::as_slice);
        let peers = self.peers();
        read_history(id, &query, |request| {
            let messages = get_history(history, &request, self.data.past_newest);
            std::future::ready(Ok(HistoryAnswer {
                returned: messages.len(),
                messages,
                peers: peers.clone(),
            }))
        })
        .await
    }

    async fn get_chat_info(&self, id: ChatRef) -> Result<ChatInfo, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        let peer = self
            .peers()
            .into_iter()
            .find(|p| p.id == chat)
            .ok_or_else(|| {
                TgError::InvalidInput(format!(
                    "{chat} is not one of your chats; chat ids come from list_chats"
                ))
            })?;
        let details = self.data.details.get(&chat).cloned().unwrap_or_default();
        Ok(chat_info(&peer, details.description, details.member_count))
    }
}

impl FakeTelegram {
    fn flood(&self) -> Result<(), TgError> {
        match self.data.flood_wait {
            Some(seconds) => Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Default,
            }),
            None => Ok(()),
        }
    }

    /// Like Telegram: an id with a wrong access hash is `CHANNEL_INVALID` /
    /// `PEER_ID_INVALID`, which the Live client reports this way.
    fn check_hash(&self, chat: ChatRef) -> Result<(), TgError> {
        match chat.access_hash() {
            Some(hash) if Some(hash) != fake_access_hash(chat.chat()) => Err(
                TgError::InvalidInput(format!("{} can't be read by this account", chat.chat())),
            ),
            _ => Ok(()),
        }
    }

    /// The fake knows a Chat if it lists it or has history for it.
    fn knows(&self, chat: ChatId) -> bool {
        self.data.messages.contains_key(&chat)
            || self
                .data
                .chats
                .iter()
                .any(|c| c.id.parse::<ChatRef>().is_ok_and(|id| id.chat() == chat))
    }

    fn peers(&self) -> Vec<RawPeer> {
        let me = &self.data.account;
        self.data
            .chats
            .iter()
            .filter_map(|c| {
                let id = c.id.parse::<ChatRef>().ok()?.chat();
                Some(RawPeer {
                    id,
                    title: c.title.clone(),
                    kind: c.kind,
                    username: c.username.clone(),
                    access_hash: fake_access_hash(id),
                    is_self: c.is_self,
                })
            })
            .chain(std::iter::once(RawPeer {
                id: ChatId::User(me.id),
                title: me.display_name(),
                kind: ChatKind::Private,
                username: me.username.clone(),
                access_hash: fake_access_hash(ChatId::User(me.id)),
                is_self: true,
            }))
            .collect()
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
        is_self: false,
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
                class: MethodClass::Default,
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

/// Connects a Grant to the data recorded in its fake Telegram session. Clones
/// share one fake "Telegram side": which Accounts' sessions are dead, which were
/// logged out, and whether Telegram is reachable.
#[derive(Debug, Clone, Default)]
pub struct FakeConnector {
    telegram: Arc<FakeTelegramSide>,
}

#[derive(Debug, Default)]
struct FakeTelegramSide {
    /// Accounts whose session Telegram considers ended.
    dead: Mutex<HashSet<i64>>,
    /// Accounts `auth.logOut` was called for, in order.
    logged_out: Mutex<Vec<i64>>,
    unreachable: std::sync::atomic::AtomicBool,
    /// How many times [`Connector::shutdown`] was called.
    shutdowns: std::sync::atomic::AtomicUsize,
}

impl FakeConnector {
    /// The user ended the session in Telegram (Settings → Devices → Terminate).
    pub fn end_session_in_telegram(&self, account_id: i64) {
        self.telegram.dead.lock().unwrap().insert(account_id);
    }

    /// Accounts whose session was logged out through [`Connector::log_out`].
    pub fn logged_out(&self) -> Vec<i64> {
        self.telegram.logged_out.lock().unwrap().clone()
    }

    /// How many times the server shut this connector down.
    pub fn shutdowns(&self) -> usize {
        self.telegram
            .shutdowns
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Make every call to Telegram fail as if it were down.
    pub fn set_unreachable(&self, unreachable: bool) {
        self.telegram
            .unreachable
            .store(unreachable, std::sync::atomic::Ordering::SeqCst);
    }

    fn data(grant: &OpenedGrant) -> Result<FakeData, TgError> {
        serde_json::from_slice(&grant.telegram_session)
            .map_err(|e| TgError::BadSession(e.to_string()))
    }

    fn reachable(&self) -> Result<(), TgError> {
        if self
            .telegram
            .unreachable
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TgError::Upstream("fake telegram is unreachable".into()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let data = Self::data(&grant)?;
        self.reachable()?;
        if self
            .telegram
            .dead
            .lock()
            .unwrap()
            .contains(&data.account.id)
        {
            return Err(TgError::BadSession("fake session was ended".into()));
        }
        Ok(Arc::new(FakeTelegram::new(data)))
    }

    async fn log_out(&self, grant: OpenedGrant) -> Result<(), TgError> {
        let account_id = Self::data(&grant)?.account.id;
        self.reachable()?;
        self.telegram.dead.lock().unwrap().insert(account_id);
        self.telegram.logged_out.lock().unwrap().push(account_id);
        Ok(())
    }

    /// Nothing is held open, so there is nothing to flush; only counted.
    async fn shutdown(&self) {
        self.telegram
            .shutdowns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids with gaps (deleted messages, or a private chat's account-wide ids),
    /// shuffled: the model must not rely on the order it is given.
    fn history() -> Vec<RawHistoryMessage> {
        [3, 50, 1, 9, 40, 10, 2, 30, 20]
            .into_iter()
            .map(|id| RawHistoryMessage {
                id,
                date: 1000 + id,
                from: None,
                out: false,
                text: String::new(),
                reply_to: None,
                forwarded: false,
                edited: false,
            })
            .collect()
    }

    fn ids(request: HistoryRequest, past_newest: PastNewest) -> Vec<i32> {
        get_history(&history(), &request, past_newest)
            .iter()
            .map(|m| m.id)
            .collect()
    }

    fn slice(offset_id: i32, add_offset: i32, limit: i32) -> HistoryRequest {
        HistoryRequest {
            offset_id,
            add_offset,
            limit,
            ..HistoryRequest::default()
        }
    }

    #[test]
    fn the_offsets_page_examples() {
        // core.telegram.org/api/offsets: older than MSGID, newer than MSGID
        // (it counts MSGID itself in), around MSGID.
        let shorten = PastNewest::Shorten;
        assert_eq!(ids(slice(10, 0, 3), shorten), [9, 3, 2]);
        assert_eq!(ids(slice(10, -3, 3), shorten), [30, 20, 10]);
        assert_eq!(ids(slice(10, -2, 4), shorten), [20, 10, 9, 3]);
        // No offset: from the newest.
        assert_eq!(ids(slice(0, 0, 2), shorten), [50, 40]);
    }

    #[test]
    fn a_missing_offset_id_counts_like_the_next_older_one() {
        // 15 doesn't exist: it sits between 20 and 10.
        let shorten = PastNewest::Shorten;
        assert_eq!(ids(slice(15, 0, 2), shorten), [10, 9]);
        assert_eq!(ids(slice(15, -1, 2), shorten), [20, 10]);
    }

    #[test]
    fn a_slice_past_the_newest_message_shortens_or_shifts() {
        // Only 50 is above 40: four at and above 40 reach past the newest.
        assert_eq!(ids(slice(40, -4, 5), PastNewest::Shorten), [50, 40, 30]);
        assert_eq!(
            ids(slice(40, -4, 5), PastNewest::ShiftOlder),
            [50, 40, 30, 20, 10]
        );
        // Past the oldest: just ends.
        assert_eq!(ids(slice(2, 0, 5), PastNewest::Shorten), [1]);
    }

    #[test]
    fn min_and_max_id_apply_after_the_slice() {
        let request = HistoryRequest {
            min_id: 9,
            max_id: 40,
            ..slice(0, 0, 4)
        };
        // The slice is 50, 40, 30, 20; the bounds leave 30 and 20, not 4 messages.
        assert_eq!(ids(request, PastNewest::Shorten), [30, 20]);
    }

    #[test]
    fn offset_date_starts_strictly_before_it_unless_an_id_is_given() {
        let by_date = HistoryRequest {
            offset_date: 1030,
            ..slice(0, 0, 2)
        };
        assert_eq!(ids(by_date, PastNewest::Shorten), [20, 10]);
        let both = HistoryRequest {
            offset_id: 10,
            ..by_date
        };
        assert_eq!(ids(both, PastNewest::Shorten), [9, 3]);
    }

    #[test]
    fn limit_is_cut_to_telegrams_maximum() {
        let long: Vec<RawHistoryMessage> = (1..=150)
            .map(|id| RawHistoryMessage {
                id,
                ..history()[0].clone()
            })
            .collect();
        assert_eq!(
            get_history(&long, &slice(0, 0, 500), PastNewest::Shorten).len(),
            100
        );
    }
}
