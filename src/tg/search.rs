//! Search as the tools see it, independent of grammers/TL (ADR-0004: Telegram
//! searches, we keep no index).
//!
//! [`search`] turns a [`SearchQuery`] and its cursor into one
//! `messages.searchGlobal` (no Chat) or `messages.search` (one Chat) call, a
//! [`SearchRequest`]; the adapter sends it and turns the answer into a
//! [`SearchAnswer`]. Which offsets the next page starts from, whether there is
//! one, and the ids handed out are decided here. The fake answers the same
//! requests with a model of Telegram's search paging.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatKind, ChatRef, RawPeer, fallback_kind},
    messages::{
        Issuer, KnownHashes, MAX_LINK_CHARS, Media, MediaSummary, RawHistoryMessage, Reactions,
        Sender,
    },
    topics::{RawForums, RawTopic, SearchTopics},
};

/// Which kind of Chats a search across all of them looks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    /// One-to-one chats with people and bots.
    Private,
    /// Groups and supergroups.
    Groups,
    Channels,
}

/// Only messages with this kind of attachment (Telegram's search filters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaFilter {
    Photo,
    Video,
    /// Files.
    Document,
    /// Messages with a link in them.
    Link,
    Voice,
    /// Round video messages.
    VideoNote,
    Gif,
}

/// Whose messages a search within one Chat keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFrom {
    /// The connected Account.
    Me,
    /// A user, or a channel posting as itself.
    Peer(ChatRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// All of the Account's Chats. `archived`: `None` both the main list and the
    /// Archive, `Some(true)` the Archive only, `Some(false)` the main list only.
    Global {
        chat_type: Option<ChatType>,
        archived: Option<bool>,
    },
    /// One Chat; `topic`: one Topic of a forum (the adapter has checked the
    /// Chat is one).
    Chat {
        chat: ChatRef,
        from: Option<SearchFrom>,
        topic: Option<i32>,
    },
}

/// What to search for. The tool has checked the combination already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub scope: SearchScope,
    /// The words; may be empty with `media` or `from`.
    pub text: String,
    pub media: Option<MediaFilter>,
    /// Only messages at or after this time (unix seconds).
    pub since: Option<i64>,
    /// Only messages at or before this time (unix seconds).
    pub until: Option<i64>,
    pub limit: usize,
    /// A `next_cursor` of the same search.
    pub cursor: Option<String>,
}

// ── Output ──────────────────────────────────────────────────────────────────

/// The Chat a Search hit is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HitChat {
    /// A chat id, as list_chats gives it: pass it to get_messages.
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChatKind,
}

/// One Search hit: a get_messages message with its Chat, and a snippet
/// instead of the whole text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub chat: HitChat,
    /// The message id: get_messages `around_id` reads it in context.
    pub id: i32,
    /// UTC, RFC 3339.
    pub date: String,
    /// Absent when the connected account sent it (`outgoing: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<Sender>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outgoing: bool,
    /// Up to 300 characters of the text around the first word that matches the
    /// query (the start of the text if none matches literally); empty for media
    /// without a caption, `[no visible text]` for text with nothing visible.
    pub snippet: String,
    /// Only the kind of media the message carries; get_messages describes it
    /// in full. Absent without media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaSummary>,
    /// Id of the message this one replies to, in the same chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// As in get_messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Reactions>,
    /// As in get_messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_LINK_CHARS))]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// As in get_messages: the Topic of a forum message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<i32>,
}

/// A page of Search hits, newest first. No total: Telegram's isn't reliable
/// under filters (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// Pass back as `cursor`, with the same query and filters, for the next
    /// page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Only with hits in forums: the title of each Topic they are in, by
    /// the hit's `chat.id`, then by Topic id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<SearchTopics>,
}

// ── What goes to Telegram and comes back ────────────────────────────────────

/// Where a request searches, with its offsets (0 / `None` = not set, as in TL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestScope {
    /// `messages.searchGlobal`: results across Chats ordered by Telegram's
    /// "rate", a page starting strictly after (`offset_rate`, `offset_peer`,
    /// `offset_id`), the position of the previous page's last message.
    Global {
        chat_type: Option<ChatType>,
        /// Telegram's peer folder: 0 the main list, 1 the Archive, `None` both.
        folder_id: Option<i32>,
        offset_rate: i32,
        offset_peer: Option<ChatRef>,
        offset_id: i32,
    },
    /// `messages.search`: one Chat, newest first, ids strictly below
    /// `offset_id` (`add_offset` 0); `topic`: only that Topic (`top_msg_id`).
    Chat {
        chat: ChatRef,
        from: Option<SearchFrom>,
        topic: Option<i32>,
        offset_id: i32,
    },
}

