//! Topics of forum supergroups as the tools see them, independent of
//! grammers/TL (#28).
//!
//! A forum's every message is in one Topic: the one its reply header names,
//! else General ([`GENERAL_TOPIC_ID`]). The adapters say which Chats an answer
//! describes as forums and which Topics it names ([`RawForums`]); messages
//! are tagged with their Topic where they are turned into the tools' shape
//! ([`super::messages::Issuer`]), and a page names the Topics it mentions in
//! one `topics` map: titles the answers carried, the rest looked up with one
//! `messages.getForumTopicsByID` per forum ([`complete`],
//! [`complete_search`]). Titles are Telegram content: the sanitiser cleans
//! them, the tools cut them to size.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatRef},
    messages::{ChatMessage, MessagePage},
    search::SearchPage,
};

/// The General Topic's id in every forum. Its messages name no Topic.
pub const GENERAL_TOPIC_ID: i32 = 1;
/// get_chat_info lists at most this many Topics.
pub const MAX_LISTED_TOPICS: usize = 50;
/// Telegram's longest Topic title, in characters.
pub const MAX_TOPIC_TITLE_CHARS: usize = 128;
/// A page names at most this many Topics: one per message of the largest
/// page Telegram answers.
pub const MAX_PAGE_TOPICS: usize = 100;
/// A search page across Chats looks titles up in at most this many forums;
/// the titles of the others' Topics are left out (their hits keep `topic`).
pub const MAX_TOPIC_LOOKUPS: usize = 5;

/// One Topic as get_chat_info lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Topic {
    /// Pass it as `topic` to get_messages or search_messages. General is 1.
    pub id: i32,
    #[schemars(length(max = MAX_TOPIC_TITLE_CHARS))]
    pub title: String,
    /// Closed: nobody but admins can post in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

/// The titles of the Topics a page mentions, by Topic id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend(
    "maxProperties" = MAX_PAGE_TOPICS,
    "patternProperties" = {
        "^[1-9][0-9]{0,9}$": { "type": "string", "maxLength": MAX_TOPIC_TITLE_CHARS },
    },
    "additionalProperties" = false,
))]
pub struct TopicTitles(pub BTreeMap<i32, String>);

/// The titles of the Topics a search page mentions: by the chat id its hits
/// carry, then by Topic id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("maxProperties" = MAX_PAGE_TOPICS))]
pub struct SearchTopics(pub BTreeMap<String, TopicTitles>);

/// A Topic as Telegram describes it (`forumTopic`), and its forum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawTopic {
    pub chat: ChatId,
    pub id: i32,
    pub title: String,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub pinned: bool,
}

/// What one answer says about forums: which of the Chats it describes are
/// forums (`channel.forum`), and the Topics it describes (`topics`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawForums {
    pub forums: Vec<ChatId>,
    pub topics: Vec<RawTopic>,
}

impl RawForums {
    pub fn extend(&mut self, other: RawForums) {
        self.forums.extend(other.forums);
        self.topics.extend(other.topics);
    }
}

/// The Topic of a forum message that names `raw` ([`RawHistoryMessage::topic`](super::messages::RawHistoryMessage::topic)).
pub fn topic_of(raw: Option<i32>) -> i32 {
    raw.unwrap_or(GENERAL_TOPIC_ID)
}

/// What `topic` on a Chat that is no forum says.
pub fn not_a_forum(chat: ChatId) -> TgError {
    TgError::InvalidInput(format!(
        "{chat} is not a forum: only forum supergroups have Topics. Leave out topic"
    ))
}

/// What a Topic the forum doesn't have says.
pub fn unknown_topic(topic: i32) -> TgError {
    TgError::InvalidInput(format!(
        "topic {topic} is not a Topic of this forum (get_chat_info lists them)"
    ))
}

/// get_chat_info's `topics` and `topics_truncated`: the first
/// [`MAX_LISTED_TOPICS`] in Telegram's order, and whether the forum has more
/// than those (`count`, Telegram's total).
pub fn listed(raw: Vec<RawTopic>, count: usize) -> (Vec<Topic>, bool) {
    let truncated = count.max(raw.len()) > MAX_LISTED_TOPICS;
    let topics = raw
        .into_iter()
        .take(MAX_LISTED_TOPICS)
        .map(|t| Topic {
            id: t.id,
            title: t.title,
            closed: t.closed,
            pinned: t.pinned,
        })
        .collect();
    (topics, truncated)
}

/// The Topics `messages` mention, each once.
fn mentioned<'a>(messages: impl IntoIterator<Item = &'a ChatMessage>) -> BTreeSet<i32> {
    messages.into_iter().filter_map(|m| m.topic).collect()
}

