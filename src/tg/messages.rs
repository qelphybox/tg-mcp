//! Chat history as the tools see it, independent of grammers/TL.
//!
//! [`read_history`] decides which `messages.getHistory` calls a query takes
//! ([`HistoryRequest`]s), or for a reply chain which messages to fetch by id
//! ([`MessageRequest`]s); the adapter sends them and turns each answer into
//! [`RawHistoryMessage`]s and [`RawPeer`]s. Who sent what, the date window,
//! where a window around a message lies, whether there is more on either
//! side and where a reply chain ends are decided here. The fake answers the
//! same requests with a model of Telegram.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatRef, RawPeer, rfc3339},
    sanitise::is_blank,
    search::RawFoundMessage,
};

/// What to fetch. Messages come newest first. At most one of `before_id`,
/// `around_id`, `after_id` and `thread_of` is set, and `since`/`until` only go
/// with `before_id` (or none of them).
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
    /// The reply chain of this message: it, the message it replies to, that
    /// one's parent, …, at most `limit` messages in all.
    pub thread_of: Option<i32>,
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
    /// The text; media and service messages as `[photo]`, `[voice]`, …; a
    /// Rich message as plain lines (`src/tg/rich.rs`); text with nothing
    /// visible (only hidden characters) as `[no visible text]`.
    pub text: String,
    /// Id of the message this one replies to, in the same chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// The message an `around_id` window or a `thread_of` chain was asked for.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub target: bool,
    /// Only in a `thread_of` chain: its root, a message that replies to nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thread_root: bool,
}

/// A message in some Chat, as the tools name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessageLink {
    /// A chat id as the other tools take it.
    pub chat_id: String,
    pub message_id: i32,
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
    /// Only for `thread_of`: the oldest message of the chain replies to this
    /// message in another chat, where the chain goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues_in: Option<MessageLink>,
    /// Only for `thread_of`: the oldest message of the chain replies to this
    /// id, which is deleted or can't be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken_at: Option<i32>,
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

/// A message's text as get_messages and list_chats show it: media whose
/// caption has nothing visible (none, blanks or only hidden characters) shows
/// as its placeholder. Text-only messages are left to the sanitiser, which
/// marks the ones with nothing visible `[no visible text]`.
pub fn shown_text(text: String, media: Option<Media>) -> String {
    match media {
        Some(media) if is_blank(&text) => media.placeholder().to_string(),
        _ => text,
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
    /// The text as written, then its Rich message flattened
    /// ([`super::rich::message_text`]): empty for media without a caption.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    /// The message it replies to, in the same Chat.
    #[serde(default)]
    pub reply_to: Option<i32>,
    /// Instead of `reply_to`: the message it replies to is in another Chat
    /// (`reply_to_peer_id`, e.g. a comment reply forwarded to `@replies`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_elsewhere: Option<MessageAt>,
    #[serde(default)]
    pub forwarded: bool,
    /// A forwarded message's original author (`fwd_from.from_id`); absent
    /// when the author hides their account, or for a message not forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from: Option<ChatId>,
    #[serde(default)]
    pub edited: bool,
}

/// A message by its Chat and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAt {
    pub chat: ChatId,
    pub id: i32,
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

/// One `messages.getMessages` call (`channels.getMessages` in a channel) for
/// one message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRequest {
    pub id: i32,
}

/// What Telegram answered to one [`MessageRequest`].
#[derive(Debug, Clone, Default)]
pub struct MessageAnswer {
    /// Each with the Chat it is in (its `peer_id`), `messageEmpty` left out.
    /// Outside channels ids count across all of the Account's private chats
    /// and basic groups, so a message here may be another Chat's.
    pub messages: Vec<RawFoundMessage>,
    pub peers: Vec<RawPeer>,
}

/// Access hashes of users and channels seen in earlier answers (the Live
/// client's peer cache, #24). An id the answers at hand leave without a usable
/// hash (a peer described as "min", or not at all) takes it from here.
pub type KnownHashes<'a> = &'a (dyn Fn(ChatId) -> Option<i64> + Sync);

