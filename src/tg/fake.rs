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
        chat_info, fallback_kind,
    },
    find::{self, FindRequest, FoundAnswer, FoundChats},
    messages::{
        HistoryAnswer, HistoryQuery, HistoryRequest, Media, MessageAnswer, MessagePage,
        MessageRequest, RawHistoryMessage, read_history,
    },
    search::{
        self, ChatType, MediaFilter, QUERY_REQUIRED, RawFoundMessage, RequestScope, SearchAnswer,
        SearchFrom, SearchPage, SearchQuery, SearchRequest, SearchScope,
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
    /// Chats in the Archive; the rest are in the main list.
    #[serde(default)]
    pub archived: Vec<ChatId>,
    /// Contacts the Account has no chat with: not in list_chats, but found by
    /// find_chats, with a profile (get_chat_info) and an empty history.
    #[serde(default)]
    pub contacts: Vec<ChatSummary>,
    /// Public Chats and bots the Account isn't in: only the global part of
    /// `contacts.search` knows them.
    #[serde(default)]
    pub public: Vec<ChatSummary>,
    /// Peers `contacts.search` describes as "min": their access hash only
    /// works within that answer.
    #[serde(default)]
    pub min_peers: Vec<ChatId>,
    /// When set, every reading call (method class Default) answers like
    /// Telegram's FLOOD_WAIT_<seconds>.
    #[serde(default)]
    pub flood_wait: Option<u32>,
    /// When set, every search (method class Search) answers like
    /// FLOOD_WAIT_<seconds>, and nothing else does.
    #[serde(default)]
    pub search_flood_wait: Option<u32>,
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

/// `messages.getMessages` / `channels.getMessages` for one id as Telegram
/// answers it (core.telegram.org/method/messages.getMessages,
/// /method/channels.getMessages, /constructor/messageEmpty), on the
/// Account's histories:
///
/// - in a channel or supergroup (`channels.getMessages`), the id is looked up
///   in that Chat only;
/// - anywhere else (`messages.getMessages`, which takes no peer), ids count
///   across all of the Account's private chats and basic groups, so the
///   message found may be another Chat's: the caller has to check its Chat;
/// - a deleted or unknown id is a `messageEmpty`, i.e. no message.
///
/// Real ids outside channels are unique across those Chats; should a fixture
/// repeat one, it is always the same Chat's (by [`peer_key`]).
pub fn get_message(
    histories: &std::collections::HashMap<ChatId, Vec<RawHistoryMessage>>,
    chat: ChatId,
    request: &MessageRequest,
) -> Vec<RawFoundMessage> {
    let mut chats: Vec<ChatId> = match chat {
        ChatId::Channel(_) => vec![chat],
        _ => histories
            .keys()
            .copied()
            .filter(|c| !matches!(c, ChatId::Channel(_)))
            .collect(),
    };
    chats.sort_by_key(|&c| peer_key(c));
    chats
        .into_iter()
        .find_map(|c| {
            let message = histories.get(&c)?.iter().find(|m| m.id == request.id)?;
            Some(RawFoundMessage {
                chat: c,
                message: message.clone(),
            })
        })
        .into_iter()
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
            archived: Vec::new(),
            contacts: Vec::new(),
            public: Vec::new(),
            min_peers: Vec::new(),
            flood_wait: None,
            search_flood_wait: None,
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

    pub fn with_archived(mut self, chats: Vec<ChatId>) -> Self {
        self.archived = chats;
        self
    }

    pub fn with_contacts(mut self, contacts: Vec<ChatSummary>) -> Self {
        self.contacts = contacts;
        self
    }

    pub fn with_public(mut self, chats: Vec<ChatSummary>) -> Self {
        self.public = chats;
        self
    }

    pub fn with_min_peers(mut self, peers: Vec<ChatId>) -> Self {
        self.min_peers = peers;
        self
    }
}

/// One Chat as the fake's search sees it.
#[derive(Debug, Clone, Copy)]
pub struct FakeSearchChat<'a> {
    pub id: ChatId,
    pub kind: ChatKind,
    pub archived: bool,
    pub messages: &'a [RawHistoryMessage],
}

/// The fake's "rate" of a message: what orders global results and what
/// `next_rate` hands out. Monotonic in the date but not the date, so a server
/// that passes a date where Telegram's rate belongs pages wrong.
pub fn fake_rate(date: i32) -> i32 {
    date - 1_000_000_000
}

/// Telegram's order across Chats for equal rates: by peer, as the Bot API
/// numbers them, then by message id.
fn peer_key(chat: ChatId) -> i64 {
    match chat {
        ChatId::User(id) => id,
        ChatId::Chat(id) => -id,
        ChatId::Channel(id) => -1_000_000_000_000 - id,
    }
}

