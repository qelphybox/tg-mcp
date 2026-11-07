//! Finding Chats by name or @username (`find_chats`), independent of
//! grammers/TL (ADR-0004: Telegram searches, we keep no index).
//!
//! One `contacts.search` call. Its answer has two lists: `my_results`, the
//! Account's own Chats and contacts that match, and `results`, public Chats
//! and users found globally, which may include ones the Account hasn't joined
//! and may repeat its own. Only `my_results` is kept (#19: no Chats the Account
//! isn't in); which Chats are shown, once each, and the ids handed out are
//! decided here. The fake answers the same request with a model of Telegram.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatKind, ChatRef, RawPeer, fallback_kind},
    messages::{Issuer, KnownHashes},
};

/// One Chat or contact found: the part of a list_chats Chat that names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundChat {
    /// Pass this to the other tools.
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChatKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// What `find_chats` answers: the matching Chats in Telegram's order. No
/// paging: `limit` is all there is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundChats {
    pub chats: Vec<FoundChat>,
}

/// How many peers `contacts.search` is asked for, whatever the caller's
/// `limit`: Telegram doesn't document whether its `limit` caps each list or
/// both together, so global results must not be able to crowd out the
/// Account's own. The page is cut to the caller's `limit` here.
pub const TELEGRAM_LIMIT: i32 = 50;

/// One `contacts.search` call, as its TL parameters (`broadcasts` and `bots`
/// stay unset: no narrowing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindRequest {
    pub q: String,
    pub limit: i32,
}

/// What Telegram answered to one [`FindRequest`] (`contacts.found`).
#[derive(Debug, Clone, Default)]
pub struct FoundAnswer {
    /// The Account's own Chats and contacts, in Telegram's order.
    pub my_results: Vec<ChatId>,
    /// Global results: public Chats and users, joined or not. Never shown.
    pub results: Vec<ChatId>,
    /// The answer's `users` and `chats`: they may not describe every peer,
    /// and min peers come without a usable access hash.
    pub peers: Vec<RawPeer>,
}

/// Telegram's `QUERY_TOO_SHORT`, as the tools report it.
pub const QUERY_TOO_SHORT: &str = "Telegram refuses so short a query for its chat search: give \
more of the name, title or @username.";

/// Find the Account's Chats and contacts matching `query` (already trimmed,
/// without a leading `@`), at most `limit`, with one call sent through `send`;
/// ids the answer leaves without a hash take the one in `known`.
pub async fn find_chats<F, Fut>(
    query: &str,
    limit: usize,
    known: KnownHashes<'_>,
    send: F,
) -> Result<FoundChats, TgError>
where
    F: FnOnce(FindRequest) -> Fut,
    Fut: Future<Output = Result<FoundAnswer, TgError>>,
{
    if !(1..=TELEGRAM_LIMIT as usize).contains(&limit) {
        return Err(TgError::InvalidInput("limit is out of range".into()));
    }
    let answer = send(FindRequest {
        q: query.to_string(),
        limit: TELEGRAM_LIMIT,
    })
    .await?;
    Ok(assemble(answer, known, limit))
}