/// Answer `query` with as many `messages.getHistory` calls as it takes (one,
/// or two for a window at the newest message), sent through `get_history`;
/// a `thread_of` chain with one `get_message` per message of the chain.
pub async fn read_history<F, Fut, G, GFut>(
    chat: ChatRef,
    query: &HistoryQuery,
    known: KnownHashes<'_>,
    mut get_history: F,
    get_message: G,
) -> Result<MessagePage, TgError>
where
    F: FnMut(HistoryRequest) -> Fut,
    Fut: Future<Output = Result<HistoryAnswer, TgError>>,
    G: FnMut(MessageRequest) -> GFut,
    GFut: Future<Output = Result<MessageAnswer, TgError>>,
{
    let limit = i32::try_from(query.limit)
        .ok()
        .filter(|&l| l >= 1)
        .ok_or_else(|| TgError::InvalidInput("limit is out of range".into()))?;
    let dated = query.since.is_some() || query.until.is_some();
    if let Some(start) = query.thread_of {
        let positioned =
            query.before_id.is_some() || query.around_id.is_some() || query.after_id.is_some();
        if positioned || dated {
            return Err(TgError::InvalidInput(
                "thread_of doesn't combine with before_id, around_id, after_id, since or until"
                    .into(),
            ));
        }
        return read_thread(chat, start, query.limit, known, get_message).await;
    }
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
                known,
                query,
                full_page,
            ))
        }
        (None, Some(target), None) if !dated => {
            around(chat, target, limit, known, get_history).await
        }
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
            let issuer = Issuer::new(&answer.peers, known);
            Ok(MessagePage {
                next_after_id,
                ..MessagePage::new(
                    issuer.chat_id(chat),
                    window.iter().map(|m| issuer.message(chat, m)).collect(),
                )
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
    known: KnownHashes<'_>,
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
    // Telegram may answer such a slice short (it keeps its older end, as in
    // production) or full (it moves down). If short, the window is the
    // newest `limit` messages: read them again as a plain page from the top,
    // whose answer describes the senders in full where the first one gave
    // them as min (#51).
    if at_newest && more_older && window.len() < size {
        let top = get_history(HistoryRequest {
            limit,
            ..HistoryRequest::default()
        })
        .await?;
        let top_window = newest_first(top.messages, |_| true);
        // Unless so many messages arrived in between that the target left
        // the top: then the first answer is the window.
        if top_window.iter().any(|m| m.id == target) {
            more_older = top.returned >= size;
            window = top_window;
        }
        peers.extend(top.peers);
    }
    let issuer = Issuer::new(&peers, known);
    Ok(MessagePage {
        next_before_id: more_older.then(|| window.last().map(|m| m.id)).flatten(),
        next_after_id: (!at_newest).then(|| window.first().map(|m| m.id)).flatten(),
        ..MessagePage::new(
            issuer.chat_id(chat),
            window
                .iter()
                .map(|m| ChatMessage {
                    target: m.id == target,
                    ..issuer.message(chat, m)
                })
                .collect(),
        )
    })
}

/// The reply chain of `start`, newest first: `start` and its ancestors, at
/// most `limit` messages, fetched one by one (each parent's id is only known
/// once its child is). The walk ends at the root (a message that replies to
/// nothing), at `limit`, at a parent in another Chat (`continues_in`) or at a
/// parent that is deleted or can't be read (`broken_at`). `start` itself
/// missing from this Chat is an error.
async fn read_thread<G, GFut>(
    chat: ChatRef,
    start: i32,
    limit: usize,
    known: KnownHashes<'_>,
    mut get_message: G,
) -> Result<MessagePage, TgError>
where
    G: FnMut(MessageRequest) -> GFut,
    GFut: Future<Output = Result<MessageAnswer, TgError>>,
{
    let mut peers = Vec::new();
    let mut chain: Vec<RawHistoryMessage> = Vec::new();
    let (mut root, mut broken_at, mut elsewhere) = (false, None, None);
    let mut next = start;
    loop {
        let answer = get_message(MessageRequest { id: next }).await?;
        peers.extend(answer.peers);
        // Outside channels the id may name another Chat's message: not ours.
        let Some(found) = answer
            .messages
            .into_iter()
            .find(|f| f.chat == chat.chat() && f.message.id == next)
        else {
            if chain.is_empty() {
                return Err(TgError::InvalidInput(format!(
                    "message {start} not found in this chat (deleted, or the id is from another \
                     chat)"
                )));
            }
            broken_at = Some(next);
            break;
        };
        let message = found.message;
        let parent = message.reply_to;
        let other_chat = message.reply_elsewhere.filter(|at| at.chat != chat.chat());
        chain.push(message);
        if let Some(at) = other_chat {
            elsewhere = Some(at);
            break;
        }
        match parent {
            None => {
                root = true;
                break;
            }
            Some(_) if chain.len() >= limit => break,
            // A reply always points to an older message, so this can't
            // happen; if it did, the link can't be followed.
            Some(parent) if chain.iter().any(|m| m.id == parent) => {
                broken_at = Some(parent);
                break;
            }
            Some(parent) => next = parent,
        }
    }
    let issuer = Issuer::new(&peers, known);
    let last = chain.len() - 1;
    let messages = chain
        .iter()
        .enumerate()
        .map(|(i, m)| ChatMessage {
            target: i == 0,
            thread_root: root && i == last,
            ..issuer.message(chat, m)
        })
        .collect();
    Ok(MessagePage {
        continues_in: elsewhere.map(|at| MessageLink {
            chat_id: issuer.issue(at.chat, chat).to_string(),
            message_id: at.id,
        }),
        broken_at,
        ..MessagePage::new(issuer.chat_id(chat), messages)
    })
}

impl MessagePage {
    /// A page of `messages` with no cursors and no chain ends.
    pub fn new(chat_id: String, messages: Vec<ChatMessage>) -> Self {
        Self {
            chat_id,
            messages,
            next_before_id: None,
            next_after_id: None,
            continues_in: None,
            broken_at: None,
        }
    }
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
/// peers Telegram sent along with them, else from the hashes seen before.
pub(super) struct Issuer<'a> {
    peers: HashMap<ChatId, &'a RawPeer>,
    known: KnownHashes<'a>,
    /// The Account, as the answers describe it (its user has the `self`
    /// flag, and is described wherever a message names it).
    me: Option<ChatId>,
}

impl<'a> Issuer<'a> {
    /// `peers` may come from several answers and describe a peer more than
    /// once: a description with a usable hash wins over one without (min).
    pub(super) fn new(peers: &'a [RawPeer], known: KnownHashes<'a>) -> Self {
        let mut by_id: HashMap<ChatId, &'a RawPeer> = HashMap::new();
        for peer in peers {
            let kept = by_id.entry(peer.id).or_insert(peer);
            if kept.access_hash.is_none() && peer.access_hash.is_some() {
                *kept = peer;
            }
        }
        Self {
            me: peers.iter().find(|p| p.is_self).map(|p| p.id),
            peers: by_id,
            known,
        }
    }

    pub(super) fn peer(&self, id: ChatId) -> Option<&'a RawPeer> {
        self.peers.get(&id).copied()
    }

    /// Ids handed out carry the access hash Telegram just sent along, else
    /// the one seen before (none is invented); `asked`, the chat as the
    /// caller named it, keeps the hash it was asked with if neither has one.
    pub(super) fn issue(&self, id: ChatId, asked: ChatRef) -> ChatRef {
        let hash = self
            .peers
            .get(&id)
            .and_then(|p| p.access_hash)
            .or_else(|| (self.known)(id));
        match hash {
            Some(hash) => ChatRef::new(id, Some(hash)),
            None if id == asked.chat() => asked,
            None => ChatRef::from(id),
        }
    }

    fn chat_id(&self, chat: ChatRef) -> String {
        self.issue(chat.chat(), chat).to_string()
    }

    /// `m` as a message of `chat`. Telegram names the sender in `from_id`,
    /// else it is the other side of a private chat. The Account's own
    /// messages are outgoing: those with `out`, and those naming the Account
    /// as sender, which is how Telegram sends them in Saved Messages (#52).
    /// There, a message forwarded from someone else is theirs, as Telegram's
    /// apps show it: its sender is the original author, and it is not
    /// outgoing. An author who hides their account has no id to name, so
    /// that forward stays the Account's (outgoing), as does one forwarded
    /// from the Account itself. Elsewhere a forward is the forwarder's.
    pub(super) fn message(&self, chat: ChatRef, m: &RawHistoryMessage) -> ChatMessage {
        let me = self.me;
        let original_author = m
            .forwarded_from
            .filter(|&author| Some(chat.chat()) == me && Some(author) != me);
        let from = original_author.unwrap_or(m.from.unwrap_or(chat.chat()));
        let outgoing = original_author.is_none() && (m.out || Some(from) == me);
        let sender = (!outgoing).then(|| Sender {
            id: self.issue(from, chat).to_string(),
            name: self
                .peers
                .get(&from)
                .map(|p| p.title.clone())
                .unwrap_or_else(|| from.to_string()),
        });
        let text = shown_text(m.text.clone(), m.media);
        ChatMessage {
            id: m.id,
            date: rfc3339(m.date),
            sender,
            outgoing,
            text,
            reply_to: m.reply_to,
            forwarded: m.forwarded,
            edited: m.edited,
            target: false,
            thread_root: false,
        }
    }
}

