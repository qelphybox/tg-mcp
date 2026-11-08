//! Drafts as the tools see them, independent of grammers/TL (#43).
//!
//! `get_drafts` reads every Draft of the Account (`messages.getAllDrafts`,
//! answered as one `updateDraftMessage` per Chat or Topic with a Draft);
//! the adapters extract [`RawDraft`]s and the peers the answer describes,
//! and [`assemble`] turns them into the tools' shape. `save_draft` takes a
//! [`NewDraft`] whose text is already formatted
//! ([`super::compose::prepare`]) and puts it where
//! [`super::compose::place`] says.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    chats::{ChatId, ChatKind, ChatRef, RawPeer, fallback_kind, rfc3339},
    markdown::FormattedText,
    topics::GENERAL_TOPIC_ID,
};

/// The Chat a Draft is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DraftChat {
    /// A chat id, as list_chats gives it: pass it to save_draft or get_messages.
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChatKind,
    /// The account's own chat (Saved Messages).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
}

/// One Draft: unsent text in a Chat's input field (or a forum Topic's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Draft {
    pub chat: DraftChat,
    /// Only in a forum: the Topic whose input field holds it (General is 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<i32>,
    /// The message the Draft replies to, in the same chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
    /// When the Draft was last changed, UTC, RFC 3339.
    pub date: String,
    /// The Draft's text without its formatting, possibly empty (a Draft may
    /// be only a reply); text with nothing visible reads `[no visible text]`.
    pub text: String,
}

/// The Account's Drafts, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Drafts {
    pub drafts: Vec<Draft>,
    /// More Drafts exist than are listed: the oldest are left out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// A Draft as Telegram describes it (`updateDraftMessage` + `draftMessage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawDraft {
    pub chat: ChatId,
    /// `top_msg_id`: the forum Topic it is in; `None` in General or outside forums.
    #[serde(default)]
    pub topic: Option<i32>,
    /// The message it replies to in the same Chat, not a Topic's top message.
    #[serde(default)]
    pub reply_to: Option<i32>,
    pub text: String,
    /// Unix seconds.
    pub date: i32,
}

/// What `messages.getAllDrafts` answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftsAnswer {
    pub drafts: Vec<RawDraft>,
    /// The users and Chats the answer describes.
    pub peers: Vec<RawPeer>,
    /// Which of the described Chats are forums.
    pub forums: Vec<ChatId>,
}

/// The tools' Drafts from Telegram's answer: newest first, each Chat named by
/// an id with its access hash (when the answer gave one) and titled; a
/// forum's Draft names its Topic. Uncut: the tool bounds the list.
pub fn assemble(answer: DraftsAnswer) -> Drafts {
    let DraftsAnswer {
        mut drafts,
        peers,
        forums,
    } = answer;
    drafts.sort_by_key(|d| std::cmp::Reverse(d.date));
    let drafts = drafts
        .into_iter()
        .map(|raw| {
            let peer = peers.iter().find(|p| p.id == raw.chat);
            let id = peer.map_or_else(|| ChatRef::from(raw.chat), RawPeer::chat_ref);
            Draft {
                chat: DraftChat {
                    id: id.to_string(),
                    title: peer.map_or_else(|| "Unknown chat".into(), |p| p.title.clone()),
                    kind: peer.map_or_else(|| fallback_kind(raw.chat), |p| p.kind),
                    is_self: peer.is_some_and(|p| p.is_self),
                },
                topic: raw
                    .topic
                    .or_else(|| forums.contains(&raw.chat).then_some(GENERAL_TOPIC_ID)),
                reply_to: raw.reply_to,
                date: rfc3339(raw.date),
                text: raw.text,
            }
        })
        .collect();
    Drafts {
        drafts,
        truncated: false,
    }
}

/// A Draft to save: its text already formatted and checked, empty to clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDraft {
    pub chat: ChatRef,
    pub text: FormattedText,
    pub reply_to: Option<i32>,
    pub topic: Option<i32>,
}

/// Where a Draft was saved or cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedDraft {
    pub chat: ChatId,
    /// Only in a forum: the Topic whose input field it is in (General is 1).
    pub topic: Option<i32>,
    pub reply_to: Option<i32>,
    /// The text was empty: the Draft there is gone.
    pub cleared: bool,
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

    fn raw(chat: ChatId, date: i32, text: &str) -> RawDraft {
        RawDraft {
            chat,
            topic: None,
            reply_to: None,
            text: text.into(),
            date,
        }
    }

    #[test]
    fn drafts_come_newest_first_with_issued_ids_titles_and_topics() {
        let (alice, team, forum, gone) = (
            ChatId::User(1),
            ChatId::Chat(2),
            ChatId::Channel(3),
            ChatId::Channel(4),
        );
        let answer = DraftsAnswer {
            drafts: vec![
                raw(alice, 100, "older"),
                RawDraft {
                    reply_to: Some(7),
                    ..raw(team, 300, "newest")
                },
                RawDraft {
                    topic: Some(10),
                    ..raw(forum, 200, "in a topic")
                },
                raw(forum, 150, "in general"),
                raw(gone, 50, "unknown"),
            ],
            peers: vec![
                RawPeer {
                    is_self: true,
                    ..peer(alice, "Alice", ChatKind::Private, Some(-5))
                },
                peer(team, "Team", ChatKind::Group, None),
                peer(forum, "Forum", ChatKind::Supergroup, Some(9)),
            ],
            forums: vec![forum],
        };
        let drafts = assemble(answer).drafts;
        let texts: Vec<&str> = drafts.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(
            texts,
            ["newest", "in a topic", "in general", "older", "unknown"]
        );
        assert_eq!(drafts[0].chat.id, "chat:2");
        assert_eq!(drafts[0].reply_to, Some(7));
        assert_eq!(drafts[0].topic, None);
        assert_eq!(drafts[1].chat.id, ChatRef::new(forum, Some(9)).to_string());
        assert_eq!(drafts[1].topic, Some(10));
        assert_eq!(
            drafts[2].topic,
            Some(1),
            "a forum's Draft without a Topic is in General"
        );
        assert!(drafts[3].chat.is_self);
        assert_eq!(drafts[3].chat.id, ChatRef::new(alice, Some(-5)).to_string());
        assert_eq!(drafts[3].date, "1970-01-01T00:01:40Z");
        assert_eq!(drafts[4].chat.title, "Unknown chat");
        assert_eq!(drafts[4].chat.kind, ChatKind::Channel);
        assert_eq!(drafts[4].chat.id, "channel:4");
    }
}
