//! Chats as the tools see them, independent of grammers/TL.
//!
//! The Telegram adapter extracts plain [`RawDialog`]s, [`RawPeer`]s and
//! [`RawMessage`]s from a `messages.getDialogs` answer; everything that needs
//! thinking (pairing, titles, kinds, the pagination cursor) happens here.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    find::FoundChat,
    messages::{MediaKind, MediaSummary},
};

/// Which Chat: `user:<id>`, `chat:<id>` (basic group) or `channel:<id>` (channels
/// and supergroups). The tools hand out and take [`ChatRef`]s, which add the
/// access hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatId {
    User(i64),
    Chat(i64),
    Channel(i64),
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatId::User(id) => write!(f, "user:{id}"),
            ChatId::Chat(id) => write!(f, "chat:{id}"),
            ChatId::Channel(id) => write!(f, "channel:{id}"),
        }
    }
}

impl Serialize for ChatId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ChatId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for ChatId {
    type Err = TgError;

    fn from_str(s: &str) -> Result<Self, TgError> {
        let invalid = || {
            TgError::InvalidInput(format!(
                "'{s}' is not a chat id (expected user:N, chat:N or channel:N)"
            ))
        };
        let (kind, id) = s.split_once(':').ok_or_else(invalid)?;
        if !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let id: i64 = id.parse().map_err(|_| invalid())?;
        if id <= 0 {
            return Err(invalid());
        }
        match kind {
            "user" => Ok(ChatId::User(id)),
            "chat" => Ok(ChatId::Chat(id)),
            "channel" => Ok(ChatId::Channel(id)),
            _ => Err(invalid()),
        }
    }
}

/// A chat id as the tools hand it out and take it back: the Chat, plus the
/// access hash Telegram needs to address a user or channel, so resolving an id
/// takes no lookup. Written `user:N~H`, `channel:N~H` (H: the hash as 11
/// base64url characters) and `chat:N` (basic groups need no hash). Ids without
/// `~H`, as issued before hashes were added, are still accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatRef {
    chat: ChatId,
    access_hash: Option<i64>,
}

/// Longest chat id we issue: `channel:` + 19 digits + `~` + 11 hash characters.
pub const CHAT_REF_MAX_LEN: usize = 39;

/// What a chat id looks like (for input schemas).
pub const CHAT_REF_PATTERN: &str =
    r"^(chat:[1-9][0-9]{0,18}|(user|channel):[1-9][0-9]{0,18}(~[A-Za-z0-9_-]{11})?)$";

/// Base64url characters of an encoded access hash (8 bytes, no padding).
const HASH_CHARS: usize = 11;

impl ChatRef {
    /// A basic group's hash is dropped: it has none.
    pub fn new(chat: ChatId, access_hash: Option<i64>) -> Self {
        let access_hash = access_hash.filter(|_| !matches!(chat, ChatId::Chat(_)));
        Self { chat, access_hash }
    }

    pub fn chat(&self) -> ChatId {
        self.chat
    }

    pub fn access_hash(&self) -> Option<i64> {
        self.access_hash
    }
}

/// An id without a hash: resolved through the Live client's peer cache.
impl From<ChatId> for ChatRef {
    fn from(chat: ChatId) -> Self {
        Self::new(chat, None)
    }
}

impl fmt::Display for ChatRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.chat)?;
        if let Some(hash) = self.access_hash {
            write!(f, "~{}", URL_SAFE_NO_PAD.encode(hash.to_be_bytes()))?;
        }
        Ok(())
    }
}

impl FromStr for ChatRef {
    type Err = TgError;

    fn from_str(s: &str) -> Result<Self, TgError> {
        let invalid = || {
            TgError::InvalidInput(format!(
                "'{s}' is not a chat id (expected user:N, chat:N or channel:N, \
                 optionally followed by ~ and an access hash, as list_chats returns them)"
            ))
        };
        let (chat, hash) = match s.split_once('~') {
            Some((chat, hash)) => (chat, Some(hash)),
            None => (s, None),
        };
        let chat: ChatId = chat.parse().map_err(|_| invalid())?;
        let access_hash = match hash {
            None => None,
            Some(_) if matches!(chat, ChatId::Chat(_)) => return Err(invalid()),
            Some(hash) => Some(decode_hash(hash).ok_or_else(invalid)?),
        };
        Ok(Self { chat, access_hash })
    }
}