/// Build a page from what Telegram returned for `query` (newest first).
/// `full_page` = Telegram returned as many messages as asked for.
fn assemble_history(
    chat: ChatRef,
    raw: &[RawHistoryMessage],
    peers: &[RawPeer],
    known: KnownHashes<'_>,
    query: &HistoryQuery,
    full_page: bool,
) -> MessagePage {
    let issuer = Issuer::new(peers, known);
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
        next_before_id,
        ..MessagePage::new(issuer.chat_id(chat), messages)
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
        fake::{PastNewest, get_history, get_message},
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
            reply_elsewhere: None,
            forwarded: false,
            forwarded_from: None,
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
            &unknown,
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
            &unknown,
            &query(10),
            false,
        );
        assert_eq!(page.messages[0].sender.as_ref().unwrap().name, "Bob");
        assert_eq!(page.messages[1].sender.as_ref().unwrap().name, "user:3");
    }

    /// The Account as Telegram describes it: its user, with the `self` flag.
    fn me_peer(me: ChatId) -> RawPeer {
        RawPeer {
            is_self: true,
            ..peer(me, "Me")
        }
    }

    /// (sender name, outgoing, forwarded) of each message.
    fn authors(page: &MessagePage) -> Vec<(Option<&str>, bool, bool)> {
        page.messages
            .iter()
            .map(|m| {
                (
                    m.sender.as_ref().map(|s| s.name.as_str()),
                    m.outgoing,
                    m.forwarded,
                )
            })
            .collect()
    }

    #[test]
    fn the_accounts_own_messages_are_outgoing_even_without_the_out_flag() {
        let me = ChatId::User(42);
        let alice = ChatId::User(1);
        let news = ChatId::Channel(9);
        let forwarded = |id, from: Option<ChatId>, original: Option<ChatId>| RawHistoryMessage {
            from,
            forwarded: true,
            forwarded_from: original,
            ..msg(id, id)
        };
        let peers = [me_peer(me), peer(alice, "Alice"), peer(news, "News")];

        // Saved Messages: Telegram names the Account as sender (from_id, or
        // only peer_id) and leaves `out` unset.
        let saved = [
            RawHistoryMessage {
                from: Some(me),
                ..msg(6, 6)
            },
            msg(5, 5),
            forwarded(4, Some(me), Some(me)),
            forwarded(3, Some(me), Some(alice)),
            forwarded(2, Some(me), Some(news)),
            // The original author hides their account: no one else to name.
            forwarded(1, Some(me), None),
        ];
        let page = assemble_history(me.into(), &saved, &peers, &unknown, &query(10), false);
        assert_eq!(
            authors(&page),
            [
                (None, true, false),
                (None, true, false),
                (None, true, true),
                (Some("Alice"), false, true),
                (Some("News"), false, true),
                (None, true, true),
            ]
        );
        assert_eq!(
            page.messages[3].sender.as_ref().unwrap().id,
            "user:1",
            "the original author's id, usable with the other tools"
        );

        // Anywhere else the Account named as sender is outgoing too, and a
        // forward's sender is whoever forwarded it.
        let group = [
            RawHistoryMessage {
                from: Some(me),
                ..msg(3, 3)
            },
            forwarded(2, Some(me), Some(alice)),
            forwarded(1, Some(alice), Some(news)),
        ];
        let team = ChatId::Channel(8);
        let page = assemble_history(team.into(), &group, &peers, &unknown, &query(10), false);
        assert_eq!(
            authors(&page),
            [
                (None, true, false),
                (None, true, true),
                (Some("Alice"), false, true),
            ]
        );
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
        let page = assemble_history(group.into(), &raw, &peers, &unknown, &query(10), false);
        assert_eq!(page.chat_id, ChatRef::new(group, Some(99)).to_string());
        let sender = |i: usize| page.messages[i].sender.as_ref().unwrap().id.clone();
        assert_eq!(sender(0), ChatRef::new(bob, Some(-7)).to_string());
        assert_eq!(sender(1), "user:3", "no hash known, none invented");

        // Telegram didn't describe the chat: its id stays as it was asked.
        let asked = ChatRef::new(group, Some(5));
        let page = assemble_history(asked, &raw, &[], &unknown, &query(10), false);
        assert_eq!(page.chat_id, asked.to_string());
    }

    /// No hash seen before: what a cold Live client knows.
    fn unknown(_: ChatId) -> Option<i64> {
        None
    }

    #[test]
    fn ids_the_answer_leaves_without_a_hash_take_the_one_seen_before() {
        let group = ChatId::Channel(9);
        let (bob, carol, dave) = (ChatId::User(2), ChatId::User(3), ChatId::User(4));
        // Seen in earlier answers: Bob, Dave, the group, and a channel with
        // Carol's number, which is another peer (its hash isn't Carol's).
        let known = |id: ChatId| match id {
            ChatId::User(2) => Some(-7),
            ChatId::User(4) => Some(1),
            ChatId::Channel(3) => Some(11),
            ChatId::Channel(9) => Some(99),
            _ => None,
        };
        let from = |id, who| RawHistoryMessage {
            from: Some(who),
            ..msg(id, id)
        };
        let raw = [from(3, bob), from(2, carol), from(1, dave)];
        // Bob described as min (no usable hash), Carol not at all, Dave with
        // the hash this answer gives.
        let peers = [
            peer(bob, "Bob"),
            RawPeer {
                access_hash: Some(5),
                ..peer(dave, "Dave")
            },
        ];
        let page = assemble_history(group.into(), &raw, &peers, &known, &query(10), false);
        assert_eq!(page.chat_id, ChatRef::new(group, Some(99)).to_string());
        let senders: Vec<&str> = page
            .messages
            .iter()
            .map(|m| m.sender.as_ref().unwrap().id.as_str())
            .collect();
        assert_eq!(
            senders,
            [
                ChatRef::new(bob, Some(-7)).to_string(),
                "user:3".into(),
                ChatRef::new(dave, Some(5)).to_string(),
            ]
        );
    }

    /// How one answer describes Bob: with his access hash, as "min" (the
    /// adapter leaves its hash out) or not at all.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Described {
        Hashed,
        Min,
        Not,
    }

    const BOB: ChatId = ChatId::User(2);
    const BOB_HASH: i64 = -7;

    fn describing_bob(how: Described) -> Vec<RawPeer> {
        let bob = peer(BOB, "Bob");
        match how {
            Described::Hashed => vec![RawPeer {
                access_hash: Some(BOB_HASH),
                ..bob
            }],
            Described::Min => vec![bob],
            Described::Not => Vec::new(),
        }
    }

    /// Every way two answers can describe Bob, and the id he gets from them:
    /// with his hash if either answer gives it, whichever of them that is.
    fn two_answers() -> Vec<([Described; 2], String)> {
        use Described::*;
        let all = [Hashed, Min, Not];
        all.iter()
            .flat_map(|&a| all.iter().map(move |&b| [a, b]))
            .map(|answers| {
                let hash = answers.contains(&Hashed).then_some(BOB_HASH);
                (answers, ChatRef::new(BOB, hash).to_string())
            })
            .collect()
    }

    fn sender_of(page: &MessagePage, id: i32) -> String {
        let message = page.messages.iter().find(|m| m.id == id).unwrap();
        message.sender.as_ref().unwrap().id.clone()
    }

    #[test]
    fn a_window_merged_from_two_answers_issues_ids_from_both() {
        // Bob wrote the newest message; Telegram shortens the window around it
        // (6, 5) and the second call reads the top again (6, 5, 4).
        let history: Vec<RawHistoryMessage> = (1..=6)
            .map(|id| RawHistoryMessage {
                from: (id == 6).then_some(BOB),
                ..msg(id, id)
            })
            .collect();
        for (answers, expected) in two_answers() {
            let mut calls = 0;
            let page = futures_executor(read_history(
                ChatId::Channel(1).into(),
                &around(6, 3),
                &unknown,
                |request| {
                    let messages = get_history(&history, &request, PastNewest::Shorten);
                    calls += 1;
                    std::future::ready(Ok(HistoryAnswer {
                        returned: messages.len(),
                        messages,
                        peers: describing_bob(answers[calls - 1]),
                    }))
                },
                |_| -> std::future::Ready<Result<MessageAnswer, TgError>> {
                    unreachable!("a window fetches no message by id")
                },
            ))
            .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(sender_of(&page, 6), expected, "{answers:?}");
        }
    }

    #[test]
    fn a_chain_merged_from_its_answers_issues_ids_from_all_of_them() {
        // Bob wrote 3, which replies to 2; each is a getMessages call.
        let histories: Histories = [(
            GROUP,
            vec![
                RawHistoryMessage {
                    from: Some(BOB),
                    ..reply(3, 2)
                },
                msg(2, 2),
            ],
        )]
        .into();
        for (answers, expected) in two_answers() {
            let mut calls = 0;
            let page = futures_executor(read_history(
                GROUP.into(),
                &thread(3, 5),
                &unknown,
                |_| -> std::future::Ready<Result<HistoryAnswer, TgError>> {
                    unreachable!("a chain reads no history")
                },
                |request| {
                    calls += 1;
                    std::future::ready(Ok(MessageAnswer {
                        messages: get_message(&histories, GROUP, &request),
                        peers: describing_bob(answers[calls - 1]),
                    }))
                },
            ))
            .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(sender_of(&page, 3), expected, "{answers:?}");
        }
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
            // A caption of nothing but hidden characters or blanks is none.
            RawHistoryMessage {
                id: 5,
                ..photo("\u{2063}\u{200B}")
            },
            RawHistoryMessage {
                id: 6,
                ..photo(" \n")
            },
            // Without media the sanitiser decides (`[no visible text]`).
            RawHistoryMessage {
                id: 7,
                text: "\u{2063}".into(),
                ..msg(7, 10)
            },
        ];
        let page = assemble_history(
            ChatId::User(1).into(),
            &raw,
            &[],
            &unknown,
            &query(10),
            false,
        );
        let texts: Vec<&str> = page.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "[photo]",
                "look",
                "[video message]",
                "",
                "[photo]",
                "[photo]",
                "\u{2063}"
            ]
        );
    }

    #[test]
    fn next_page_starts_below_the_oldest_message() {
        let chat = ChatId::User(1);
        let raw = [msg(30, 300), msg(20, 200), msg(10, 100)];
        assert_eq!(
            assemble_history(chat.into(), &raw, &[], &unknown, &query(3), true).next_before_id,
            Some(10)
        );
        assert_eq!(
            assemble_history(chat.into(), &raw, &[], &unknown, &query(5), false).next_before_id,
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
        let page = assemble_history(chat.into(), &raw, &[], &unknown, &q, true);
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
        let page = futures_executor(read_history(
            ChatId::Channel(1).into(),
            &query,
            &unknown,
            |request| {
                sent.push(request);
                let messages = get_history(history, &request, past_newest);
                std::future::ready(Ok(HistoryAnswer {
                    returned: messages.len(),
                    messages,
                    peers: Vec::new(),
                }))
            },
            |_| -> std::future::Ready<Result<MessageAnswer, TgError>> {
                unreachable!("history pages fetch no message by id")
            },
        ));
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
        // Shortened at the newest message (20, 19, 18, 17): the window is
        // the newest 5, read again as a plain page from the top, whose
        // answer describes its senders in full (#51).
        let (page, sent) = read(&history, around(19, 5), PastNewest::Shorten);
        assert_eq!(page_ids(&page.unwrap()), [20, 19, 18, 17, 16]);
        assert_eq!(
            sent[1..],
            [HistoryRequest {
                limit: 5,
                ..HistoryRequest::default()
            }]
        );
    }

    #[test]
    fn a_window_whose_target_left_the_top_in_between_keeps_the_first_answer() {
        // Between the two calls so many messages arrived that the target is
        // no longer among the newest: the shortened first answer is the window.
        let history: Vec<RawHistoryMessage> = (1..=20).map(|i| msg(i, i)).collect();
        let arrived: Vec<RawHistoryMessage> = (1..=30).map(|i| msg(i, i)).collect();
        let mut calls = 0;
        let page = futures_executor(read_history(
            ChatId::Channel(1).into(),
            &around(19, 5),
            &unknown,
            |request| {
                calls += 1;
                let now = if calls == 1 { &history } else { &arrived };
                let messages = get_history(now, &request, PastNewest::Shorten);
                std::future::ready(Ok(HistoryAnswer {
                    returned: messages.len(),
                    messages,
                    peers: Vec::new(),
                }))
            },
            |_| -> std::future::Ready<Result<MessageAnswer, TgError>> {
                unreachable!("a window fetches no message by id")
            },
        ))
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(page_ids(&page), [20, 19, 18, 17]);
        assert_eq!(page.next_before_id, Some(17));
        assert_eq!(page.next_after_id, None);
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
            HistoryQuery {
                thread_of: Some(1),
                ..around(1, 3)
            },
            HistoryQuery {
                before_id: Some(2),
                ..thread(1, 3)
            },
            HistoryQuery {
                after_id: Some(1),
                ..thread(1, 3)
            },
            HistoryQuery {
                since: Some(0),
                ..thread(1, 3)
            },
            HistoryQuery {
                until: Some(5),
                ..thread(1, 3)
            },
            thread(1, 0),
        ] {
            let (page, sent) = read(&history, bad, PastNewest::Shorten);
            assert!(
                matches!(page, Err(TgError::InvalidInput(_))),
                "{bad:?}: {page:?}"
            );
            assert!(sent.is_empty(), "nothing is asked of Telegram");
        }
    }

    fn thread(start: i32, limit: usize) -> HistoryQuery {
        HistoryQuery {
            thread_of: Some(start),
            ..query(limit)
        }
    }

    fn reply(id: i32, to: i32) -> RawHistoryMessage {
        RawHistoryMessage {
            reply_to: Some(to),
            ..msg(id, id)
        }
    }

    type Histories = HashMap<ChatId, Vec<RawHistoryMessage>>;

    /// Reads the chain of `start` in `chat` through the fake's model of
    /// getMessages, recording the requests sent.
    fn read_chain(
        histories: &Histories,
        chat: ChatId,
        start: i32,
        limit: usize,
    ) -> (Result<MessagePage, TgError>, Vec<MessageRequest>) {
        let mut sent = Vec::new();
        let page = futures_executor(read_history(
            chat.into(),
            &thread(start, limit),
            &unknown,
            |_| -> std::future::Ready<Result<HistoryAnswer, TgError>> {
                unreachable!("a chain reads no history")
            },
            |request| {
                sent.push(request);
                std::future::ready(Ok(MessageAnswer {
                    messages: get_message(histories, chat, &request),
                    peers: Vec::new(),
                }))
            },
        ));
        (page, sent)
    }

    fn flagged(page: &MessagePage, flag: fn(&ChatMessage) -> bool) -> Vec<i32> {
        page.messages
            .iter()
            .filter(|m| flag(m))
            .map(|m| m.id)
            .collect()
    }

    const GROUP: ChatId = ChatId::Channel(1);

    /// A busy supergroup: a 60-hop chain (each `10 * k` replies to
    /// `10 * (k - 1)`, 10 is the root) between unrelated messages.
    fn busy_group() -> Histories {
        let mut history: Vec<RawHistoryMessage> = (1..=600)
            .filter(|id| id % 10 != 0)
            .map(|id| msg(id, id))
            .collect();
        history.push(msg(10, 10));
        history.extend((2..=60).map(|k| reply(10 * k, 10 * (k - 1))));
        HashMap::from([(GROUP, history)])
    }

    #[test]
    fn a_chain_is_the_message_and_its_ancestors_newest_first() {
        let histories = busy_group();
        for start in [10, 20, 50, 300, 600] {
            for limit in 1..=8 {
                let case = format!("thread_of {start}, {limit} messages");
                let (page, sent) = read_chain(&histories, GROUP, start, limit);
                let page = page.unwrap();
                let ancestors = (start / 10 - 1) as usize;
                let expected: Vec<i32> = (0..=ancestors.min(limit - 1) as i32)
                    .map(|k| start - 10 * k)
                    .collect();
                assert_eq!(page_ids(&page), expected, "{case}");
                // One getMessages per message of the chain, none beyond it.
                let asked: Vec<i32> = sent.iter().map(|r| r.id).collect();
                assert_eq!(asked, expected, "{case}");
                assert_eq!(flagged(&page, |m| m.target), [start], "{case}");
                let reached_root = ancestors < limit;
                let roots: &[i32] = if reached_root { &[10] } else { &[] };
                assert_eq!(flagged(&page, |m| m.thread_root), roots, "{case}");
                // A chain cut by the cap says where it goes on: its oldest
                // message still names its parent.
                let oldest = page.messages.last().unwrap();
                assert_eq!(oldest.reply_to.is_some(), !reached_root, "{case}");
                assert_eq!(page.broken_at, None, "{case}");
                assert_eq!(page.continues_in, None, "{case}");
                assert_eq!(
                    (page.next_before_id, page.next_after_id),
                    (None, None),
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn a_deleted_parent_ends_the_chain_and_is_named() {
        let mut histories = busy_group();
        // 300's parent, 290, is deleted: messageEmpty.
        histories.get_mut(&GROUP).unwrap().retain(|m| m.id != 290);
        let (page, sent) = read_chain(&histories, GROUP, 320, 10);
        let page = page.unwrap();
        assert_eq!(page_ids(&page), [320, 310, 300]);
        assert_eq!(page.broken_at, Some(290));
        assert!(flagged(&page, |m| m.thread_root).is_empty());
        assert_eq!(page.messages[2].reply_to, Some(290), "the link is shown");
        assert_eq!(sent.len(), 4, "the deleted parent was asked for once");

        // The message itself replying to a deleted one: just it.
        let (page, _) = read_chain(&histories, GROUP, 300, 10);
        let page = page.unwrap();
        assert_eq!(page_ids(&page), [300]);
        assert_eq!(page.broken_at, Some(290));
    }

    #[test]
    fn a_parent_in_another_chat_ends_the_chain_and_says_where() {
        let other = ChatId::Channel(9);
        let mut histories = busy_group();
        let history = histories.get_mut(&GROUP).unwrap();
        let at = history.iter().position(|m| m.id == 30).unwrap();
        // 30 replies to message 7 of another chat (reply_to_peer_id).
        history[at] = RawHistoryMessage {
            reply_elsewhere: Some(MessageAt { chat: other, id: 7 }),
            ..msg(30, 30)
        };
        histories.insert(other, vec![msg(7, 7)]);
        let peers = [RawPeer {
            access_hash: Some(-5),
            ..peer(other, "News")
        }];
        let mut sent = Vec::new();
        let page = futures_executor(read_history(
            GROUP.into(),
            &thread(50, 20),
            &unknown,
            |_| -> std::future::Ready<Result<HistoryAnswer, TgError>> { unreachable!() },
            |request| {
                sent.push(request.id);
                std::future::ready(Ok(MessageAnswer {
                    messages: get_message(&histories, GROUP, &request),
                    peers: peers.to_vec(),
                }))
            },
        ))
        .unwrap();
        assert_eq!(page_ids(&page), [50, 40, 30]);
        assert_eq!(sent, [50, 40, 30], "nothing is fetched in the other chat");
        assert_eq!(
            page.continues_in,
            Some(MessageLink {
                chat_id: ChatRef::new(other, Some(-5)).to_string(),
                message_id: 7,
            })
        );
        assert_eq!(page.broken_at, None);
        assert!(flagged(&page, |m| m.thread_root).is_empty());
        assert_eq!(page.messages[2].reply_to, None, "reply_to is this chat's");
    }

    #[test]
    fn outside_channels_an_id_from_another_chat_is_not_this_chats_message() {
        // messages.getMessages takes no peer: ids count across all private
        // chats and basic groups, so asking for Bob's id finds Bob's message.
        let (alice, bob) = (ChatId::User(1), ChatId::User(2));
        let histories = HashMap::from([
            (alice, vec![msg(3, 3), reply(9, 3)]),
            (bob, vec![msg(5, 5), reply(8, 5)]),
        ]);
        for missing in [5, 8, 100] {
            let (page, sent) = read_chain(&histories, alice, missing, 5);
            let err = page.unwrap_err().to_string();
            assert!(
                err.contains(&format!("message {missing} not found in this chat")),
                "{err}"
            );
            assert_eq!(sent.len(), 1);
        }
        let (page, _) = read_chain(&histories, alice, 9, 5);
        let page = page.unwrap();
        assert_eq!(page_ids(&page), [9, 3]);
        assert_eq!(flagged(&page, |m| m.thread_root), [3]);
        let (page, _) = read_chain(&histories, ChatId::Chat(4), 8, 5);
        assert!(page.is_err(), "a basic group is no different");
    }

    #[test]
    fn a_loop_ends_the_walk_without_repeating_a_message() {
        // Telegram can't produce this (replies point to older messages).
        let histories = HashMap::from([(GROUP, vec![reply(5, 9), reply(9, 7), reply(7, 9)])]);
        let (page, sent) = read_chain(&histories, GROUP, 5, 50);
        let page = page.unwrap();
        assert_eq!(page_ids(&page), [5, 9, 7]);
        assert_eq!(page.broken_at, Some(9));
        assert_eq!(sent.len(), 3);
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