/// Telegram's largest search `limit`, as for getHistory.
const SEARCH_MAX_LIMIT: i32 = 100;

/// `messages.searchGlobal` / `messages.search` as Telegram answers them, on
/// the account `me`'s Chats (core.telegram.org/method/messages.searchGlobal,
/// /method/messages.search, /constructor/messages.messagesSlice):
///
/// - matching: every query word begins a word of the text, case-insensitive
///   (Telegram also matches word forms; the fake doesn't); an empty query
///   matches everything, then `filter`, `min_date`/`max_date` (exclusive) and
///   the sender apply;
/// - global: results of the chosen Chats ordered by (rate, peer, id), newest
///   first; a page holds the `limit` results strictly after (`offset_rate`,
///   `offset_peer`, `offset_id`), and a slice's `next_rate` is the rate of its
///   last message;
/// - one Chat: newest first, ids strictly below `offset_id`;
/// - the answer is `messages.messages` when a first page holds every result,
///   else `messages.messagesSlice` (`messages.channelMessages` in a channel,
///   which has no `next_rate`). Never more than `limit` messages, and pages
///   never overlap.
pub fn search_answer(chats: &[FakeSearchChat], me: i64, request: &SearchRequest) -> SearchAnswer {
    let limit = request.limit.clamp(0, SEARCH_MAX_LIMIT) as usize;
    let matches = |chat: ChatId, m: &RawHistoryMessage, from: Option<SearchFrom>| {
        let sender = if m.out {
            ChatId::User(me)
        } else {
            m.from.unwrap_or(chat)
        };
        search::text_matches(&m.text, &request.q)
            && request.filter.is_none_or(|f| media_matches(f, m))
            && (request.min_date == 0 || m.date > request.min_date)
            && (request.max_date == 0 || m.date < request.max_date)
            && from.is_none_or(|from| match from {
                SearchFrom::Me => sender == ChatId::User(me),
                SearchFrom::Peer(peer) => sender == peer.chat(),
            })
    };
    let found = |chat: ChatId, m: &RawHistoryMessage| RawFoundMessage {
        chat,
        message: m.clone(),
    };
    let (page, first_page, rest, channel) = match request.scope {
        RequestScope::Global {
            chat_type,
            folder_id,
            offset_rate,
            offset_peer,
            offset_id,
        } => {
            let mut results: Vec<RawFoundMessage> = chats
                .iter()
                .filter(|c| match chat_type {
                    None => true,
                    Some(ChatType::Private) => matches!(c.id, ChatId::User(_)),
                    Some(ChatType::Groups) => {
                        matches!(c.kind, ChatKind::Group | ChatKind::Supergroup)
                    }
                    Some(ChatType::Channels) => c.kind == ChatKind::Channel,
                })
                .filter(|c| match folder_id {
                    Some(1) => c.archived,
                    Some(0) => !c.archived,
                    _ => true,
                })
                .flat_map(|c| {
                    c.messages
                        .iter()
                        .filter(|m| matches(c.id, m, None))
                        .map(|m| found(c.id, m))
                })
                .collect();
            let order =
                |f: &RawFoundMessage| (fake_rate(f.message.date), peer_key(f.chat), f.message.id);
            results.sort_by_key(|f| std::cmp::Reverse(order(f)));
            let first_page = offset_rate == 0 && offset_peer.is_none() && offset_id == 0;
            if !first_page {
                let offset = (
                    offset_rate,
                    offset_peer.map_or(i64::MAX, |p| peer_key(p.chat())),
                    offset_id,
                );
                results.retain(|f| order(f) < offset);
            }
            let rest = results.split_off(limit.min(results.len()));
            (results, first_page, rest.len(), false)
        }
        RequestScope::Chat {
            chat,
            from,
            offset_id,
        } => {
            let mut results: Vec<RawFoundMessage> = chats
                .iter()
                .filter(|c| c.id == chat.chat())
                .flat_map(|c| {
                    c.messages
                        .iter()
                        .filter(|m| offset_id == 0 || m.id < offset_id)
                        .filter(|m| matches(c.id, m, from))
                        .map(|m| found(c.id, m))
                })
                .collect();
            results.sort_by_key(|f| std::cmp::Reverse(f.message.id));
            let rest = results.split_off(limit.min(results.len()));
            let channel = matches!(chat.chat(), ChatId::Channel(_));
            (results, offset_id == 0, rest.len(), channel)
        }
    };
    let complete = first_page && rest == 0 && !channel;
    let global = matches!(request.scope, RequestScope::Global { .. });
    SearchAnswer {
        next_rate: (global && !complete)
            .then(|| page.last().map(|f| fake_rate(f.message.date)))
            .flatten(),
        returned: page.len(),
        complete,
        messages: page,
        peers: Vec::new(),
    }
}