fn decode_hash(s: &str) -> Option<i64> {
    if s.len() != HASH_CHARS {
        return None;
    }
    let bytes: [u8; 8] = URL_SAFE_NO_PAD.decode(s).ok()?.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatKind {
    /// One-to-one chat with a person.
    Private,
    /// One-to-one chat with a bot.
    Bot,
    /// Basic group.
    Group,
    Supergroup,
    Channel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessagePreview {
    /// UTC, RFC 3339.
    pub date: String,
    /// The text or caption, shortened, possibly empty (`media` says what there
    /// is); a service message as `[service message]`; text with nothing
    /// visible (only hidden characters) as `[no visible text]`.
    pub text: String,
    /// Only the kind; get_messages describes it in full. Absent without media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatSummary {
    /// Pass this to the other tools.
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChatKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The connected account itself: this chat is Telegram's "Saved Messages".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
    pub unread_count: i32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<MessagePreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatPage {
    pub chats: Vec<ChatSummary>,
    /// Pass back as `cursor` to get the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Details of one Chat (`get_chat_info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatInfo {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChatKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The connected account itself: this chat is Telegram's "Saved Messages".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
    /// Bio of a user, or the description of a group/channel. Absent if there is
    /// none, or if the person hides it from this account (privacy settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Members of a group, subscribers of a channel; absent when Telegram hides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_count: Option<i32>,
    /// Users only: the bio, as Telegram shows it to this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    /// Users only: when they were last online, as Telegram shows it to this
    /// account. Absent when Telegram gives none (bots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<UserStatus>,
    /// With `status: last_seen`: when, UTC, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bot: bool,
    /// Bots only: what the bot says it can do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
    /// Telegram marks the user as a scam.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scam: bool,
    /// Telegram marks the user as impersonating someone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fake: bool,
    /// Users only, when Telegram shows it to this account: international
    /// format, `+` and digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Users only, when Telegram shows it to this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birthday: Option<Birthday>,
    /// Users only: how many groups and channels this account shares with them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_chats_count: Option<i32>,
    /// Only when asked for (`common_chats: true`): those groups and channels,
    /// at most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 20))]
    pub common_chats: Option<Vec<FoundChat>>,
}

/// A user's last-seen status as Telegram shows it to this account: exact
/// (`online`, `last_seen` with `last_seen_at`) or, when they hide it,
/// approximate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Online,
    /// At `last_seen_at`.
    LastSeen,
    /// Within the last few days.
    Recently,
    LastWeek,
    LastMonth,
    /// More than a month ago, or never.
    LongAgo,
}

/// A user's status as Telegram sends it (`UserStatus`), times in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawUserStatus {
    /// `userStatusEmpty`: Telegram's apps show "last seen a long time ago".
    Empty,
    /// Online until `expires`; after that, last seen then.
    Online {
        expires: i32,
    },
    Offline {
        was_online: i32,
    },
    Recently,
    LastWeek,
    LastMonth,
}

impl RawUserStatus {
    /// What the tools say, at Unix time `now`, with the exact time if any.
    pub fn shown(self, now: i64) -> (UserStatus, Option<String>) {
        match self {
            RawUserStatus::Empty => (UserStatus::LongAgo, None),
            RawUserStatus::Online { expires } if i64::from(expires) > now => {
                (UserStatus::Online, None)
            }
            RawUserStatus::Online { expires: at } | RawUserStatus::Offline { was_online: at } => (
                UserStatus::LastSeen,
                Some(rfc3339(at)).filter(|t| !t.is_empty()),
            ),
            RawUserStatus::Recently => (UserStatus::Recently, None),
            RawUserStatus::LastWeek => (UserStatus::LastWeek, None),
            RawUserStatus::LastMonth => (UserStatus::LastMonth, None),
        }
    }
}

/// A birthday as Telegram shows it; the year only if the user shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Birthday {
    #[schemars(range(min = 1, max = 31))]
    pub day: i32,
    #[schemars(range(min = 1, max = 12))]
    pub month: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
}

/// A user's profile as the adapter extracts it from `users.getFullUser` and
/// the user object. Telegram only sends `phone` and `birthday` when it shows
/// them to this Account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawProfile {
    pub bio: Option<String>,
    pub status: Option<RawUserStatus>,
    pub bot: bool,
    pub bot_description: Option<String>,
    pub verified: bool,
    pub premium: bool,
    pub scam: bool,
    pub fake: bool,
    /// Digits, as Telegram sends it (no `+`).
    pub phone: Option<String>,
    pub birthday: Option<Birthday>,
    pub common_chats_count: i32,
}

/// Normalise what Telegram returns: empty descriptions are none, counts are
/// only shown for groups and channels.
pub fn chat_info(
    peer: &RawPeer,
    description: Option<String>,
    member_count: Option<i32>,
) -> ChatInfo {
    let member_count = match peer.kind {
        ChatKind::Private | ChatKind::Bot => None,
        _ => member_count.filter(|n| *n > 0),
    };
    ChatInfo {
        id: peer.chat_ref().to_string(),
        title: peer.title.clone(),
        kind: peer.kind,
        username: peer.username.clone(),
        is_self: peer.is_self,
        description: non_empty(description),
        member_count,
        bio: None,
        status: None,
        last_seen_at: None,
        bot: false,
        bot_description: None,
        verified: false,
        premium: false,
        scam: false,
        fake: false,
        phone: None,
        birthday: None,
        common_chats_count: None,
        common_chats: None,
    }
}

/// A user's details: their profile as Telegram shows it at Unix time `now`.
/// Empty strings are none; a birthday that is no date is left out.
pub fn user_info(peer: &RawPeer, profile: RawProfile, now: i64) -> ChatInfo {
    let RawProfile {
        bio,
        status,
        bot,
        bot_description,
        verified,
        premium,
        scam,
        fake,
        phone,
        birthday,
        common_chats_count,
    } = profile;
    // The Account's own status is always "online now" while it is being used:
    // noise, not information.
    let status = status.filter(|_| !peer.is_self);
    let (status, last_seen_at) = match status.map(|s| s.shown(now)) {
        Some((status, at)) => (Some(status), at),
        None => (None, None),
    };
    // A bot's About and its "What can this bot do?" text are often the same;
    // show it once, as the description.
    let bot_description = non_empty(bot_description);
    let bio = non_empty(bio).filter(|b| bot_description.as_ref() != Some(b));
    let phone = non_empty(phone).map(|p| match p.strip_prefix('+') {
        Some(_) => p,
        None => format!("+{p}"),
    });
    ChatInfo {
        bio,
        status,
        last_seen_at,
        bot,
        bot_description,
        verified,
        premium,
        scam,
        fake,
        phone,
        birthday: birthday
            .filter(|b| (1..=31).contains(&b.day) && (1..=12).contains(&b.month))
            .map(|b| Birthday {
                year: b.year.filter(|y| *y > 0),
                ..b
            }),
        common_chats_count: Some(common_chats_count.max(0)),
        ..chat_info(peer, None, None)
    }
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

// ── What the adapter extracts from TL ───────────────────────────────────────

/// A user, basic group or channel from a response, with what's needed to name it
/// and to address it again (`access_hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPeer {
    pub id: ChatId,
    pub title: String,
    pub kind: ChatKind,
    pub username: Option<String>,
    pub access_hash: Option<i64>,
    /// The connected Account itself (its chat is Telegram's Saved Messages).
    pub is_self: bool,
}