/// One search call, as its TL parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub q: String,
    pub filter: Option<MediaFilter>,
    /// Unix seconds; only messages strictly after it (0: no bound).
    pub min_date: i32,
    /// Unix seconds; only messages strictly before it (0: no bound).
    pub max_date: i32,
    pub limit: i32,
    pub scope: RequestScope,
}

/// A message a search found, and the Chat it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFoundMessage {
    pub chat: ChatId,
    pub message: RawHistoryMessage,
}

/// What Telegram answered to one [`SearchRequest`].
#[derive(Debug, Clone, Default)]
pub struct SearchAnswer {
    /// In Telegram's order, `messageEmpty` left out.
    pub messages: Vec<RawFoundMessage>,
    /// How many messages Telegram sent, `messageEmpty` included.
    pub returned: usize,
    /// `messages.messages`: this is every result, there is no next page.
    /// (`messages.messagesSlice` and `messages.channelMessages` are not.)
    pub complete: bool,
    /// `messages.messagesSlice.next_rate`: the next global page's `offset_rate`.
    pub next_rate: Option<i32>,
    /// The answer's `users` and `chats`: they may not describe every Chat or
    /// sender, and min peers come without a usable access hash.
    pub peers: Vec<RawPeer>,
    /// Which of the answer's Chats are forums, and the Topics it describes.
    pub forums: RawForums,
}

/// Telegram's error for a search without a query where it needs one
/// (`SEARCH_QUERY_EMPTY`), as the tools report it.
pub const QUERY_REQUIRED: &str = "Telegram refuses an empty query for this search: a query is \
required here. Add one or more words.";

/// What a reused cursor that belongs to another search says.
pub const CURSOR_MISMATCH: &str = "cursor belongs to a different search: the query or a filter \
changed. Start again without cursor, or repeat the search exactly as before.";

/// Answer `query` with one search call, sent through `send`; ids the answer
/// leaves without a hash take the one in `known`.
pub async fn search<F, Fut>(
    query: &SearchQuery,
    known: KnownHashes<'_>,
    send: F,
) -> Result<SearchPage, TgError>
where
    F: FnOnce(SearchRequest) -> Fut,
    Fut: Future<Output = Result<SearchAnswer, TgError>>,
{
    let limit = i32::try_from(query.limit)
        .ok()
        .filter(|l| (1..=100).contains(l))
        .ok_or_else(|| TgError::InvalidInput("limit is out of range".into()))?;
    let fingerprint = query.fingerprint();
    let cursor = query
        .cursor
        .as_deref()
        .map(Cursor::decode)
        .transpose()?
        .unwrap_or(Cursor {
            fingerprint,
            offset_rate: None,
            offset_peer: None,
            offset_id: 0,
        });
    if cursor.fingerprint != fingerprint {
        return Err(TgError::InvalidInput(CURSOR_MISMATCH.into()));
    }
    let offset_peer = cursor
        .offset_peer
        .as_deref()
        .map(str::parse::<ChatRef>)
        .transpose()
        .map_err(|_| Cursor::invalid())?;
    let scope = match query.scope {
        SearchScope::Global {
            chat_type,
            archived,
        } => RequestScope::Global {
            chat_type,
            folder_id: archived.map(i32::from),
            offset_rate: cursor.offset_rate.unwrap_or(0),
            offset_peer,
            offset_id: cursor.offset_id,
        },
        SearchScope::Chat { chat, from, topic } => RequestScope::Chat {
            chat,
            from,
            topic,
            offset_id: cursor.offset_id,
        },
    };
    // Telegram's bounds are exclusive, ours inclusive.
    let date = |bound: Option<i64>, step: i64| {
        bound.map_or(0, |b| {
            i32::try_from(b.saturating_add(step).max(1)).unwrap_or(i32::MAX)
        })
    };
    let answer = send(SearchRequest {
        q: query.text.trim().to_string(),
        filter: query.media,
        min_date: date(query.since, -1),
        max_date: date(query.until, 1),
        limit,
        scope,
    })
    .await?;
    Ok(assemble(query, answer, known, limit as usize, fingerprint))
}