/// Whether `contacts.search` finds `peer` for `q`: every word of `q` begins
/// a word of the name or title, or `q` begins the @username, ignoring case.
/// Taken literally: `@alice` begins no username (the tool strips the `@`).
fn peer_matches(peer: &RawPeer, q: &str) -> bool {
    let q = q.to_lowercase();
    search::text_matches(&peer.title, &q)
        || peer
            .username
            .as_deref()
            .is_some_and(|u| u.to_lowercase().starts_with(&q))
}

/// `contacts.search` as Telegram answers it (core.telegram.org/method/contacts.search,
/// /constructor/contacts.found; how the lists are filled is not documented,
/// so this is the least polite reading):
///
/// - `my_results`: the Account's own Chats (main list and Archive) and
///   contacts that match, in the given order;
/// - `results`: public peers that match, the Account's own public Chats among
///   them (so a peer can be in both lists), and public ones it isn't in;
/// - each list holds at most `limit` peers; `users`/`chats` describe them all,
///   `min` ones without a hash.
pub fn contacts_search(
    own: &[RawPeer],
    public: &[RawPeer],
    min: &[ChatId],
    request: &FindRequest,
) -> FoundAnswer {
    let limit = request.limit.max(0) as usize;
    let found = |peers: &mut dyn Iterator<Item = &RawPeer>| -> Vec<RawPeer> {
        peers
            .filter(|p| peer_matches(p, &request.q))
            .take(limit)
            .cloned()
            .collect()
    };
    let mine = found(&mut own.iter());
    let global = found(
        &mut own
            .iter()
            .filter(|p| p.username.is_some())
            .chain(public.iter()),
    );
    let peers = mine
        .iter()
        .chain(&global)
        .map(|p| RawPeer {
            access_hash: p.access_hash.filter(|_| !min.contains(&p.id)),
            ..p.clone()
        })
        .collect();
    FoundAnswer {
        my_results: mine.iter().map(|p| p.id).collect(),
        results: global.iter().map(|p| p.id).collect(),
        peers,
    }
}

/// Telegram's `inputMessagesFilter*` on one message. Links: the fake counts
/// a `://` in the text (Telegram goes by its url entities).
fn media_matches(filter: MediaFilter, m: &RawHistoryMessage) -> bool {
    let media = match filter {
        MediaFilter::Link => return m.text.contains("://"),
        MediaFilter::Photo => Media::Photo,
        MediaFilter::Video => Media::Video,
        MediaFilter::Document => Media::Document,
        MediaFilter::Voice => Media::Voice,
        MediaFilter::VideoNote => Media::VideoNote,
        MediaFilter::Gif => Media::Gif,
    };
    m.media == Some(media)
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

    /// The main list or the Archive, in the order of `chats`. Cursors are
    /// `fake:<offset>` (main list) and `fake:a<offset>` (Archive).
    async fn list_chats(
        &self,
        limit: usize,
        cursor: Option<&str>,
        archived: bool,
    ) -> Result<ChatPage, TgError> {
        self.flood()?;
        let (start, archived) = match cursor {
            None => (0, archived),
            Some(c) => {
                let offset = c.strip_prefix(CURSOR_PREFIX).ok_or_else(foreign_cursor)?;
                let (offset, archived) = match offset.strip_prefix('a') {
                    Some(offset) => (offset, true),
                    None => (offset, false),
                };
                (
                    offset.parse::<usize>().map_err(|_| foreign_cursor())?,
                    archived,
                )
            }
        };
        let listing: Vec<&ChatSummary> = self
            .data
            .chats
            .iter()
            .filter(|c| {
                c.id.parse::<ChatRef>()
                    .is_ok_and(|id| self.data.archived.contains(&id.chat()))
                    == archived
            })
            .collect();
        let chats: Vec<ChatSummary> = listing
            .iter()
            .skip(start)
            .take(limit)
            .map(|c| ChatSummary {
                id: c
                    .id
                    .parse::<ChatRef>()
                    .map_or_else(|_| c.id.clone(), |id| fake_issued_id(id.chat())),
                ..(*c).clone()
            })
            .collect();
        let end = start + chats.len();
        let folder = if archived { "a" } else { "" };
        Ok(ChatPage {
            next_cursor: (end < listing.len()).then(|| format!("{CURSOR_PREFIX}{folder}{end}")),
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
        read_history(
            id,
            &query,
            |request| {
                let messages = get_history(history, &request, self.data.past_newest);
                std::future::ready(Ok(HistoryAnswer {
                    returned: messages.len(),
                    messages,
                    peers: peers.clone(),
                }))
            },
            |request| {
                std::future::ready(Ok(MessageAnswer {
                    messages: get_message(&self.data.messages, chat, &request),
                    peers: peers.clone(),
                }))
            },
        )
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

    /// The same search calls as the Live client, answered by [`search_answer`].
    async fn search_messages(&self, query: SearchQuery) -> Result<SearchPage, TgError> {
        if let Some(seconds) = self.data.search_flood_wait {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Search,
            });
        }
        if let SearchScope::Chat { chat, from } = query.scope {
            self.check_hash(chat)?;
            if !self.knows(chat.chat()) {
                return Err(TgError::InvalidInput(format!(
                    "{} is not one of your chats; chat ids come from list_chats",
                    chat.chat()
                )));
            }
            if let Some(SearchFrom::Peer(peer)) = from {
                self.check_hash(peer)?;
            }
        }
        let chats: Vec<FakeSearchChat> = self
            .data
            .messages
            .iter()
            .map(|(&id, messages)| FakeSearchChat {
                id,
                kind: self
                    .data
                    .chats
                    .iter()
                    .find(|c| c.id.parse::<ChatRef>().is_ok_and(|r| r.chat() == id))
                    .map_or_else(|| fallback_kind(id), |c| c.kind),
                archived: self.data.archived.contains(&id),
                messages,
            })
            .collect();
        let peers = self.peers();
        let me = self.data.account.id;
        search::search(&query, |request| {
            // SEARCH_QUERY_EMPTY, as the Live client reports it.
            let answer = if request.q.is_empty()
                && request.filter.is_none()
                && !matches!(request.scope, RequestScope::Chat { from: Some(_), .. })
            {
                Err(TgError::InvalidInput(QUERY_REQUIRED.into()))
            } else {
                Ok(SearchAnswer {
                    peers,
                    ..search_answer(&chats, me, &request)
                })
            };
            std::future::ready(answer)
        })
        .await
    }

    /// The same `contacts.search` call as the Live client, answered by
    /// [`contacts_search`].
    async fn find_chats(&self, query: &str, limit: usize) -> Result<FoundChats, TgError> {
        if let Some(seconds) = self.data.search_flood_wait {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Search,
            });
        }
        let me = self.data.account.id;
        let own: Vec<RawPeer> = self
            .peers()
            .into_iter()
            .filter(|p| p.id != ChatId::User(me))
            .collect();
        let public = Self::raw_peers(&self.data.public);
        find::find_chats(query, limit, |request| {
            std::future::ready(Ok(contacts_search(
                &own,
                &public,
                &self.data.min_peers,
                &request,
            )))
        })
        .await
    }
}

