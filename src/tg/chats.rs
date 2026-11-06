//! Chats as the tools see them, independent of grammers/TL.
//!
//! The Telegram adapter extracts plain [`RawDialog`]s, [`RawPeer`]s and
//! [`RawMessage`]s from a `messages.getDialogs` answer; everything that needs
//! thinking (pairing, titles, kinds, the pagination cursor) happens here.

use std::{collections::HashMap, fmt, str::FromStr};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::TgError;

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
    /// The text, shortened; media and service messages as `[photo]`, `[voice]`, ….
    pub text: String,
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
}

/// Normalise what Telegram returns: empty descriptions are none, counts are
/// only shown for groups and channels.
pub fn chat_info(
    peer: &RawPeer,
    description: Option<String>,
    member_count: Option<i32>,
) -> ChatInfo {
    let description = description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
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
        description,
        member_count,
    }
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
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDialog {
    pub chat: ChatId,
    pub top_message: i32,
    pub unread_count: i32,
    pub pinned: bool,
}

/// Where the next `messages.getDialogs` page starts: the last dialog of this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(rename = "d")]
    pub offset_date: i32,
    #[serde(rename = "i")]
    pub offset_id: i32,
    #[serde(rename = "p")]
    pub offset_peer: String,
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
        cursor
            .offset_peer
            .parse::<ChatId>()
            .map_err(|_| invalid())?;
        Ok(cursor)
    }
}

/// Assemble a page. `full_page` = Telegram returned as many dialogs as asked for,
/// so there may be more.
pub fn assemble_page(
    dialogs: &[RawDialog],
    peers: &[RawPeer],
    messages: &[RawMessage],
    full_page: bool,
    archived: bool,
) -> ChatPage {
    let peers: HashMap<ChatId, &RawPeer> = peers.iter().map(|p| (p.id, p)).collect();
    let messages: HashMap<(ChatId, i32), &RawMessage> =
        messages.iter().map(|m| ((m.chat, m.id), m)).collect();

    let chats = dialogs
        .iter()
        .map(|dialog| {
            let peer = peers.get(&dialog.chat);
            let top = messages.get(&(dialog.chat, dialog.top_message));
            ChatSummary {
                id: ChatRef::new(dialog.chat, peer.and_then(|p| p.access_hash)).to_string(),
                title: peer
                    .map(|p| p.title.clone())
                    .unwrap_or_else(|| "Unknown chat".into()),
                kind: peer.map(|p| p.kind).unwrap_or(match dialog.chat {
                    ChatId::User(_) => ChatKind::Private,
                    ChatId::Chat(_) => ChatKind::Group,
                    ChatId::Channel(_) => ChatKind::Channel,
                }),
                username: peer.and_then(|p| p.username.clone()),
                is_self: peer.is_some_and(|p| p.is_self),
                unread_count: dialog.unread_count,
                pinned: dialog.pinned,
                last_message: top.map(|m| MessagePreview {
                    date: rfc3339(m.date),
                    text: m.text.clone(),
                }),
            }
        })
        .collect();

    let next_cursor = dialogs.last().filter(|_| full_page).map(|last| {
        Cursor {
            offset_date: messages
                .get(&(last.chat, last.top_message))
                .map(|m| m.date)
                .unwrap_or(0),
            offset_id: last.top_message,
            offset_peer: last.chat.to_string(),
            offset_access_hash: peers.get(&last.chat).and_then(|p| p.access_hash),
            archived,
        }
        .encode()
    });

    ChatPage { chats, next_cursor }
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
            },
            RawDialog {
                chat: ChatId::User(7),
                top_message: 2,
                unread_count: 0,
                pinned: false,
            },
        ];
        let other = peer(ChatId::User(7), "Bob", ChatKind::Private, Some(2));
        let page = assemble_page(&dialogs, &[me.clone(), other], &[], false, false);
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
            },
            RawDialog {
                chat: alice,
                top_message: 55,
                unread_count: 0,
                pinned: false,
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
            },
            RawMessage {
                chat: alice,
                id: 55,
                date: 1_700_000_000,
                text: "hi".into(),
            },
            RawMessage {
                chat: team,
                id: 900,
                date: 1_700_000_100,
                text: "[photo]".into(),
            },
        ];

        let page = assemble_page(&dialogs, &peers, &messages, false, false);
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
        assert_eq!(page.chats[0].last_message.as_ref().unwrap().text, "[photo]");
        assert_eq!(page.chats[1].title, "Alice Smith");
        assert_eq!(
            page.chats[1].last_message,
            Some(MessagePreview {
                date: "2023-11-14T22:13:20Z".into(),
                text: "hi".into()
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
        }];
        let peers = [peer(bob, "Bob", ChatKind::Bot, Some(-5))];
        let messages = [RawMessage {
            chat: bob,
            id: 77,
            date: 1_600_000_000,
            text: "x".into(),
        }];

        let page = assemble_page(&dialogs, &peers, &messages, true, true);
        let cursor = Cursor::decode(page.next_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(
            cursor,
            Cursor {
                offset_date: 1_600_000_000,
                offset_id: 77,
                offset_peer: "user:9".into(),
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
        }];
        let messages = [RawMessage {
            chat: news,
            id: 55,
            date: 1,
            text: "someone else's message".into(),
        }];
        let page = assemble_page(&dialogs, &[], &messages, false, false);
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
        }];
        let page = assemble_page(&dialogs, &[], &[], false, false);
        assert_eq!(page.chats[0].title, "Unknown chat");
        assert_eq!(page.chats[0].kind, ChatKind::Group);
        assert_eq!(page.chats[0].last_message, None);
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