/// A page of at most `limit` hits from `answer`, and where the next one starts.
fn assemble(
    query: &SearchQuery,
    answer: SearchAnswer,
    known: KnownHashes<'_>,
    limit: usize,
    fingerprint: u64,
) -> SearchPage {
    let SearchAnswer {
        mut messages,
        returned,
        complete,
        next_rate,
        peers,
        forums,
    } = answer;
    let issuer = Issuer::new(&peers, known).in_forums(&forums);
    // Only the chat that was asked keeps the hash it was asked with.
    let asked = |chat: ChatId| match query.scope {
        SearchScope::Chat { chat: asked, .. } if asked.chat() == chat => asked,
        _ => ChatRef::from(chat),
    };
    // More than asked for is cut here, and the cursor continues after the cut.
    let cut = messages.len() > limit;
    messages.truncate(limit);
    let more = !complete && (cut || returned >= limit);
    let next_cursor = messages.last().filter(|_| more).map(|last| {
        let global = matches!(query.scope, SearchScope::Global { .. });
        Cursor {
            fingerprint,
            // next_rate belongs to the last message Telegram sent; without it
            // (or after a cut) the docs say: the date of the last message.
            offset_rate: global.then(|| next_rate.filter(|_| !cut).unwrap_or(last.message.date)),
            offset_peer: global.then(|| issuer.issue(last.chat, asked(last.chat)).to_string()),
            offset_id: last.message.id,
        }
        .encode()
    });
    let within = |date: i32| {
        let date = i64::from(date);
        query.since.is_none_or(|s| date >= s) && query.until.is_none_or(|u| date <= u)
    };
    let hits: Vec<SearchHit> = messages
        .iter()
        .filter(|found| within(found.message.date))
        .map(|found| {
            let chat = asked(found.chat);
            let message = issuer.message(chat, &found.message);
            let peer = issuer.peer(found.chat);
            SearchHit {
                chat: HitChat {
                    id: issuer.issue(found.chat, chat).to_string(),
                    title: peer.map_or_else(|| "Unknown chat".into(), |p| p.title.clone()),
                    kind: peer.map_or_else(|| fallback_kind(found.chat), |p| p.kind),
                },
                id: message.id,
                date: message.date,
                sender: message.sender,
                outgoing: message.outgoing,
                snippet: found.message.text.clone(),
                media: message.media.as_ref().map(Media::summary),
                reply_to: message.reply_to,
                forwarded: message.forwarded,
                edited: message.edited,
                reactions: message.reactions,
                link: message.link,
                pinned: message.pinned,
                topic: message.topic,
            }
        })
        .collect();
    SearchPage {
        topics: search_titles(&hits, &forums.topics),
        hits,
        next_cursor,
    }
}

/// A search page's `topics`: for each Chat whose hits are in Topics, the
/// titles `known` has of those Topics; `None` when no hit is in a Topic.
fn search_titles(hits: &[SearchHit], known: &[RawTopic]) -> Option<SearchTopics> {
    let mut topics = SearchTopics::default();
    for hit in hits {
        let Some(topic) = hit.topic else { continue };
        let titles = topics.0.entry(hit.chat.id.clone()).or_default();
        let chat = hit.chat.id.parse::<ChatRef>().ok().map(|c| c.chat());
        if let Some(t) = known.iter().find(|t| Some(t.chat) == chat && t.id == topic) {
            titles.0.insert(topic, t.title.clone());
        }
    }
    (!topics.0.is_empty()).then_some(topics)
}

impl SearchQuery {
    /// What a cursor must have been issued for: the scope, the words and every
    /// filter. Stable across releases (FNV-1a), so cursors survive a deploy.
    fn fingerprint(&self) -> u64 {
        let scope = match self.scope {
            SearchScope::Global {
                chat_type,
                archived,
            } => format!("global {chat_type:?} {archived:?}"),
            SearchScope::Chat { chat, from, topic } => {
                let from = match from {
                    None => String::new(),
                    Some(SearchFrom::Me) => "me".into(),
                    Some(SearchFrom::Peer(peer)) => peer.chat().to_string(),
                };
                // Without a Topic, as before Topics: cursors survive the deploy.
                let topic = topic.map_or_else(String::new, |t| format!(" topic {t}"));
                format!("chat {} {from}{topic}", chat.chat())
            }
        };
        let canonical = format!(
            "{scope}\0{}\0{:?}\0{:?}\0{:?}",
            self.text.trim(),
            self.media,
            self.since,
            self.until
        );
        canonical.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    }
}

/// Where the next page starts, and which search it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    #[serde(rename = "f")]
    fingerprint: u64,
    /// Global search only.
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    offset_rate: Option<i32>,
    /// Global search only: an issued chat id.
    #[serde(rename = "p", default, skip_serializing_if = "Option::is_none")]
    offset_peer: Option<String>,
    #[serde(rename = "i")]
    offset_id: i32,
}