/// A page's `topics` from the Topics `known` describes: `None` when no
/// message of `chat` is in a Topic (no forum); otherwise the titles of the
/// mentioned ones it knows.
pub fn page_titles(
    chat: ChatId,
    messages: &[ChatMessage],
    known: &[RawTopic],
) -> Option<TopicTitles> {
    let mentioned = mentioned(messages);
    if mentioned.is_empty() {
        return None;
    }
    Some(TopicTitles(
        known
            .iter()
            .filter(|t| t.chat == chat && mentioned.contains(&t.id))
            .map(|t| (t.id, t.title.clone()))
            .collect(),
    ))
}

/// Name every Topic `page` mentions that its `topics` has no title for,
/// with one `lookup` of their ids (`messages.getForumTopicsByID`). A Topic
/// Telegram doesn't describe (deleted) stays without a title.
pub async fn complete<F, Fut>(page: &mut MessagePage, lookup: F) -> Result<(), TgError>
where
    F: FnOnce(Vec<i32>) -> Fut,
    Fut: Future<Output = Result<Vec<RawTopic>, TgError>>,
{
    let Some(titles) = &mut page.topics else {
        return Ok(());
    };
    let missing = titles.missing(mentioned(&page.messages));
    if !missing.is_empty() {
        titles.add(&missing, lookup(missing.clone()).await?);
    }
    Ok(())
}

impl TopicTitles {
    /// Those of `mentioned` without a title here.
    fn missing(&self, mentioned: BTreeSet<i32>) -> Vec<i32> {
        mentioned
            .into_iter()
            .filter(|id| !self.0.contains_key(id))
            .collect()
    }

    /// The titles of the `found` Topics that were `asked` for.
    fn add(&mut self, asked: &[i32], found: Vec<RawTopic>) {
        for topic in found {
            if asked.contains(&topic.id) {
                self.0.insert(topic.id, topic.title);
            }
        }
    }
}

