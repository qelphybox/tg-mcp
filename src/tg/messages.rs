//! Chat history as the tools see it, independent of grammers/TL.
//!
//! The adapter turns a `messages.getHistory` answer into [`RawHistoryMessage`]s
//! and [`RawPeer`]s; who sent what, the date window and whether there is a next
//! page are decided here. The fake uses the same assembly.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::chats::{ChatId, RawPeer, rfc3339};

/// What to fetch. Messages come newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryQuery {
    pub limit: usize,
    /// Only messages older than this id (the previous page's `next_before_id`).
    pub before_id: Option<i32>,
    /// Only messages at or after this time (unix seconds).
    pub since: Option<i64>,
    /// Only messages at or before this time (unix seconds).
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Sender {
    /// A chat id (`user:N` or `channel:N`), usable with the other tools.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatMessage {
    pub id: i32,
    /// UTC, RFC 3339.
    pub date: String,
    /// Absent when the connected account sent it (`outgoing: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<Sender>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outgoing: bool,
    /// The text; media and service messages as `[photo]`, `[voice]`, ….
    pub text: String,
    /// Id of the message this one replies to, in the same chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessagePage {
    pub chat_id: String,
    /// Newest first.
    pub messages: Vec<ChatMessage>,
    /// Pass back as `before_id` for older messages; absent when there are none
    /// (or the `since` bound was reached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_id: Option<i32>,
}

/// One message as extracted from TL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawHistoryMessage {
    pub id: i32,
    /// Unix seconds.
    pub date: i32,
    /// `from_id`; Telegram leaves it out for the other side of a private chat
    /// and for channel posts.
    #[serde(default)]
    pub from: Option<ChatId>,
    #[serde(default)]
    pub out: bool,
    pub text: String,
    #[serde(default)]
    pub reply_to: Option<i32>,
    #[serde(default)]
    pub forwarded: bool,
    #[serde(default)]
    pub edited: bool,
}

/// Build a page from what Telegram returned for `query` (newest first).
/// `full_page` = Telegram returned as many messages as asked for.
pub fn assemble_history(
    chat: ChatId,
    raw: &[RawHistoryMessage],
    peers: &[RawPeer],
    query: &HistoryQuery,
    full_page: bool,
) -> MessagePage {
    let names: HashMap<ChatId, &str> = peers.iter().map(|p| (p.id, p.title.as_str())).collect();
    let mut reached_since = false;
    let mut messages = Vec::new();
    for m in raw {
        let date = i64::from(m.date);
        if query.until.is_some_and(|until| date > until) {
            continue;
        }
        if query.since.is_some_and(|since| date < since) {
            reached_since = true;
            break;
        }
        let sender = (!m.out).then(|| {
            let id = m.from.unwrap_or(chat);
            Sender {
                id: id.to_string(),
                name: names
                    .get(&id)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| id.to_string()),
            }
        });
        messages.push(ChatMessage {
            id: m.id,
            date: rfc3339(m.date),
            sender,
            outgoing: m.out,
            text: m.text.clone(),
            reply_to: m.reply_to,
            forwarded: m.forwarded,
            edited: m.edited,
        });
    }
    let next_before_id = (full_page && !reached_since)
        .then(|| raw.last().map(|m| m.id))
        .flatten();
    MessagePage {
        chat_id: chat.to_string(),
        messages,
        next_before_id,
    }
}

/// Parse a `since`/`until` bound: RFC 3339, or a plain `YYYY-MM-DD` (midnight UTC;
/// for `until` the end of that day).
pub fn parse_bound(s: &str, end_of_day: bool) -> Option<i64> {
    use time::{
        Date, OffsetDateTime, format_description::well_known::Rfc3339, macros::format_description,
    };
    if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
        return Some(t.unix_timestamp());
    }
    let date = Date::parse(s, format_description!("[year]-[month]-[day]")).ok()?;
    let midnight = date.midnight().assume_utc().unix_timestamp();
    Some(if end_of_day {
        midnight + 86_399
    } else {
        midnight
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::chats::ChatKind;

    fn msg(id: i32, date: i32) -> RawHistoryMessage {
        RawHistoryMessage {
            id,
            date,
            from: None,
            out: false,
            text: format!("m{id}"),
            reply_to: None,
            forwarded: false,
            edited: false,
        }
    }

    fn query(limit: usize) -> HistoryQuery {
        HistoryQuery {
            limit,
            before_id: None,
            since: None,
            until: None,
        }
    }

    fn peer(id: ChatId, title: &str) -> RawPeer {
        RawPeer {
            id,
            title: title.into(),
            kind: ChatKind::Private,
            username: None,
            access_hash: None,
        }
    }

    #[test]
    fn senders_follow_telegram_rules() {
        let alice = ChatId::User(1);
        let group = ChatId::Channel(9);
        let bob = ChatId::User(2);

        // Private chat: no from_id means the other side; out means us.
        let mine = RawHistoryMessage {
            out: true,
            ..msg(2, 100)
        };
        let page = assemble_history(
            alice,
            &[mine, msg(1, 90)],
            &[peer(alice, "Alice")],
            &query(10),
            false,
        );
        assert_eq!(page.messages[0].sender, None);
        assert!(page.messages[0].outgoing);
        assert_eq!(
            page.messages[1].sender,
            Some(Sender {
                id: "user:1".into(),
                name: "Alice".into()
            })
        );

        // Supergroup: from_id names the member; unknown members fall back to their id.
        let from_bob = RawHistoryMessage {
            from: Some(bob),
            ..msg(5, 50)
        };
        let from_ghost = RawHistoryMessage {
            from: Some(ChatId::User(3)),
            ..msg(4, 40)
        };
        let page = assemble_history(
            group,
            &[from_bob, from_ghost],
            &[peer(bob, "Bob")],
            &query(10),
            false,
        );
        assert_eq!(page.messages[0].sender.as_ref().unwrap().name, "Bob");
        assert_eq!(page.messages[1].sender.as_ref().unwrap().name, "user:3");
    }

    #[test]
    fn next_page_starts_below_the_oldest_message() {
        let chat = ChatId::User(1);
        let raw = [msg(30, 300), msg(20, 200), msg(10, 100)];
        assert_eq!(
            assemble_history(chat, &raw, &[], &query(3), true).next_before_id,
            Some(10)
        );
        assert_eq!(
            assemble_history(chat, &raw, &[], &query(5), false).next_before_id,
            None
        );
    }

    #[test]
    fn date_window_filters_and_since_ends_paging() {
        let chat = ChatId::User(1);
        let raw = [msg(40, 400), msg(30, 300), msg(20, 200), msg(10, 100)];
        let q = HistoryQuery {
            since: Some(200),
            until: Some(300),
            ..query(4)
        };
        let page = assemble_history(chat, &raw, &[], &q, true);
        let ids: Vec<i32> = page.messages.iter().map(|m| m.id).collect();
        assert_eq!(ids, [30, 20]);
        assert_eq!(
            page.next_before_id, None,
            "older pages are all before `since`"
        );
    }

    #[test]
    fn bounds_accept_rfc3339_and_plain_dates() {
        assert_eq!(
            parse_bound("2026-11-06T12:00:00Z", false),
            Some(1_793_966_400)
        );
        assert_eq!(
            parse_bound("2026-11-06T15:00:00+03:00", false),
            Some(1_793_966_400)
        );
        assert_eq!(parse_bound("2026-11-06", false), Some(1_793_923_200));
        assert_eq!(
            parse_bound("2026-11-06", true),
            Some(1_793_923_200 + 86_399)
        );
        assert_eq!(parse_bound("yesterday", false), None);
    }
}