/// A cursor of `query` continuing at these offsets, as `search` would issue it.
#[cfg(test)]
pub(crate) fn cursor_for(
    query: &SearchQuery,
    offset_rate: Option<i32>,
    offset_peer: Option<ChatRef>,
    offset_id: i32,
) -> String {
    Cursor {
        fingerprint: query.fingerprint(),
        offset_rate,
        offset_peer: offset_peer.map(|p| p.to_string()),
        offset_id,
    }
    .encode()
}

impl Cursor {
    fn invalid() -> TgError {
        TgError::InvalidInput("cursor is not one this server returned".into())
    }

    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor serializes"))
    }

    fn decode(s: &str) -> Result<Self, TgError> {
        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| Self::invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| Self::invalid())
    }
}

// ── Words ───────────────────────────────────────────────────────────────────

/// The words of a query, lowercased: letters and digits, split at anything else.
pub fn query_words(query: &str) -> Vec<Vec<char>> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().flat_map(char::to_lowercase).collect())
        .collect()
}

fn same_letter(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Where the first word of `text` starts that begins with one of `words`
/// (case-insensitive), and how long the matching part is.
fn first_match(text: &[char], words: &[Vec<char>]) -> Option<(usize, usize)> {
    (0..text.len())
        .filter(|&at| at == 0 || !text[at - 1].is_alphanumeric())
        .find_map(|at| {
            words
                .iter()
                .filter(|w| {
                    text.len() - at >= w.len()
                        && w.iter().zip(&text[at..]).all(|(&q, &t)| same_letter(t, q))
                })
                .map(|w| (at, w.len()))
                .max_by_key(|&(_, len)| len)
        })
}

/// Up to `max` characters of `text` on one line, centred on the first word
/// that begins with a word of `query`; the start of the text if none does
/// (Telegram matched another word form). Cuts are marked with `…`.
pub fn snippet(text: &str, query: &str, max: usize) -> String {
    let flat: Vec<char> = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.len() <= max {
        return flat.into_iter().collect();
    }
    let room = max.saturating_sub(2).max(1);
    let start = match first_match(&flat, &query_words(query)) {
        Some((at, len)) => (at + len / 2)
            .saturating_sub(room / 2)
            .min(flat.len() - room),
        None => 0,
    };
    let end = start + room;
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&flat[start..end]);
    if end < flat.len() {
        out.push('…');
    }
    out
}

