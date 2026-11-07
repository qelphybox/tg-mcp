//! Chat history as the tools see it, independent of grammers/TL.
//!
//! [`read_history`] decides which `messages.getHistory` calls a query takes
//! ([`HistoryRequest`]s); the adapter sends them and turns each answer into
//! [`RawHistoryMessage`]s and [`RawPeer`]s. Who sent what, the date window,
//! where a window around a message lies and whether there is more on either
//! side are decided here. The fake answers the same requests with a model of
//! Telegram's paging.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatRef, RawPeer, rfc3339},
};

/// What to fetch. Messages come newest first. At most one of `before_id`,
/// `around_id` and `after_id` is set, and `since`/`until` only go with
/// `before_id` (or none of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryQuery {
    pub limit: usize,
    /// Only messages older than this id (the previous page's `next_before_id`).
    pub before_id: Option<i32>,
    /// A window of `limit` messages centred on this one, which it includes.
    pub around_id: Option<i32>,
    /// Only the `limit` messages right after this id (a `next_after_id`).
    pub after_id: Option<i32>,
    /// Only messages at or after this time (unix seconds).
    pub since: Option<i64>,
    /// Only messages at or before this time (unix seconds).
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Sender {
    /// A chat id (`user:N~H` or `channel:N~H`), usable with the other tools.
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
    /// The message an `around_id` window was asked around.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub target: bool,
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
    /// Only for `around_id` and `after_id`: pass back as `after_id` for newer
    /// messages; absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after_id: Option<i32>,
}

/// What a message carries besides its text. Never the media itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Media {
    Photo,
    Video,
    /// A round video message.
    VideoNote,
    Voice,
    Audio,
    Gif,
    Sticker,
    /// Any other file.
    Document,
    Location,
    Contact,
    Poll,
    Dice,
    Story,
    Other,
}

impl Media {
    /// How a message without text shows in get_messages and list_chats.
    pub fn placeholder(self) -> &'static str {
        match self {
            Media::Photo => "[photo]",
            Media::Video => "[video]",
            Media::VideoNote => "[video message]",
            Media::Voice => "[voice]",
            Media::Audio => "[audio]",
            Media::Gif => "[gif]",
            Media::Sticker => "[sticker]",
            Media::Document => "[file]",
            Media::Location => "[location]",
            Media::Contact => "[contact]",
            Media::Poll => "[poll]",
            Media::Dice => "[dice]",
            Media::Story => "[story]",
            Media::Other => "[media]",
        }
    }
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
    /// The text as written: empty for media without a caption.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    #[serde(default)]
    pub reply_to: Option<i32>,
    #[serde(default)]
    pub forwarded: bool,
    #[serde(default)]
    pub edited: bool,
}

/// One `messages.getHistory` call, as its TL parameters (0 = not set, as in TL).
///
/// Telegram slices the Chat's history, newest first, at
/// `position(offset_id) + add_offset`, where `position` counts the messages
/// down to `offset_id` inclusive (so `add_offset: 0` starts strictly below it
/// and a negative `add_offset` takes in that many messages at and above it),
/// keeps `limit` of them and only then drops ids outside `min_id`/`max_id`
/// (core.telegram.org/api/offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryRequest {
    pub offset_id: i32,
    /// Unix seconds; the slice starts strictly before it. Used without `offset_id`.
    pub offset_date: i32,
    pub add_offset: i32,
    pub limit: i32,
    /// Only ids strictly below it.
    pub max_id: i32,
    /// Only ids strictly above it.
    pub min_id: i32,
}

/// What Telegram answered to one [`HistoryRequest`].
#[derive(Debug, Clone, Default)]
pub struct HistoryAnswer {
    /// As Telegram sent them (newest first), `messageEmpty` left out.
    pub messages: Vec<RawHistoryMessage>,
    /// How many messages Telegram sent, `messageEmpty` included: a slice is
    /// full when this reaches its `limit`.
    pub returned: usize,
    pub peers: Vec<RawPeer>,
}