/// Like [`complete`] for a search page: one `lookup` per forum with Topics
/// its `topics` has no title for, in at most [`MAX_TOPIC_LOOKUPS`] forums.
pub async fn complete_search<F, Fut>(page: &mut SearchPage, mut lookup: F) -> Result<(), TgError>
where
    F: FnMut(ChatRef, Vec<i32>) -> Fut,
    Fut: Future<Output = Result<Vec<RawTopic>, TgError>>,
{
    let Some(topics) = &mut page.topics else {
        return Ok(());
    };
    let mut wanted: BTreeMap<&str, BTreeSet<i32>> = BTreeMap::new();
    for hit in &page.hits {
        if let Some(topic) = hit.topic {
            wanted.entry(&hit.chat.id).or_default().insert(topic);
        }
    }
    let mut lookups = 0;
    for (chat_id, mentioned) in wanted {
        let titles = topics.0.entry(chat_id.to_string()).or_default();
        let missing = titles.missing(mentioned);
        if missing.is_empty() {
            continue;
        }
        if lookups == MAX_TOPIC_LOOKUPS {
            break;
        }
        lookups += 1;
        // Ids on hits are issued ids; one that doesn't parse can't be asked.
        let Ok(chat) = chat_id.parse::<ChatRef>() else {
            continue;
        };
        titles.add(&missing, lookup(chat, missing.clone()).await?);
    }
    topics.0.retain(|_, titles| !titles.0.is_empty());
    if topics.0.is_empty() {
        page.topics = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::{
        chats::ChatKind,
        messages::MessagePage,
        search::{HitChat, SearchHit},
    };

    fn now<T>(future: impl Future<Output = T>) -> T {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(future).poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => unreachable!("lookups here never wait"),
        }
    }

    const FORUM: ChatId = ChatId::Channel(5);

    fn raw(chat: ChatId, id: i32, title: &str) -> RawTopic {
        RawTopic {
            chat,
            id,
            title: title.into(),
            closed: false,
            pinned: false,
        }
    }

    fn message(id: i32, topic: Option<i32>) -> ChatMessage {
        ChatMessage {
            comments: None,
            id,
            date: String::new(),
            sender: None,
            outgoing: true,
            text: String::new(),
            media: None,
            reply_to: None,
            forwarded: false,
            edited: false,
            reactions: None,
            link: None,
            pinned: false,
            topic,
            target: false,
            thread_root: false,
        }
    }

    /// A page of `chat` whose messages are in `topics`, titled from `known`.
    fn page(chat: ChatId, topics: &[Option<i32>], known: &[RawTopic]) -> MessagePage {
        let messages: Vec<ChatMessage> = topics
            .iter()
            .enumerate()
            .map(|(i, &t)| message(i as i32 + 1, t))
            .collect();
        MessagePage {
            topics: page_titles(chat, &messages, known),
            ..MessagePage::new(chat.to_string(), messages)
        }
    }

    #[test]
    fn a_page_titles_the_topics_it_mentions_from_what_the_answers_described() {
        let known = [
            raw(FORUM, 1, "General"),
            raw(FORUM, 10, "Releases"),
            raw(FORUM, 20, "Bugs"),
            raw(ChatId::Channel(6), 30, "Elsewhere"),
        ];
        let forum = page(FORUM, &[Some(10), Some(1), Some(10), Some(30)], &known);
        assert_eq!(
            forum.topics,
            Some(TopicTitles(
                [(1, "General".into()), (10, "Releases".into())].into()
            )),
            "only mentioned Topics, only this forum's"
        );
        assert_eq!(page(ChatId::Channel(9), &[None, None], &known).topics, None);
    }

    #[test]
    fn untitled_topics_are_looked_up_once_and_titled_ones_never() {
        let known = [raw(FORUM, 10, "Releases")];
        let mut lookups = Vec::new();
        let mut forum = page(FORUM, &[Some(10), Some(20), Some(1), Some(20)], &known);
        now(complete(&mut forum, |ids| {
            lookups.push(ids);
            // Telegram describes what it has: Topic 20 was deleted.
            std::future::ready(Ok(vec![raw(FORUM, 1, "General"), raw(FORUM, 7, "Other")]))
        }))
        .unwrap();
        assert_eq!(lookups, [vec![1, 20]]);
        assert_eq!(
            forum.topics,
            Some(TopicTitles(
                [(1, "General".into()), (10, "Releases".into())].into()
            ))
        );

        for mut titled in [
            page(FORUM, &[Some(10)], &known),
            page(ChatId::Channel(9), &[None], &known),
        ] {
            now(complete(&mut titled, |_| -> std::future::Ready<_> {
                unreachable!("nothing to look up")
            }))
            .unwrap();
        }
    }

    fn hit(chat: usize, id: i32, topic: Option<i32>) -> SearchHit {
        SearchHit {
            chat: HitChat {
                id: format!("channel:{chat}"),
                title: String::new(),
                kind: ChatKind::Supergroup,
            },
            id,
            date: String::new(),
            sender: None,
            outgoing: true,
            snippet: String::new(),
            media: None,
            reply_to: None,
            forwarded: false,
            edited: false,
            reactions: None,
            link: None,
            pinned: false,
            topic,
        }
    }

    #[test]
    fn a_search_page_looks_up_each_forums_untitled_topics_in_a_few_forums_at_most() {
        // Hits in eight forums, Topic 2 of each untitled; forum 1 also has
        // Topic 1, titled by the answer.
        let mut hits: Vec<SearchHit> = (1..=8).map(|chat| hit(chat, 10, Some(2))).collect();
        hits.push(hit(1, 11, Some(1)));
        hits.push(hit(9, 12, None));
        let mut page = SearchPage {
            topics: Some(SearchTopics(
                [(
                    "channel:1".to_string(),
                    TopicTitles([(1, "General".into())].into()),
                )]
                .into(),
            )),
            hits,
            next_cursor: None,
        };
        let mut lookups = Vec::new();
        now(complete_search(&mut page, |chat, ids| {
            lookups.push((chat.chat(), ids.clone()));
            std::future::ready(Ok(ids
                .iter()
                .map(|&id| raw(chat.chat(), id, &format!("{} #{id}", chat.chat())))
                .collect()))
        }))
        .unwrap();
        let asked: Vec<(ChatId, Vec<i32>)> = (1..=5)
            .map(|chat| (ChatId::Channel(chat), vec![2]))
            .collect();
        assert_eq!(lookups, asked, "one lookup per forum, at most five");
        let topics = page.topics.unwrap().0;
        assert_eq!(
            topics["channel:1"],
            TopicTitles([(1, "General".into()), (2, "channel:1 #2".into())].into())
        );
        assert_eq!(topics.len(), 5, "forums 6-8 stay untitled: {topics:?}");
    }

    #[test]
    fn a_forum_lists_fifty_topics_and_says_when_it_has_more() {
        let raw: Vec<RawTopic> = (1..=60).map(|id| raw(FORUM, id, "t")).collect();
        let (listed, truncated) = listed(raw.clone(), 60);
        assert_eq!(listed.len(), MAX_LISTED_TOPICS);
        assert_eq!((listed[0].id, listed[49].id), (1, 50));
        assert!(truncated);
        // Telegram's count, when it says there are more than it sent.
        assert!(listed_len(raw[..50].to_vec(), 51).1);
        assert!(!listed_len(raw[..50].to_vec(), 50).1);
        assert!(!listed_len(raw[..3].to_vec(), 3).1);
    }

    fn listed_len(raw: Vec<RawTopic>, count: usize) -> (usize, bool) {
        let (topics, truncated) = listed(raw, count);
        (topics.len(), truncated)
    }
}