impl RawPeer {
    /// The id to hand out for this peer, with its access hash if it has one.
    pub fn chat_ref(&self) -> ChatRef {
        ChatRef::new(self.id, self.access_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    pub chat: ChatId,
    pub id: i32,
    /// Unix seconds.
    pub date: i32,
    /// The text or caption, empty if there is none.
    pub text: String,
    pub media: Option<MediaKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDialog {
    pub chat: ChatId,
    pub top_message: i32,
    pub unread_count: i32,
    pub pinned: bool,
    /// Telegram's peer folder: 0 (absent in TL) is the main list, 1 the Archive.
    pub folder_id: i32,
}

/// Which listing a page belongs to (main list or Archive), and whether it is
/// its first page. Decides both the `messages.getDialogs` flags and what of the
/// answer is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    pub archived: bool,
    pub first_page: bool,
}

impl Listing {
    /// Asked for explicitly: without a folder Telegram answers from all of them.
    pub fn folder_id(self) -> i32 {
        if self.archived { 1 } else { 0 }
    }

    /// Pinned chats come once, first, on the first page; later pages skip them.
    pub fn exclude_pinned(self) -> bool {
        !self.first_page
    }
}

/// Where the next `messages.getDialogs` page starts: the last unpinned dialog
/// of this one, or the top of the unpinned list (`offset_peer` absent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(rename = "d")]
    pub offset_date: i32,
    #[serde(rename = "i")]
    pub offset_id: i32,
    #[serde(rename = "p", default, skip_serializing_if = "Option::is_none")]
    pub offset_peer: Option<String>,
    #[serde(rename = "h", default, skip_serializing_if = "Option::is_none")]
    pub offset_access_hash: Option<i64>,
    #[serde(rename = "a", default)]
    pub archived: bool,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor serializes"))
    }

    pub fn decode(s: &str) -> Result<Self, TgError> {
        let invalid = || TgError::InvalidInput("cursor is not one this server returned".into());
        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid())?;
        let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if let Some(peer) = &cursor.offset_peer {
            peer.parse::<ChatId>().map_err(|_| invalid())?;
        }
        Ok(cursor)
    }
}

/// Assemble a page of at most `limit` chats. `full_page` = Telegram returned
/// as many dialogs as asked for, so there may be more. Whatever Telegram sends,
/// the page keeps only dialogs of the listing's folder, pinned ones only on the
/// first page, each chat once. The page is cut here and nowhere else: the
/// cursor has to know where it ends.
pub fn assemble_page(
    dialogs: &[RawDialog],
    peers: &[RawPeer],
    messages: &[RawMessage],
    limit: usize,
    full_page: bool,
    listing: Listing,
) -> ChatPage {
    let peers: HashMap<ChatId, &RawPeer> = peers.iter().map(|p| (p.id, p)).collect();
    let messages: HashMap<(ChatId, i32), &RawMessage> =
        messages.iter().map(|m| ((m.chat, m.id), m)).collect();

    let mut seen = HashSet::new();
    let mut shown: Vec<&RawDialog> = dialogs
        .iter()
        .filter(|d| d.folder_id == listing.folder_id())
        .filter(|d| !(d.pinned && listing.exclude_pinned()))
        .filter(|d| seen.insert(d.chat))
        .collect();
    // A first page brings the pinned dialogs on top of `limit` unpinned ones.
    let cut = shown.len() > limit;
    shown.truncate(limit);

    let chats = shown
        .iter()
        .map(|dialog| {
            let peer = peers.get(&dialog.chat);
            let top = messages.get(&(dialog.chat, dialog.top_message));
            ChatSummary {
                id: ChatRef::new(dialog.chat, peer.and_then(|p| p.access_hash)).to_string(),
                title: peer
                    .map(|p| p.title.clone())
                    .unwrap_or_else(|| "Unknown chat".into()),
                kind: peer.map_or_else(|| fallback_kind(dialog.chat), |p| p.kind),
                username: peer.and_then(|p| p.username.clone()),
                is_self: peer.is_some_and(|p| p.is_self),
                unread_count: dialog.unread_count,
                pinned: dialog.pinned,
                last_message: top.map(|m| MessagePreview {
                    date: rfc3339(m.date),
                    text: m.text.clone(),
                    media: m.media.map(|kind| MediaSummary { kind }),
                }),
            }
        })
        .collect();

    // A cut page continues right after its last shown chat. Otherwise offsets
    // come from what Telegram sent, kept or not, so dropped dialogs aren't asked
    // for again. Pinned dates are out of order: a pinned dialog is never one.
    let last_unpinned = if cut {
        shown.iter().copied().rev().find(|d| !d.pinned)
    } else {
        dialogs.iter().rev().find(|d| !d.pinned)
    };
    let next = match last_unpinned {
        Some(last) => Some(Cursor {
            offset_date: messages
                .get(&(last.chat, last.top_message))
                .map(|m| m.date)
                .unwrap_or(0),
            offset_id: last.top_message,
            offset_peer: Some(last.chat.to_string()),
            offset_access_hash: peers.get(&last.chat).and_then(|p| p.access_hash),
            archived: listing.archived,
        }),
        // A first page of pinned chats only: the unpinned ones start at the top.
        None if listing.first_page => Some(Cursor {
            offset_date: 0,
            offset_id: 0,
            offset_peer: None,
            offset_access_hash: None,
            archived: listing.archived,
        }),
        // A later page with nothing to go on would only repeat itself.
        None => None,
    };
    // What was cut off is still to come.
    let next_cursor = next.filter(|_| cut || full_page).map(|c| c.encode());

    ChatPage { chats, next_cursor }
}