/// Answer `query` with as many `messages.getHistory` calls as it takes (one,
/// or two for a window at the newest message), sent through `get_history`.
pub async fn read_history<F, Fut>(
    chat: ChatRef,
    query: &HistoryQuery,
    mut get_history: F,
) -> Result<MessagePage, TgError>
where
    F: FnMut(HistoryRequest) -> Fut,
    Fut: Future<Output = Result<HistoryAnswer, TgError>>,
{
    let limit = i32::try_from(query.limit)
        .ok()
        .filter(|&l| l >= 1)
        .ok_or_else(|| TgError::InvalidInput("limit is out of range".into()))?;
    let dated = query.since.is_some() || query.until.is_some();
    match (query.before_id, query.around_id, query.after_id) {
        (_, None, None) => {
            let answer = get_history(HistoryRequest {
                offset_id: query.before_id.unwrap_or(0),
                // getHistory returns messages strictly before offset_date.
                offset_date: query
                    .until
                    .map(|until| i32::try_from(until.saturating_add(1)).unwrap_or(i32::MAX))
                    .unwrap_or(0),
                limit,
                ..HistoryRequest::default()
            })
            .await?;
            let full_page = answer.returned >= query.limit;
            Ok(assemble_history(
                chat,
                &answer.messages,
                &answer.peers,
                query,
                full_page,
            ))
        }
        (None, Some(target), None) if !dated => around(chat, target, limit, get_history).await,
        (None, None, Some(after)) if !dated => {
            // Everything at and above after+1, i.e. strictly newer; min_id drops
            // older ones should Telegram fill a slice that reaches past the
            // newest message from below.
            let answer = get_history(HistoryRequest {
                offset_id: after.saturating_add(1),
                add_offset: -limit,
                limit,
                min_id: after,
                ..HistoryRequest::default()
            })
            .await?;
            let window = newest_first(answer.messages, |id| id > after);
            let next_after_id = (answer.returned >= query.limit)
                .then(|| window.first().map(|m| m.id))
                .flatten();
            let issuer = Issuer::new(&answer.peers);
            Ok(MessagePage {
                chat_id: issuer.chat_id(chat),
                messages: window.iter().map(|m| issuer.message(chat, m)).collect(),
                next_before_id: None,
                next_after_id,
            })
        }
        _ => Err(TgError::InvalidInput(
            "before_id, around_id and after_id exclude each other, and since/until don't go \
             with around_id or after_id"
                .into(),
        )),
    }
}

/// `limit` messages around `target`: (limit-1)/2 newer, the rest older, more
/// older ones where there aren't enough newer.
async fn around<F, Fut>(
    chat: ChatRef,
    target: i32,
    limit: i32,
    mut get_history: F,
) -> Result<MessagePage, TgError>
where
    F: FnMut(HistoryRequest) -> Fut,
    Fut: Future<Output = Result<HistoryAnswer, TgError>>,
{
    let size = limit as usize;
    let newer_wanted = (size - 1) / 2;
    let older_wanted = size - 1 - newer_wanted;
    // The target and the newer_wanted messages above it, then older ones.
    let answer = get_history(HistoryRequest {
        offset_id: target,
        add_offset: -(newer_wanted as i32 + 1),
        limit,
        ..HistoryRequest::default()
    })
    .await?;
    let mut peers = answer.peers;
    let mut window = newest_first(answer.messages, |_| true);
    let Some(at) = window.iter().position(|m| m.id == target) else {
        return Err(TgError::InvalidInput(format!(
            "message {target} not found in this chat (deleted, or the id is from another chat)"
        )));
    };
    let newer = at;
    let older = window.len() - at - 1;
    // Fewer newer messages than asked for: the slice reached past the newest one.
    let at_newest = newer < newer_wanted;
    let mut more_older = older >= older_wanted;
    // Telegram may answer such a slice short (it keeps its older end) or full
    // (it moves down); if short, fill it up from below.
    if at_newest && more_older && window.len() < size {
        let oldest = window.last().map_or(target, |m| m.id);
        let missing = size - window.len();
        let fill = get_history(HistoryRequest {
            offset_id: oldest,
            limit: missing as i32,
            ..HistoryRequest::default()
        })
        .await?;
        more_older = fill.returned >= missing;
        window.extend(newest_first(fill.messages, |id| id < oldest));
        peers.extend(fill.peers);
    }
    let issuer = Issuer::new(&peers);
    Ok(MessagePage {
        chat_id: issuer.chat_id(chat),
        messages: window
            .iter()
            .map(|m| ChatMessage {
                target: m.id == target,
                ..issuer.message(chat, m)
            })
            .collect(),
        next_before_id: more_older.then(|| window.last().map(|m| m.id)).flatten(),
        next_after_id: (!at_newest).then(|| window.first().map(|m| m.id)).flatten(),
    })
}

/// Newest first, each id once, only ids that `keep`.
fn newest_first(
    mut messages: Vec<RawHistoryMessage>,
    keep: impl Fn(i32) -> bool,
) -> Vec<RawHistoryMessage> {
    messages.retain(|m| keep(m.id));
    messages.sort_by_key(|m| std::cmp::Reverse(m.id));
    messages.dedup_by_key(|m| m.id);
    messages
}