/// Whether every word of `query` begins a word of `text` (case-insensitive):
/// Telegram's matching without its word forms, for the fake.
pub fn text_matches(text: &str, query: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    query_words(query)
        .iter()
        .all(|word| first_match(&text, std::slice::from_ref(word)).is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::tg::fake::{FakeSearchChat, fake_rate, search_answer};

    /// No hash seen before: what a cold Live client knows.
    fn unknown(_: ChatId) -> Option<i64> {
        None
    }

    const DAY: i32 = 1_793_923_200;

    fn msg(id: i32, date: i32, text: &str) -> RawHistoryMessage {
        RawHistoryMessage {
            comments: None,
            id,
            date,
            from: None,
            out: false,
            text: text.into(),
            media: None,
            reply_to: None,
            reply_elsewhere: None,
            forwarded: false,
            forwarded_from: None,
            edited: false,
            reactions: Vec::new(),
            pinned: false,
            topic: None,
        }
    }

    fn global(text: &str, limit: usize) -> SearchQuery {
        SearchQuery {
            scope: SearchScope::Global {
                chat_type: None,
                archived: None,
            },
            text: text.into(),
            media: None,
            since: None,
            until: None,
            limit,
            cursor: None,
        }
    }

    fn in_chat(chat: ChatId, text: &str, limit: usize) -> SearchQuery {
        SearchQuery {
            scope: SearchScope::Chat {
                chat: chat.into(),
                from: None,
                topic: None,
            },
            ..global(text, limit)
        }
    }

    /// The futures here never wait: poll once.
    fn now<T>(future: impl Future<Output = T>) -> T {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(future).poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => unreachable!("the fake never waits"),
        }
    }

    /// One page of `query` from the fake's model of Telegram's search,
    /// recording the request.
    fn page(
        chats: &[FakeSearchChat],
        query: &SearchQuery,
        sent: &mut Vec<SearchRequest>,
    ) -> Result<SearchPage, TgError> {
        now(search(query, &unknown, |request| {
            sent.push(request.clone());
            std::future::ready(Ok(search_answer(chats, 42, &request)))
        }))
    }

    /// Every page of `query`, following `next_cursor` to the end.
    fn page_through(chats: &[FakeSearchChat], query: &SearchQuery) -> Vec<SearchPage> {
        let mut query = query.clone();
        let mut pages = Vec::new();
        loop {
            let page = page(chats, &query, &mut Vec::new()).unwrap();
            assert!(page.hits.len() <= query.limit, "a page over its limit");
            let next = page.next_cursor.clone();
            pages.push(page);
            match next {
                Some(next) => query.cursor = Some(next),
                None => return pages,
            }
            assert!(pages.len() < 500, "paging never ends");
        }
    }

    fn key(hit: &SearchHit) -> (String, i32) {
        (hit.chat.id.clone(), hit.id)
    }

    /// Chats with clashing message ids (ids are per chat), shared dates
    /// (several messages in one second, across chats) and texts of which
    /// some match "deploy".
    fn random_chats(seed: u64) -> Vec<(ChatId, Vec<RawHistoryMessage>)> {
        let mut seed = seed;
        let mut next = move |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % bound
        };
        let ids = [
            ChatId::User(5),
            ChatId::User(9),
            ChatId::Chat(3),
            ChatId::Channel(7),
            ChatId::Channel(8),
        ];
        let texts = [
            "deploy now",
            "Deployed!",
            "nothing",
            "redeploy",
            "the deploy",
        ];
        ids.iter()
            .take(1 + next(5) as usize)
            .map(|&chat| {
                let mut date = DAY;
                let messages = (1..=next(25) as i32)
                    .map(|id| {
                        date += next(3) as i32;
                        msg(id, date, texts[next(5) as usize])
                    })
                    .collect();
                (chat, messages)
            })
            .collect()
    }

    fn searchable(chats: &[(ChatId, Vec<RawHistoryMessage>)]) -> Vec<FakeSearchChat<'_>> {
        chats
            .iter()
            .map(|(id, messages)| FakeSearchChat {
                id: *id,
                kind: fallback_kind(*id),
                archived: false,
                messages,
            })
            .collect()
    }

    #[test]
    fn paging_a_global_search_yields_every_hit_once_newest_first() {
        for seed in 0..40 {
            let chats = random_chats(seed);
            let expected: HashSet<(String, i32)> = chats
                .iter()
                .flat_map(|(chat, messages)| {
                    messages
                        .iter()
                        .filter(|m| text_matches(&m.text, "deploy"))
                        .map(|m| (ChatRef::from(*chat).to_string(), m.id))
                })
                .collect();
            let chats = searchable(&chats);
            for limit in 1..=7 {
                let case = format!("seed {seed} limit {limit}");
                let hits: Vec<SearchHit> = page_through(&chats, &global("deploy", limit))
                    .into_iter()
                    .flat_map(|p| p.hits)
                    .collect();
                let keys: Vec<(String, i32)> = hits.iter().map(key).collect();
                let unique: HashSet<(String, i32)> = keys.iter().cloned().collect();
                assert_eq!(unique.len(), keys.len(), "a hit repeats: {case}");
                assert_eq!(unique, expected, "{case}");
                assert!(
                    hits.windows(2).all(|w| w[0].date >= w[1].date),
                    "newest first: {case}"
                );
            }
        }
    }

    #[test]
    fn paging_a_search_in_one_chat_yields_every_hit_once_newest_first() {
        for seed in 0..40 {
            let chats = random_chats(seed);
            let (chat, messages) = &chats[0];
            let mut expected: Vec<i32> = messages
                .iter()
                .filter(|m| text_matches(&m.text, "deploy"))
                .map(|m| m.id)
                .collect();
            expected.reverse();
            let chats = searchable(&chats);
            for limit in 1..=7 {
                let ids: Vec<i32> = page_through(&chats, &in_chat(*chat, "deploy", limit))
                    .into_iter()
                    .flat_map(|p| p.hits)
                    .map(|h| h.id)
                    .collect();
                assert_eq!(ids, expected, "seed {seed} limit {limit} in {chat}");
            }
        }
    }

    #[test]
    fn the_next_global_page_starts_at_telegrams_next_rate_and_the_last_hit() {
        let messages: Vec<RawHistoryMessage> = (1..=5).map(|i| msg(i, DAY + i, "deploy")).collect();
        let chats = [FakeSearchChat {
            id: ChatId::Channel(7),
            kind: ChatKind::Channel,
            archived: false,
            messages: &messages,
        }];
        let mut sent = Vec::new();
        let first = page(&chats, &global("deploy", 2), &mut sent).unwrap();
        let ids: Vec<i32> = first.hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, [5, 4]);
        let next = SearchQuery {
            cursor: first.next_cursor,
            ..global("deploy", 2)
        };
        page(&chats, &next, &mut sent).unwrap();
        assert_eq!(
            sent[1].scope,
            RequestScope::Global {
                chat_type: None,
                folder_id: None,
                // Telegram's rate, not the date.
                offset_rate: fake_rate(DAY + 4),
                offset_peer: Some(ChatRef::from(ChatId::Channel(7))),
                offset_id: 4,
            }
        );
        assert_ne!(fake_rate(DAY + 4), DAY + 4, "the fake's rate is opaque");
    }

    fn found(chat: ChatId, id: i32, date: i32) -> RawFoundMessage {
        RawFoundMessage {
            chat,
            message: msg(id, date, "x"),
        }
    }

    #[test]
    fn without_next_rate_or_after_a_cut_the_rate_is_the_last_hits_date() {
        let answer = |n: i32, next_rate| SearchAnswer {
            messages: (1..=n)
                .rev()
                .map(|i| found(ChatId::User(i.into()), i, i * 10))
                .collect(),
            returned: n as usize,
            complete: false,
            next_rate,
            peers: Vec::new(),
            forums: Default::default(),
        };
        let decode =
            |page: &SearchPage| Cursor::decode(page.next_cursor.as_deref().unwrap()).unwrap();
        let query = global("x", 3);
        let fingerprint = query.fingerprint();

        let page = assemble(&query, answer(3, Some(777)), &unknown, 3, fingerprint);
        assert_eq!(decode(&page).offset_rate, Some(777));
        let page = assemble(&query, answer(3, None), &unknown, 3, fingerprint);
        assert_eq!(decode(&page).offset_rate, Some(10), "the oldest hit's date");

        // More than `limit` sent: cut, and continue right after the last hit kept.
        let page = assemble(&query, answer(5, Some(777)), &unknown, 3, fingerprint);
        assert_eq!(
            page.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            [5, 4, 3]
        );
        let cursor = decode(&page);
        assert_eq!(
            (
                cursor.offset_rate,
                cursor.offset_peer.as_deref(),
                cursor.offset_id
            ),
            (Some(30), Some("user:3"), 3)
        );
    }

    #[test]
    fn a_short_or_complete_answer_is_the_last_page() {
        let query = in_chat(ChatId::Channel(7), "x", 3);
        let answer = |n: usize, returned, complete| SearchAnswer {
            messages: (1..=n as i32)
                .rev()
                .map(|i| found(ChatId::Channel(7), i, i))
                .collect(),
            returned,
            complete,
            next_rate: None,
            peers: Vec::new(),
            forums: Default::default(),
        };
        let f = query.fingerprint();
        assert!(
            assemble(&query, answer(2, 2, false), &unknown, 3, f)
                .next_cursor
                .is_none()
        );
        assert!(
            assemble(&query, answer(3, 3, true), &unknown, 3, f)
                .next_cursor
                .is_none()
        );
        // A full slice may have more, even if a messageEmpty was left out.
        assert!(
            assemble(&query, answer(2, 3, false), &unknown, 3, f)
                .next_cursor
                .is_some()
        );
    }

    #[test]
    fn a_cursor_only_continues_the_search_it_came_from() {
        let messages: Vec<RawHistoryMessage> =
            (1..=5).map(|i| msg(i, DAY + i, "deploy now")).collect();
        let chats = [FakeSearchChat {
            id: ChatId::Channel(7),
            kind: ChatKind::Channel,
            archived: false,
            messages: &messages,
        }];
        let first = page(&chats, &global("deploy", 2), &mut Vec::new()).unwrap();
        let cursor = first.next_cursor.unwrap();
        let with = |query: SearchQuery| SearchQuery {
            cursor: Some(cursor.clone()),
            ..query
        };
        let changed = [
            global("now", 2),
            in_chat(ChatId::Channel(7), "deploy", 2),
            SearchQuery {
                media: Some(MediaFilter::Photo),
                ..global("deploy", 2)
            },
            SearchQuery {
                since: Some(1),
                ..global("deploy", 2)
            },
            SearchQuery {
                until: Some(i64::from(DAY) + 9),
                ..global("deploy", 2)
            },
            SearchQuery {
                scope: SearchScope::Global {
                    chat_type: Some(ChatType::Channels),
                    archived: None,
                },
                ..global("deploy", 2)
            },
            SearchQuery {
                scope: SearchScope::Global {
                    chat_type: None,
                    archived: Some(false),
                },
                ..global("deploy", 2)
            },
        ];
        for query in changed {
            let mut sent = Vec::new();
            let err = page(&chats, &with(query.clone()), &mut sent).unwrap_err();
            assert!(
                err.to_string().contains("different search"),
                "{query:?}: {err}"
            );
            assert!(sent.is_empty(), "nothing is asked of Telegram");
        }
        // The same search, any limit, continues.
        let next = page(&chats, &with(global("deploy", 3)), &mut Vec::new()).unwrap();
        assert_eq!(
            next.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        // From another sender is another search too.
        let a = in_chat(ChatId::Channel(7), "", 2);
        let b = SearchQuery {
            scope: SearchScope::Chat {
                chat: ChatId::Channel(7).into(),
                from: Some(SearchFrom::Me),
                topic: None,
            },
            ..a.clone()
        };
        assert_ne!(a.fingerprint(), b.fingerprint());
        for junk in ["", "!!", &URL_SAFE_NO_PAD.encode(b"{}")] {
            let query = SearchQuery {
                cursor: Some(junk.into()),
                ..global("deploy", 2)
            };
            assert!(matches!(
                page(&chats, &query, &mut Vec::new()),
                Err(TgError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn a_topic_is_part_of_the_search_a_cursor_continues() {
        let in_topic = |topic| SearchQuery {
            scope: SearchScope::Chat {
                chat: ChatId::Channel(5).into(),
                from: None,
                topic,
            },
            ..in_chat(ChatId::Channel(5), "deploy", 2)
        };
        let fingerprints = [None, Some(1), Some(10)].map(|t| in_topic(t).fingerprint());
        assert_ne!(fingerprints[0], fingerprints[1]);
        assert_ne!(fingerprints[1], fingerprints[2]);
        // Without a Topic, as before Topics (#28): cursors handed out then
        // still continue.
        assert_eq!(fingerprints[0], 0xf9b8_688b_0721_5b90);
    }

    #[test]
    fn a_topic_search_is_telegrams_search_with_top_msg_id_in_that_topic() {
        let messages: Vec<RawHistoryMessage> = (1..=6)
            .map(|i| RawHistoryMessage {
                topic: (i % 2 == 0).then_some(10),
                ..msg(i, DAY + i, "deploy")
            })
            .collect();
        let chats = [FakeSearchChat {
            id: ChatId::Channel(5),
            kind: ChatKind::Supergroup,
            archived: false,
            messages: &messages,
        }];
        let query = SearchQuery {
            scope: SearchScope::Chat {
                chat: ChatId::Channel(5).into(),
                from: None,
                topic: Some(10),
            },
            ..in_chat(ChatId::Channel(5), "deploy", 2)
        };
        let mut sent = Vec::new();
        let first = page(&chats, &query, &mut sent).unwrap();
        assert_eq!(first.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [6, 4]);
        assert!(matches!(
            sent[0].scope,
            RequestScope::Chat {
                topic: Some(10),
                ..
            }
        ));
        let next = SearchQuery {
            cursor: first.next_cursor,
            ..query
        };
        let rest = page(&chats, &next, &mut Vec::new()).unwrap();
        assert_eq!(rest.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn date_bounds_are_inclusive_and_sent_as_telegrams_exclusive_ones() {
        let messages: Vec<RawHistoryMessage> = (1..=9).map(|i| msg(i, DAY + i, "deploy")).collect();
        let chats = [FakeSearchChat {
            id: ChatId::User(5),
            kind: ChatKind::Private,
            archived: false,
            messages: &messages,
        }];
        let query = SearchQuery {
            since: Some(i64::from(DAY) + 3),
            until: Some(i64::from(DAY) + 6),
            ..global("deploy", 10)
        };
        let mut sent = Vec::new();
        let page = page(&chats, &query, &mut sent).unwrap();
        assert_eq!(
            page.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            [6, 5, 4, 3]
        );
        assert_eq!((sent[0].min_date, sent[0].max_date), (DAY + 2, DAY + 7));
    }

    #[test]
    fn hits_carry_issued_ids_and_never_invent_a_hash() {
        let alice = ChatId::User(5);
        let group = ChatId::Channel(9);
        let peer = |id, title: &str, kind, hash| RawPeer {
            id,
            title: title.into(),
            kind,
            username: None,
            access_hash: hash,
            is_self: false,
        };
        let from = |chat, id, sender: ChatId| RawFoundMessage {
            chat,
            message: RawHistoryMessage {
                from: Some(sender),
                ..msg(id, DAY, "deploy")
            },
        };
        let answer = SearchAnswer {
            messages: vec![
                from(group, 3, alice),
                // A min sender: described, but without a hash we may hand out.
                from(group, 2, ChatId::User(6)),
                // A chat Telegram didn't describe.
                from(ChatId::Channel(10), 1, ChatId::User(7)),
            ],
            returned: 3,
            complete: true,
            next_rate: None,
            peers: vec![
                peer(alice, "Alice", ChatKind::Private, Some(-1)),
                peer(group, "Team", ChatKind::Supergroup, Some(99)),
                peer(ChatId::User(6), "Min", ChatKind::Private, None),
            ],
            forums: Default::default(),
        };
        let query = global("deploy", 10);
        let page = assemble(&query, answer.clone(), &unknown, 10, query.fingerprint());
        let team = ChatRef::new(group, Some(99)).to_string();
        assert_eq!(
            page.hits[0].chat,
            HitChat {
                id: team.clone(),
                title: "Team".into(),
                kind: ChatKind::Supergroup
            }
        );
        assert_eq!(
            page.hits[0].sender.as_ref().unwrap().id,
            ChatRef::new(alice, Some(-1)).to_string()
        );
        assert_eq!(page.hits[1].sender.as_ref().unwrap().id, "user:6");
        assert_eq!(page.hits[1].sender.as_ref().unwrap().name, "Min");
        assert_eq!(
            page.hits[2].chat,
            HitChat {
                id: "channel:10".into(),
                title: "Unknown chat".into(),
                kind: ChatKind::Channel
            }
        );
        assert_eq!(page.hits[2].sender.as_ref().unwrap().id, "user:7");

        // Within one chat, an undescribed chat keeps the hash it was asked with.
        let asked = ChatRef::new(ChatId::Channel(10), Some(5));
        let query = SearchQuery {
            scope: SearchScope::Chat {
                chat: asked,
                from: None,
                topic: None,
            },
            ..global("deploy", 10)
        };
        let page = assemble(&query, answer, &unknown, 10, query.fingerprint());
        assert_eq!(page.hits[2].chat.id, asked.to_string());
        assert_eq!(page.hits[0].chat.id, team);
    }

    #[test]
    fn a_snippet_is_centred_on_the_first_matching_word() {
        let text = format!(
            "{} Релиза сегодня не будет {}",
            "а".repeat(400),
            "б".repeat(400)
        );
        let cut = snippet(&text, "релиз", 300);
        assert_eq!(cut.chars().count(), 300);
        assert!(cut.starts_with('…') && cut.ends_with('…'), "{cut}");
        let at = cut.find("Релиза").expect("the match is in the snippet");
        let before = cut[..at].chars().count();
        assert!((140..=150).contains(&before), "centred: {before}");

        // The first match of any word wins; inside a word doesn't count.
        let text = format!(
            "redeploy {} release then deploy {}",
            "x".repeat(300),
            "y".repeat(300)
        );
        let cut = snippet(&text, "deploy release", 100);
        assert!(
            cut.contains("release") && !cut.contains("redeploy"),
            "{cut}"
        );

        // No literal match (Telegram matched another word form: деплой →
        // деплоя changes a letter): the start.
        let text = format!("{} деплоя", "z".repeat(500));
        let cut = snippet(&text, "деплой", 300);
        assert_eq!(cut, format!("{}…", "z".repeat(298)));

        // Short texts whole, on one line; media without text has none.
        assert_eq!(snippet(" line\none\n", "one", 300), "line one");
        assert_eq!(snippet("", "x", 300), "");
        // Near the end, the window keeps its full width.
        let text = format!("{} deploy", "w".repeat(500));
        let cut = snippet(&text, "deploy", 50);
        assert!(
            cut.ends_with("deploy") && cut.chars().count() == 49,
            "{cut}"
        );
    }

    #[test]
    fn matching_is_by_word_beginnings_and_ignores_case() {
        assert!(text_matches("Релиза не будет", "релиз"));
        assert!(
            !text_matches("Деплоя не будет", "деплой"),
            "й → я is a word form"
        );
        assert!(text_matches("deploy is GREEN", "green deploy"));
        assert!(!text_matches("redeploy", "deploy"));
        assert!(!text_matches("deploy", "deploy green"));
        assert!(text_matches("anything", "  "));
    }
}