fn foreign_cursor() -> TgError {
    TgError::InvalidInput("cursor is not one this server returned".into())
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

    /// The fake knows a Chat if it lists it, has history for it or it is a
    /// contact.
    fn knows(&self, chat: ChatId) -> bool {
        self.data.messages.contains_key(&chat)
            || self
                .data
                .chats
                .iter()
                .chain(&self.data.contacts)
                .any(|c| c.id.parse::<ChatRef>().is_ok_and(|id| id.chat() == chat))
    }

    fn raw_peers(chats: &[ChatSummary]) -> Vec<RawPeer> {
        chats
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
            .collect()
    }

    /// The Account's Chats, its contacts and itself.
    fn peers(&self) -> Vec<RawPeer> {
        let me = &self.data.account;
        Self::raw_peers(&self.data.chats)
            .into_iter()
            .chain(Self::raw_peers(&self.data.contacts))
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
                media: None,
                reply_to: None,
                reply_elsewhere: None,
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
    fn get_messages_looks_in_the_channel_or_across_all_other_chats() {
        let message = |id| RawHistoryMessage {
            id,
            ..history()[0].clone()
        };
        let (alice, bob, group, channel) = (
            ChatId::User(1),
            ChatId::User(2),
            ChatId::Chat(3),
            ChatId::Channel(4),
        );
        let histories = std::collections::HashMap::from([
            (alice, vec![message(10)]),
            (bob, vec![message(11)]),
            (group, vec![message(12)]),
            (channel, vec![message(10), message(11)]),
        ]);
        let found = |chat, id| -> Vec<(ChatId, i32)> {
            get_message(&histories, chat, &MessageRequest { id })
                .into_iter()
                .map(|f| (f.chat, f.message.id))
                .collect()
        };
        // messages.getMessages: the Account's ids, whichever chat asks.
        for asking in [alice, bob, group] {
            assert_eq!(found(asking, 10), [(alice, 10)]);
            assert_eq!(found(asking, 11), [(bob, 11)]);
            assert_eq!(found(asking, 12), [(group, 12)]);
        }
        // channels.getMessages: that channel only.
        assert_eq!(found(channel, 10), [(channel, 10)]);
        assert_eq!(found(channel, 12), []);
        assert_eq!(found(ChatId::Channel(5), 10), []);
        // Deleted or never there: messageEmpty.
        assert_eq!(found(alice, 13), []);
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