/// Turns raw messages into the tools' shape, handing out chat ids from the
/// peers Telegram sent along with them.
pub(super) struct Issuer<'a> {
    peers: HashMap<ChatId, &'a RawPeer>,
}

impl<'a> Issuer<'a> {
    pub(super) fn new(peers: &'a [RawPeer]) -> Self {
        Self {
            peers: peers.iter().map(|p| (p.id, p)).collect(),
        }
    }

    pub(super) fn peer(&self, id: ChatId) -> Option<&'a RawPeer> {
        self.peers.get(&id).copied()
    }

    /// Ids handed out carry the access hash Telegram just sent along (none is
    /// invented); `asked`, the chat as the caller named it, keeps the hash it
    /// was asked with if Telegram didn't describe it.
    pub(super) fn issue(&self, id: ChatId, asked: ChatRef) -> ChatRef {
        match self.peers.get(&id).and_then(|p| p.access_hash) {
            Some(hash) => ChatRef::new(id, Some(hash)),
            None if id == asked.chat() => asked,
            None => ChatRef::from(id),
        }
    }

    fn chat_id(&self, chat: ChatRef) -> String {
        self.issue(chat.chat(), chat).to_string()
    }

    /// `m` as a message of `chat`.
    pub(super) fn message(&self, chat: ChatRef, m: &RawHistoryMessage) -> ChatMessage {
        let sender = (!m.out).then(|| {
            let id = m.from.unwrap_or(chat.chat());
            Sender {
                id: self.issue(id, chat).to_string(),
                name: self
                    .peers
                    .get(&id)
                    .map(|p| p.title.clone())
                    .unwrap_or_else(|| id.to_string()),
            }
        });
        let text = match m.media {
            Some(media) if m.text.is_empty() => media.placeholder().to_string(),
            _ => m.text.clone(),
        };
        ChatMessage {
            id: m.id,
            date: rfc3339(m.date),
            sender,
            outgoing: m.out,
            text,
            reply_to: m.reply_to,
            forwarded: m.forwarded,
            edited: m.edited,
            target: false,
        }
    }
}