/// The kind of a Chat Telegram didn't describe, as far as its id tells.
pub fn fallback_kind(chat: ChatId) -> ChatKind {
    match chat {
        ChatId::User(_) => ChatKind::Private,
        ChatId::Chat(_) => ChatKind::Group,
        ChatId::Channel(_) => ChatKind::Channel,
    }
}

pub fn rfc3339(unix: i32) -> String {
    time::OffsetDateTime::from_unix_timestamp(i64::from(unix))
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN_FIRST: Listing = Listing {
        archived: false,
        first_page: true,
    };
    const MAIN_NEXT: Listing = Listing {
        archived: false,
        first_page: false,
    };
    const ARCHIVE_FIRST: Listing = Listing {
        archived: true,
        first_page: true,
    };

    /// A main-list dialog whose top message is `user:<n>`'s message `n`, dated `n`.
    fn dialog(n: i64, pinned: bool) -> RawDialog {
        RawDialog {
            chat: ChatId::User(n),
            top_message: n as i32,
            unread_count: 0,
            pinned,
            folder_id: 0,
        }
    }

    fn message(n: i64) -> RawMessage {
        RawMessage {
            chat: ChatId::User(n),
            id: n as i32,
            date: n as i32,
            text: String::new(),
            media: None,
        }
    }

    fn ids(page: &ChatPage) -> Vec<String> {
        page.chats.iter().map(|c| c.id.clone()).collect()
    }

    fn cursor(page: &ChatPage) -> Cursor {
        Cursor::decode(page.next_cursor.as_deref().expect("a next page")).unwrap()
    }

    fn peer(id: ChatId, title: &str, kind: ChatKind, hash: Option<i64>) -> RawPeer {
        RawPeer {
            id,
            title: title.into(),
            kind,
            username: None,
            access_hash: hash,
            is_self: false,
        }
    }

    #[test]
    fn the_accounts_own_chat_is_marked_self() {
        let me = RawPeer {
            is_self: true,
            ..peer(ChatId::User(42), "Me", ChatKind::Private, Some(1))
        };
        let dialogs = [
            RawDialog {
                chat: ChatId::User(42),
                top_message: 1,
                unread_count: 0,
                pinned: false,
                folder_id: 0,
            },
            RawDialog {
                chat: ChatId::User(7),
                top_message: 2,
                unread_count: 0,
                pinned: false,
                folder_id: 0,
            },
        ];
        let other = peer(ChatId::User(7), "Bob", ChatKind::Private, Some(2));
        let page = assemble_page(&dialogs, &[me.clone(), other], &[], 100, false, MAIN_FIRST);
        assert!(page.chats[0].is_self);
        assert!(!page.chats[1].is_self);
        assert!(chat_info(&me, None, None).is_self);
    }

    #[test]
    fn chat_info_drops_empty_descriptions_and_person_counts() {
        let team = peer(ChatId::Channel(2), "Team", ChatKind::Supergroup, Some(1));
        let info = chat_info(&team, Some("  Our team chat \n".into()), Some(12));
        assert_eq!(info.description.as_deref(), Some("Our team chat"));
        assert_eq!(info.member_count, Some(12));
        assert_eq!(
            chat_info(&team, Some("   ".into()), Some(0)).description,
            None
        );
        assert_eq!(chat_info(&team, None, Some(0)).member_count, None);

        let alice = peer(ChatId::User(1), "Alice", ChatKind::Private, Some(1));
        assert_eq!(
            chat_info(&alice, Some("bio".into()), Some(2)).member_count,
            None
        );
    }

    #[test]
    fn each_status_shows_as_telegram_apps_show_it() {
        const NOW: i64 = 1_700_000_000;
        let at = |t: &str| Some(t.to_string());
        for (raw, shown) in [
            (RawUserStatus::Empty, (UserStatus::LongAgo, None)),
            (
                RawUserStatus::Online {
                    expires: NOW as i32 + 60,
                },
                (UserStatus::Online, None),
            ),
            // Online ran out without an update: last seen when it did.
            (
                RawUserStatus::Online {
                    expires: NOW as i32 - 60,
                },
                (UserStatus::LastSeen, at("2023-11-14T22:12:20Z")),
            ),
            (
                RawUserStatus::Offline {
                    was_online: 1_600_000_000,
                },
                (UserStatus::LastSeen, at("2020-09-13T12:26:40Z")),
            ),
            (RawUserStatus::Recently, (UserStatus::Recently, None)),
            (RawUserStatus::LastWeek, (UserStatus::LastWeek, None)),
            (RawUserStatus::LastMonth, (UserStatus::LastMonth, None)),
        ] {
            assert_eq!(raw.shown(NOW), shown, "{raw:?}");
        }
        assert_eq!(
            serde_json::to_value([
                UserStatus::Online,
                UserStatus::LastSeen,
                UserStatus::Recently,
                UserStatus::LastWeek,
                UserStatus::LastMonth,
                UserStatus::LongAgo,
            ])
            .unwrap(),
            serde_json::json!([
                "online",
                "last_seen",
                "recently",
                "last_week",
                "last_month",
                "long_ago"
            ])
        );
    }

    #[test]
    fn user_info_shows_the_profile_and_only_what_telegram_shows() {
        let alice = peer(ChatId::User(1), "Alice", ChatKind::Private, Some(1));
        let info = user_info(
            &alice,
            RawProfile {
                bio: Some(" coffee \n".into()),
                status: Some(RawUserStatus::Offline { was_online: 0 }),
                verified: true,
                phone: Some("79991234567".into()),
                birthday: Some(Birthday {
                    day: 17,
                    month: 5,
                    year: Some(1990),
                }),
                common_chats_count: 3,
                ..RawProfile::default()
            },
            1,
        );
        assert_eq!(info.bio.as_deref(), Some("coffee"));
        assert_eq!(info.description, None, "a user's bio is `bio`");
        assert_eq!(info.member_count, None);
        assert_eq!(info.status, Some(UserStatus::LastSeen));
        assert_eq!(info.last_seen_at.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert!(info.verified && !info.premium && !info.scam && !info.fake && !info.bot);
        assert_eq!(info.phone.as_deref(), Some("+79991234567"));
        assert_eq!(info.birthday.unwrap().year, Some(1990));
        assert_eq!(info.common_chats_count, Some(3));
        assert_eq!(info.common_chats, None, "only when asked for");

        // Hidden from this account: nothing to show, nothing made up.
        let hidden = user_info(&alice, RawProfile::default(), 1);
        assert_eq!(
            (hidden.bio, hidden.phone, hidden.birthday, hidden.status),
            (None, None, None, None)
        );
        assert_eq!(
            hidden.common_chats_count,
            Some(0),
            "the count is always there"
        );

        let odd = user_info(
            &alice,
            RawProfile {
                phone: Some("+1555".into()),
                bio: Some("  ".into()),
                birthday: Some(Birthday {
                    day: 1,
                    month: 2,
                    year: Some(0),
                }),
                ..RawProfile::default()
            },
            1,
        );
        assert_eq!(odd.phone.as_deref(), Some("+1555"));
        assert_eq!(odd.bio, None);
        assert_eq!(
            odd.birthday,
            Some(Birthday {
                day: 1,
                month: 2,
                year: None
            })
        );
        let no_date = RawProfile {
            birthday: Some(Birthday {
                day: 0,
                month: 13,
                year: None,
            }),
            ..RawProfile::default()
        };
        assert_eq!(user_info(&alice, no_date, 1).birthday, None);
    }

    #[test]
    fn a_bot_has_its_description() {
        let bot = peer(ChatId::User(9), "Helper", ChatKind::Bot, Some(1));
        let info = user_info(
            &bot,
            RawProfile {
                bot: true,
                bio: Some("I help".into()),
                bot_description: Some("What can this bot do?\nEverything.".into()),
                ..RawProfile::default()
            },
            1,
        );
        assert!(info.bot);
        assert_eq!(info.kind, ChatKind::Bot);
        assert_eq!(
            info.bot_description.as_deref(),
            Some("What can this bot do?\nEverything.")
        );
        assert_eq!(info.status, None, "bots have no status");
        assert_eq!(info.bio.as_deref(), Some("I help"), "a bot's About is kept");
    }

    #[test]
    fn a_bots_about_that_repeats_its_description_is_shown_once() {
        let bot = peer(ChatId::User(9), "Helper", ChatKind::Bot, Some(1));
        let info = user_info(
            &bot,
            RawProfile {
                bot: true,
                bio: Some(" Does everything. ".into()),
                bot_description: Some("Does everything.".into()),
                ..RawProfile::default()
            },
            1,
        );
        assert_eq!(info.bio, None);
        assert_eq!(info.bot_description.as_deref(), Some("Does everything."));
    }

    #[test]
    fn the_account_itself_has_no_last_seen_status() {
        let me = RawPeer {
            is_self: true,
            ..peer(ChatId::User(5), "Me", ChatKind::Private, Some(1))
        };
        let info = user_info(
            &me,
            RawProfile {
                status: Some(RawUserStatus::Offline { was_online: 100 }),
                ..RawProfile::default()
            },
            200,
        );
        assert!(info.is_self);
        assert_eq!(info.status, None);
        assert_eq!(info.last_seen_at, None);
    }

    #[test]
    fn chat_ids_round_trip_and_reject_junk() {
        for id in [
            ChatId::User(42),
            ChatId::Chat(7),
            ChatId::Channel(1_234_567_890_123),
        ] {
            assert_eq!(id.to_string().parse::<ChatId>().unwrap(), id);
        }
        for bad in [
            "42", "user:", "user:-1", "user:0", "group:5", "user:abc", "", "user:+5",
        ] {
            assert!(bad.parse::<ChatId>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn issued_ids_carry_the_access_hash_and_round_trip() {
        for hash in [0, 1, -1, 42, -5_000_000_000, i64::MAX, i64::MIN] {
            for chat in [ChatId::User(42), ChatId::Channel(1_234_567_890_123)] {
                let id = ChatRef::new(chat, Some(hash));
                let text = id.to_string();
                let parsed: ChatRef = text.parse().unwrap();
                assert_eq!(parsed, id, "{text}");
                assert_eq!(parsed.access_hash(), Some(hash), "{text}");
                assert_eq!(parsed.to_string(), text);
                assert!(
                    text.starts_with(&format!("{chat}~")),
                    "kind and number stay readable: {text}"
                );
                assert!(
                    text.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-:~".contains(&b)),
                    "URL-safe: {text}"
                );
            }
        }
        assert_eq!(
            ChatRef::new(ChatId::User(101), Some(-2)).to_string(),
            "user:101~__________4"
        );
        assert_eq!(
            ChatRef::new(ChatId::Channel(5), Some(0x1234_5678_9abc_def0)).to_string(),
            "channel:5~EjRWeJq83vA"
        );
        let longest = ChatRef::new(ChatId::Channel(i64::MAX), Some(i64::MIN)).to_string();
        assert_eq!(longest.len(), CHAT_REF_MAX_LEN, "{longest}");
    }

    #[test]
    fn hashless_ids_still_parse_and_basic_groups_never_carry_a_hash() {
        let legacy: ChatRef = "channel:7".parse().unwrap();
        assert_eq!(legacy, ChatRef::from(ChatId::Channel(7)));
        assert_eq!(legacy.access_hash(), None);
        assert_eq!(legacy.to_string(), "channel:7");

        let group = ChatRef::new(ChatId::Chat(4), Some(99));
        assert_eq!(group.access_hash(), None);
        assert_eq!(group.to_string(), "chat:4");
        assert_eq!("chat:4".parse::<ChatRef>().unwrap(), group);
    }

    #[test]
    fn malformed_or_misplaced_hashes_are_rejected() {
        let hash = ChatRef::new(ChatId::User(1), Some(-5)).to_string();
        let hash = hash.split_once('~').unwrap().1;
        for bad in [
            format!("chat:4~{hash}"),
            "user:1~".into(),
            "user:1~AAAA".into(),
            format!("user:1~{hash}A"),
            format!("user:1~{hash}~{hash}"),
            "user:1~AAAAAAAAAA!".into(),
            // Non-canonical: the last character carries bits beyond the 8 bytes.
            "user:1~AAAAAAAAAAB".into(),
            format!("user:-1~{hash}"),
            format!("group:1~{hash}"),
            format!("~{hash}"),
        ] {
            assert!(bad.parse::<ChatRef>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn page_pairs_each_dialog_with_its_top_message() {
        let alice = ChatId::User(1);
        let team = ChatId::Channel(2);
        let dialogs = [
            RawDialog {
                chat: team,
                top_message: 900,
                unread_count: 3,
                pinned: true,
                folder_id: 0,
            },
            RawDialog {
                chat: alice,
                top_message: 55,
                unread_count: 0,
                pinned: false,
                folder_id: 0,
            },
        ];
        let peers = [
            peer(alice, "Alice Smith", ChatKind::Private, Some(111)),
            peer(team, "Team", ChatKind::Supergroup, Some(222)),
        ];
        let messages = [
            // Same id in another chat must not be picked up.
            RawMessage {
                chat: team,
                id: 55,
                date: 1,
                text: "wrong one".into(),
                media: None,
            },
            RawMessage {
                chat: alice,
                id: 55,
                date: 1_700_000_000,
                text: "hi".into(),
                media: None,
            },
            RawMessage {
                chat: team,
                id: 900,
                date: 1_700_000_100,
                text: String::new(),
                media: Some(MediaKind::Photo),
            },
        ];

        let page = assemble_page(&dialogs, &peers, &messages, 100, false, MAIN_FIRST);
        assert_eq!(page.next_cursor, None, "a short page is the last one");
        assert_eq!(
            page.chats[0].id,
            ChatRef::new(team, Some(222)).to_string(),
            "ids carry the access hash"
        );
        assert_eq!(
            page.chats[0].id.parse::<ChatRef>().unwrap().access_hash(),
            Some(222)
        );
        assert_eq!(page.chats[0].kind, ChatKind::Supergroup);
        assert!(page.chats[0].pinned);
        assert_eq!(page.chats[0].unread_count, 3);
        assert_eq!(
            page.chats[0].last_message,
            Some(MessagePreview {
                date: "2023-11-14T22:15:00Z".into(),
                text: String::new(),
                media: Some(MediaSummary {
                    kind: MediaKind::Photo
                }),
            })
        );
        assert_eq!(page.chats[1].title, "Alice Smith");
        assert_eq!(
            page.chats[1].last_message,
            Some(MessagePreview {
                date: "2023-11-14T22:13:20Z".into(),
                text: "hi".into(),
                media: None,
            })
        );
    }

    #[test]
    fn full_page_cursor_points_at_the_last_dialog() {
        let bob = ChatId::User(9);
        let dialogs = [RawDialog {
            chat: bob,
            top_message: 77,
            unread_count: 0,
            pinned: false,
            folder_id: 1,
        }];
        let peers = [peer(bob, "Bob", ChatKind::Bot, Some(-5))];
        let messages = [RawMessage {
            chat: bob,
            id: 77,
            date: 1_600_000_000,
            text: "x".into(),
            media: None,
        }];

        let page = assemble_page(&dialogs, &peers, &messages, 1, true, ARCHIVE_FIRST);
        let cursor = Cursor::decode(page.next_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(
            cursor,
            Cursor {
                offset_date: 1_600_000_000,
                offset_id: 77,
                offset_peer: Some("user:9".into()),
                offset_access_hash: Some(-5),
                archived: true,
            }
        );
    }

    #[test]
    fn a_same_numbered_message_from_another_chat_is_never_used() {
        // Message ids are per chat in channels, so collisions are normal.
        let alice = ChatId::User(1);
        let news = ChatId::Channel(2);
        let dialogs = [RawDialog {
            chat: alice,
            top_message: 55,
            unread_count: 0,
            pinned: false,
            folder_id: 0,
        }];
        let messages = [RawMessage {
            chat: news,
            id: 55,
            date: 1,
            text: "someone else's message".into(),
            media: None,
        }];
        let page = assemble_page(&dialogs, &[], &messages, 100, false, MAIN_FIRST);
        assert_eq!(page.chats[0].last_message, None);
    }

    #[test]
    fn unknown_peers_still_get_a_row() {
        let ghost = ChatId::Chat(3);
        let dialogs = [RawDialog {
            chat: ghost,
            top_message: 1,
            unread_count: 0,
            pinned: false,
            folder_id: 0,
        }];
        let page = assemble_page(&dialogs, &[], &[], 100, false, MAIN_FIRST);
        assert_eq!(page.chats[0].title, "Unknown chat");
        assert_eq!(page.chats[0].kind, ChatKind::Group);
        assert_eq!(page.chats[0].last_message, None);
    }

    #[test]
    fn pinned_chats_are_listed_on_the_first_page_only() {
        let dialogs = [dialog(1, true), dialog(9, false), dialog(8, false)];
        let first = assemble_page(&dialogs, &[], &[], 100, false, MAIN_FIRST);
        assert_eq!(ids(&first), ["user:1", "user:9", "user:8"]);
        assert!(first.chats[0].pinned);

        // A later page drops a pinned dialog even if Telegram sends one.
        let later = assemble_page(&dialogs, &[], &[], 100, false, MAIN_NEXT);
        assert_eq!(ids(&later), ["user:9", "user:8"]);
    }

    #[test]
    fn the_next_page_starts_after_the_last_unpinned_dialog() {
        // Pinned dates are not in order, so a trailing pinned dialog is no offset.
        let dialogs = [
            dialog(1, true),
            dialog(9, false),
            dialog(8, false),
            dialog(2, true),
        ];
        let messages: Vec<RawMessage> = [1, 9, 8, 2].map(message).into();
        for listing in [MAIN_FIRST, MAIN_NEXT] {
            let page = assemble_page(&dialogs, &[], &messages, 4, true, listing);
            assert_eq!(
                cursor(&page),
                Cursor {
                    offset_date: 8,
                    offset_id: 8,
                    offset_peer: Some("user:8".into()),
                    offset_access_hash: None,
                    archived: false,
                }
            );
        }
    }

    #[test]
    fn a_first_page_of_only_pinned_chats_continues_from_the_top() {
        let dialogs = [dialog(1, true), dialog(2, true)].map(|d| RawDialog { folder_id: 1, ..d });
        let page = assemble_page(&dialogs, &[], &[1, 2].map(message), 2, true, ARCHIVE_FIRST);
        assert_eq!(page.chats.len(), 2);
        assert_eq!(
            cursor(&page),
            Cursor {
                offset_date: 0,
                offset_id: 0,
                offset_peer: None,
                offset_access_hash: None,
                archived: true,
            }
        );

        // On a later page that would ask for the same page again: stop instead.
        let later = assemble_page(&dialogs, &[], &[], 100, true, MAIN_NEXT);
        assert!(later.chats.is_empty());
        assert_eq!(later.next_cursor, None);
    }

    #[test]
    fn a_cut_first_page_continues_after_its_last_shown_chat() {
        // Telegram sends the pinned dialogs on top of `limit` unpinned ones.
        let dialogs = [
            dialog(5, true),
            dialog(999, true),
            dialog(30, false),
            dialog(29, false),
            dialog(28, false),
        ];
        let messages: Vec<RawMessage> = [5, 999, 30, 29, 28].map(message).into();
        let page = assemble_page(&dialogs, &[], &messages, 3, true, MAIN_FIRST);
        assert_eq!(ids(&page), ["user:5", "user:999", "user:30"]);
        // Telegram starts strictly after the offset: user:29 comes next.
        assert_eq!(
            cursor(&page),
            Cursor {
                offset_date: 30,
                offset_id: 30,
                offset_peer: Some("user:30".into()),
                offset_access_hash: None,
                archived: false,
            }
        );

        // A cut page has more after it, whatever the dialog count said.
        let page = assemble_page(&dialogs, &[], &messages, 3, false, MAIN_FIRST);
        assert_eq!(cursor(&page).offset_peer.as_deref(), Some("user:30"));
    }

    #[test]
    fn a_first_page_cut_to_pinned_chats_continues_from_the_top() {
        let dialogs = [
            dialog(5, true),
            dialog(999, true),
            dialog(3, true),
            dialog(30, false),
            dialog(29, false),
        ];
        let messages: Vec<RawMessage> = [5, 999, 3, 30, 29].map(message).into();
        for limit in [2, 3] {
            let page = assemble_page(&dialogs, &[], &messages, limit, true, MAIN_FIRST);
            assert_eq!(page.chats.len(), limit);
            assert!(page.chats.iter().all(|c| c.pinned));
            assert_eq!(
                cursor(&page),
                Cursor {
                    offset_date: 0,
                    offset_id: 0,
                    offset_peer: None,
                    offset_access_hash: None,
                    archived: false,
                },
                "limit {limit}"
            );
        }
    }

    /// One folder as `messages.getDialogs` serves it (as seen on production):
    /// a first page brings every pinned dialog on top of `limit` unpinned ones,
    /// and each page's unpinned dialogs are the newest strictly older than the
    /// offset. `dialog(n)` is dated `n`, so `unpinned` is in descending order;
    /// pinned dates are deliberately out of that order.
    struct Folder {
        archived: bool,
        pinned: Vec<i64>,
        unpinned: Vec<i64>,
    }

    impl Folder {
        /// What `LiveClient::list_chats` gets from Telegram and hands on.
        fn list_chats(&self, limit: usize, cursor: Option<&str>) -> ChatPage {
            let cursor = cursor.map(|c| Cursor::decode(c).unwrap());
            let listing = Listing {
                archived: cursor.as_ref().map_or(self.archived, |c| c.archived),
                first_page: cursor.is_none(),
            };
            assert_eq!(listing.archived, self.archived);
            let older_than = cursor.map_or(i32::MAX, |c| match c.offset_date {
                0 => i32::MAX,
                date => date,
            });
            let pinned = self.pinned.iter().filter(|_| !listing.exclude_pinned());
            let unpinned = self.unpinned.iter().filter(|&&n| (n as i32) < older_than);
            let dialogs: Vec<RawDialog> = pinned
                .map(|&n| dialog(n, true))
                .chain(unpinned.take(limit).map(|&n| dialog(n, false)))
                .map(|d| RawDialog {
                    folder_id: listing.folder_id(),
                    ..d
                })
                .collect();
            let messages: Vec<RawMessage> = dialogs
                .iter()
                .map(|d| message(d.top_message.into()))
                .collect();
            let full_page = dialogs.len() >= limit;
            assemble_page(&dialogs, &[], &messages, limit, full_page, listing)
        }

        /// Every page, from the first until there is no `next_cursor`.
        fn page_through(&self, limit: usize) -> Vec<ChatPage> {
            let mut pages = vec![self.list_chats(limit, None)];
            while let Some(next) = pages.last().unwrap().next_cursor.clone() {
                assert!(pages.len() < 100, "paging never ends");
                pages.push(self.list_chats(limit, Some(&next)));
            }
            pages
        }
    }

    #[test]
    fn paging_shows_every_chat_exactly_once_in_order() {
        let pinned_sets: [&[i64]; 4] = [&[], &[5], &[999, 3, 1000], &[5, 999, 3, 998]];
        for archived in [false, true] {
            for pinned in pinned_sets {
                for unpinned in 0..=12 {
                    for limit in 1..=9 {
                        let folder = Folder {
                            archived,
                            pinned: pinned.to_vec(),
                            unpinned: (100..100 + unpinned).rev().collect(),
                        };
                        let case = format!(
                            "archived {archived}, pinned {pinned:?}, {unpinned} unpinned, \
                             limit {limit}"
                        );
                        let pages = folder.page_through(limit);
                        for page in &pages {
                            assert!(page.chats.len() <= limit, "{case}");
                        }
                        let shown: Vec<String> = pages.iter().flat_map(ids).collect();
                        // Pinned chats beyond `limit` are not shown (#37's limitation).
                        let expected: Vec<String> = folder
                            .pinned
                            .iter()
                            .take(limit)
                            .chain(&folder.unpinned)
                            .map(|n| format!("user:{n}"))
                            .collect();
                        assert_eq!(shown, expected, "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn dialogs_of_the_other_folder_are_dropped() {
        let archived = RawDialog {
            folder_id: 1,
            ..dialog(5, false)
        };
        let dialogs = [dialog(9, false), archived, dialog(4, false)];
        let main = assemble_page(&dialogs, &[], &[], 100, false, MAIN_FIRST);
        assert_eq!(ids(&main), ["user:9", "user:4"]);
        let archive = assemble_page(&dialogs, &[], &[], 100, false, ARCHIVE_FIRST);
        assert_eq!(ids(&archive), ["user:5"]);
    }

    #[test]
    fn a_chat_appears_once_per_page() {
        let dialogs = [dialog(1, true), dialog(9, false), dialog(1, false)];
        let page = assemble_page(&dialogs, &[], &[], 100, false, MAIN_FIRST);
        assert_eq!(ids(&page), ["user:1", "user:9"]);
        assert!(page.chats[0].pinned, "the first occurrence wins");
    }

    #[test]
    fn cursors_issued_by_v0_1_1_still_decode() {
        let main = URL_SAFE_NO_PAD.encode(br#"{"d":1600000000,"i":77,"p":"user:9","h":-5}"#);
        assert_eq!(
            Cursor::decode(&main).unwrap(),
            Cursor {
                offset_date: 1_600_000_000,
                offset_id: 77,
                offset_peer: Some("user:9".into()),
                offset_access_hash: Some(-5),
                archived: false,
            }
        );
        let archive = URL_SAFE_NO_PAD.encode(br#"{"d":5,"i":6,"p":"chat:7","a":true}"#);
        assert_eq!(
            Cursor::decode(&archive).unwrap(),
            Cursor {
                offset_date: 5,
                offset_id: 6,
                offset_peer: Some("chat:7".into()),
                offset_access_hash: None,
                archived: true,
            }
        );
    }

    #[test]
    fn foreign_cursors_are_rejected() {
        for bad in [
            "",
            "not-base64!",
            &URL_SAFE_NO_PAD.encode(b"{}"),
            &URL_SAFE_NO_PAD.encode(br#"{"d":1,"i":1,"p":"evil"}"#),
        ] {
            assert!(
                matches!(Cursor::decode(bad), Err(TgError::InvalidInput(_))),
                "accepted {bad:?}"
            );
        }
    }
}