/// The Account's own matches, each once, at most `limit`, with issued ids.
fn assemble(answer: FoundAnswer, known: KnownHashes<'_>, limit: usize) -> FoundChats {
    let FoundAnswer {
        my_results,
        // Public Chats the Account hasn't joined are out of bounds (#19), and
        // its own public ones are in `my_results` already.
        results: _,
        peers,
    } = answer;
    let issuer = Issuer::new(&peers, known);
    let mut seen = HashSet::new();
    let chats = my_results
        .into_iter()
        .filter(|&chat| seen.insert(chat))
        .take(limit)
        .map(|chat| {
            let peer = issuer.peer(chat);
            FoundChat {
                id: issuer.issue(chat, ChatRef::from(chat)).to_string(),
                title: peer.map_or_else(|| "Unknown chat".into(), |p| p.title.clone()),
                kind: peer.map_or_else(|| fallback_kind(chat), |p| p.kind),
                username: peer.and_then(|p| p.username.clone()),
            }
        })
        .collect();
    FoundChats { chats }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No hash seen before: what a cold Live client knows.
    fn unknown(_: ChatId) -> Option<i64> {
        None
    }

    fn peer(id: ChatId, title: &str, hash: Option<i64>) -> RawPeer {
        RawPeer {
            id,
            title: title.into(),
            kind: fallback_kind(id),
            username: None,
            access_hash: hash,
            is_self: false,
        }
    }

    /// The futures here never wait: poll once.
    fn now<T>(future: impl Future<Output = T>) -> T {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(future).poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => unreachable!("never waits"),
        }
    }

    fn ids(found: &FoundChats) -> Vec<&str> {
        found.chats.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn only_own_matches_are_shown_each_once() {
        let own = ChatId::Channel(1);
        let contact = ChatId::User(2);
        let stranger = ChatId::Channel(3);
        let answer = FoundAnswer {
            // Not promised unique: a repeat is shown once.
            my_results: vec![own, contact, own],
            // An own public chat again, and one the Account isn't in.
            results: vec![stranger, own],
            peers: vec![
                peer(own, "Own", Some(10)),
                peer(contact, "Contact", Some(20)),
                peer(stranger, "Stranger", Some(30)),
            ],
        };
        let found = assemble(answer, &unknown, 10);
        assert_eq!(
            ids(&found),
            [
                ChatRef::new(own, Some(10)).to_string(),
                ChatRef::new(contact, Some(20)).to_string(),
            ]
        );
        assert_eq!(found.chats[1].title, "Contact");
        assert_eq!(found.chats[1].kind, ChatKind::Private);
    }

    #[test]
    fn ids_carry_the_hash_telegram_sent_and_never_an_invented_one() {
        let min = ChatId::User(5);
        let ghost = ChatId::Channel(6);
        let group = ChatId::Chat(7);
        let answer = FoundAnswer {
            my_results: vec![min, ghost, group],
            results: Vec::new(),
            peers: vec![peer(min, "Min", None), peer(group, "Group", None)],
        };
        let found = assemble(answer, &unknown, 10);
        assert_eq!(ids(&found), ["user:5", "channel:6", "chat:7"]);
        assert_eq!(
            (found.chats[1].title.as_str(), found.chats[1].kind),
            ("Unknown chat", ChatKind::Channel),
            "Telegram didn't describe it"
        );
    }

    #[test]
    fn the_answer_is_cut_to_limit_after_repeats_are_dropped() {
        let chats: Vec<ChatId> = (1..=8).map(ChatId::User).collect();
        let answer = FoundAnswer {
            my_results: [chats[0], chats[0]]
                .into_iter()
                .chain(chats.iter().copied())
                .collect(),
            results: Vec::new(),
            peers: Vec::new(),
        };
        assert_eq!(
            ids(&assemble(answer, &unknown, 3)),
            ["user:1", "user:2", "user:3"]
        );
    }

    #[test]
    fn telegram_is_asked_for_more_than_the_caller_wants() {
        let mut sent = Vec::new();
        let found = now(find_chats("alice", 2, &unknown, |request| {
            sent.push(request);
            std::future::ready(Ok(FoundAnswer {
                my_results: (1..=5).map(ChatId::User).collect(),
                ..FoundAnswer::default()
            }))
        }))
        .unwrap();
        assert_eq!(found.chats.len(), 2);
        assert_eq!(
            sent,
            [FindRequest {
                q: "alice".into(),
                limit: TELEGRAM_LIMIT
            }]
        );
        for limit in [0, 51] {
            let result = now(find_chats("alice", limit, &unknown, |_| async {
                unreachable!("nothing is asked of Telegram")
            }));
            assert!(matches!(result, Err(TgError::InvalidInput(_))));
        }
    }
}