/// Build a page from what Telegram returned for `query` (newest first).
/// `full_page` = Telegram returned as many messages as asked for.
fn assemble_history(
    chat: ChatRef,
    raw: &[RawHistoryMessage],
    peers: &[RawPeer],
    query: &HistoryQuery,
    full_page: bool,
) -> MessagePage {
    let issuer = Issuer::new(peers);
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
        messages.push(issuer.message(chat, m));
    }
    let next_before_id = (full_page && !reached_since)
        .then(|| raw.last().map(|m| m.id))
        .flatten();
    MessagePage {
        chat_id: issuer.chat_id(chat),
        messages,
        next_before_id,
        next_after_id: None,
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
    use crate::tg::{
        chats::ChatKind,
        fake::{PastNewest, get_history},
    };

    fn msg(id: i32, date: i32) -> RawHistoryMessage {
        RawHistoryMessage {
            id,
            date,
            from: None,
            out: false,
            text: format!("m{id}"),
            media: None,
            reply_to: None,
            forwarded: false,
            edited: false,
        }
    }

    fn query(limit: usize) -> HistoryQuery {
        HistoryQuery {
            limit,
            ..HistoryQuery::default()
        }
    }

    fn peer(id: ChatId, title: &str) -> RawPeer {
        RawPeer {
            id,
            title: title.into(),
            kind: ChatKind::Private,
            username: None,
            access_hash: None,
            is_self: false,
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
            alice.into(),
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
            group.into(),
            &[from_bob, from_ghost],
            &[peer(bob, "Bob")],
            &query(10),
            false,
        );
        assert_eq!(page.messages[0].sender.as_ref().unwrap().name, "Bob");
        assert_eq!(page.messages[1].sender.as_ref().unwrap().name, "user:3");
    }

    #[test]
    fn issued_ids_carry_the_access_hash_telegram_sent_along() {
        let group = ChatId::Channel(9);
        let bob = ChatId::User(2);
        let with_hash = |id, hash| RawPeer {
            access_hash: Some(hash),
            ..peer(id, "x")
        };
        let raw = [
            RawHistoryMessage {
                from: Some(bob),
                ..msg(2, 20)
            },
            RawHistoryMessage {
                from: Some(ChatId::User(3)),
                ..msg(1, 10)
            },
        ];
        let peers = [with_hash(bob, -7), with_hash(group, 99)];

        // Asked with a legacy id: the answer hands out hashed ids.
        let page = assemble_history(group.into(), &raw, &peers, &query(10), false);
        assert_eq!(page.chat_id, ChatRef::new(group, Some(99)).to_string());
        let sender = |i: usize| page.messages[i].sender.as_ref().unwrap().id.clone();
        assert_eq!(sender(0), ChatRef::new(bob, Some(-7)).to_string());
        assert_eq!(sender(1), "user:3", "no hash known, none invented");

        // Telegram didn't describe the chat: its id stays as it was asked.
        let asked = ChatRef::new(group, Some(5));
        let page = assemble_history(asked, &raw, &[], &query(10), false);
        assert_eq!(page.chat_id, asked.to_string());
    }

    #[test]
    fn media_without_text_shows_as_a_placeholder_a_caption_as_itself() {
        let photo = |text: &str| RawHistoryMessage {
            text: text.into(),
            media: Some(Media::Photo),
            ..msg(1, 10)
        };
        let raw = [
            photo(""),
            RawHistoryMessage {
                id: 2,
                ..photo("look")
            },
            RawHistoryMessage {
                id: 3,
                media: Some(Media::VideoNote),
                ..photo("")
            },
            RawHistoryMessage {
                id: 4,
                text: String::new(),
                ..msg(4, 10)
            },
        ];
        let page = assemble_history(ChatId::User(1).into(), &raw, &[], &query(10), false);
        let texts: Vec<&str> = page.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["[photo]", "look", "[video message]", ""]);
    }

    #[test]
    fn next_page_starts_below_the_oldest_message() {
        let chat = ChatId::User(1);
        let raw = [msg(30, 300), msg(20, 200), msg(10, 100)];
        assert_eq!(
            assemble_history(chat.into(), &raw, &[], &query(3), true).next_before_id,
            Some(10)
        );
        assert_eq!(
            assemble_history(chat.into(), &raw, &[], &query(5), false).next_before_id,
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
        let page = assemble_history(chat.into(), &raw, &[], &q, true);
        let ids: Vec<i32> = page.messages.iter().map(|m| m.id).collect();
        assert_eq!(ids, [30, 20]);
        assert_eq!(
            page.next_before_id, None,
            "older pages are all before `since`"
        );
    }

    /// Reads `query` from `history` through the fake's model of getHistory,
    /// recording the requests sent.
    fn read(
        history: &[RawHistoryMessage],
        query: HistoryQuery,
        past_newest: PastNewest,
    ) -> (Result<MessagePage, TgError>, Vec<HistoryRequest>) {
        let mut sent = Vec::new();
        let page = futures_executor(read_history(ChatId::Channel(1).into(), &query, |request| {
            sent.push(request);
            let messages = get_history(history, &request, past_newest);
            std::future::ready(Ok(HistoryAnswer {
                returned: messages.len(),
                messages,
                peers: Vec::new(),
            }))
        }));
        (page, sent)
    }

    /// The futures here never wait: poll once.
    fn futures_executor<T>(future: impl Future<Output = T>) -> T {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(future).poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => unreachable!("fake history never waits"),
        }
    }

    fn page_ids(page: &MessagePage) -> Vec<i32> {
        page.messages.iter().map(|m| m.id).collect()
    }

    fn around(target: i32, limit: usize) -> HistoryQuery {
        HistoryQuery {
            around_id: Some(target),
            ..query(limit)
        }
    }

    /// Histories with gaps of every size: deleted messages in a channel, or a
    /// private chat's ids, which count all of the account's chats.
    fn gapped_histories() -> Vec<Vec<RawHistoryMessage>> {
        let mut seed: u64 = 0x5EED;
        let mut next = move |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % bound
        };
        let mut histories = vec![vec![msg(1, 1)], (1..=12).map(|i| msg(i, i)).collect()];
        for _ in 0..30 {
            let len = 1 + next(40) as i32;
            let mut id = 0;
            histories.push(
                (0..len)
                    .map(|_| {
                        id += 1 + next(4) as i32 * next(3) as i32;
                        msg(id, id)
                    })
                    .collect(),
            );
        }
        histories
    }

    #[test]
    fn a_window_is_the_stretch_of_history_around_its_target() {
        for past_newest in [PastNewest::Shorten, PastNewest::ShiftOlder] {
            for history in gapped_histories() {
                // Newest first, as the window must read.
                let all: Vec<i32> = history.iter().rev().map(|m| m.id).collect();
                for (i, &target) in all.iter().enumerate() {
                    for limit in 1..=9 {
                        let (page, _) = read(&history, around(target, limit), past_newest);
                        let page = page.unwrap();
                        let got = page_ids(&page);
                        // Up to (limit-1)/2 newer, the rest older, more older ones
                        // where the newer run out.
                        let newer = ((limit - 1) / 2).min(i);
                        let older = (limit - 1 - newer).min(all.len() - 1 - i);
                        let expected = &all[i - newer..=i + older];
                        let case = format!("{past_newest:?} {all:?} around {target} limit {limit}");
                        assert_eq!(got, expected, "{case}");
                        let flagged: Vec<i32> = page
                            .messages
                            .iter()
                            .filter(|m| m.target)
                            .map(|m| m.id)
                            .collect();
                        assert_eq!(flagged, [target], "{case}");
                        // A cursor on a side unless that side reached the end
                        // of the history before filling up.
                        let newer_full = newer == (limit - 1) / 2;
                        assert_eq!(page.next_after_id.is_some(), newer_full, "{case}");
                        assert_eq!(
                            page.next_before_id.is_some(),
                            older == limit - 1 - newer,
                            "{case}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_window_costs_one_call_unless_telegram_shortens_it_at_the_newest_message() {
        let history: Vec<RawHistoryMessage> = (1..=20).map(|i| msg(i, i)).collect();
        // Mid-history: exactly the "around" example of the offsets page.
        let (_, sent) = read(&history, around(10, 5), PastNewest::Shorten);
        assert_eq!(
            sent,
            [HistoryRequest {
                offset_id: 10,
                add_offset: -3,
                limit: 5,
                ..HistoryRequest::default()
            }]
        );
        let (_, sent) = read(&history, around(19, 5), PastNewest::ShiftOlder);
        assert_eq!(sent.len(), 1);
        // Shortened at the newest message (20, 19, 18, 17): one more page
        // below it for what is missing.
        let (page, sent) = read(&history, around(19, 5), PastNewest::Shorten);
        assert_eq!(page_ids(&page.unwrap()), [20, 19, 18, 17, 16]);
        assert_eq!(
            sent[1..],
            [HistoryRequest {
                offset_id: 17,
                limit: 1,
                ..HistoryRequest::default()
            }]
        );
    }

    #[test]
    fn after_id_reads_the_messages_right_after_it() {
        let history: Vec<RawHistoryMessage> = [1, 2, 5, 6, 9, 11, 12].map(|i| msg(i, i)).into();
        let after = |id, limit| HistoryQuery {
            after_id: Some(id),
            ..query(limit)
        };
        for past_newest in [PastNewest::Shorten, PastNewest::ShiftOlder] {
            let (page, _) = read(&history, after(2, 3), past_newest);
            let page = page.unwrap();
            assert_eq!(page_ids(&page), [9, 6, 5]);
            assert_eq!(page.next_after_id, Some(9));
            assert_eq!(page.next_before_id, None);
            // Near the newest: only newer ones, and no cursor.
            let (page, _) = read(&history, after(6, 5), past_newest);
            let page = page.unwrap();
            assert_eq!(page_ids(&page), [12, 11, 9], "{past_newest:?}");
            assert_eq!(page.next_after_id, None);
            let (page, _) = read(&history, after(12, 5), past_newest);
            assert!(page.unwrap().messages.is_empty());
        }
    }

    #[test]
    fn a_missing_target_is_an_error_not_an_empty_window() {
        let history: Vec<RawHistoryMessage> = [1, 2, 5, 6].map(|i| msg(i, i)).into();
        for target in [3, 4, 7, 1000] {
            let (page, _) = read(&history, around(target, 5), PastNewest::Shorten);
            let err = page.unwrap_err().to_string();
            assert!(
                err.contains(&format!("message {target} not found")),
                "{err}"
            );
        }
    }

    #[test]
    fn positions_exclude_each_other() {
        let history = [msg(1, 1)];
        for bad in [
            HistoryQuery {
                before_id: Some(2),
                ..around(1, 3)
            },
            HistoryQuery {
                after_id: Some(1),
                ..around(1, 3)
            },
            HistoryQuery {
                since: Some(0),
                ..around(1, 3)
            },
            HistoryQuery {
                until: Some(5),
                after_id: Some(1),
                ..query(3)
            },
            around(1, 0),
        ] {
            let (page, sent) = read(&history, bad, PastNewest::Shorten);
            assert!(
                matches!(page, Err(TgError::InvalidInput(_))),
                "{bad:?}: {page:?}"
            );
            assert!(sent.is_empty(), "nothing is asked of Telegram");
        }
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
