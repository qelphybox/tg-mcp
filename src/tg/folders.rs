//! Chat folders: the Account's named groupings of Chats (`messages.getDialogFilters`),
//! listed by whoami, and the Chats of one defined as an explicit list, listed
//! by list_chats `chat_folder`.
//!
//! The Telegram adapter extracts [`RawChatFolder`]s; which Chats a folder
//! lists, in which order, and where its pages continue is decided here.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{
        ChatId, ChatPage, ChatRef, ChatSummary, ChatsQuery, Cursor, DialogsAnswer, RawDialog,
        RawMessage, other_unread_listing, summary,
    },
};

/// One of the Account's Chat folders, as whoami lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChatFolder {
    /// Pass it to list_chats as `chat_folder`.
    pub id: i32,
    pub title: String,
    /// The folder's icon, when the user picked an emoji for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// The folder is defined by rules (kinds of chats, or leaving out muted,
    /// read or archived chats), not only by a list of chats: list_chats can't
    /// list its chats yet.
    pub rules: bool,
}

/// A Chat folder as the adapter reads it: whoami's summary and the Chats it
/// names. Pinned and included Chats come with what addresses them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChatFolder {
    pub folder: ChatFolder,
    /// The folder's pinned Chats, in the folder's order.
    pub pinned: Vec<ChatRef>,
    pub included: Vec<ChatRef>,
    pub excluded: Vec<ChatId>,
}

/// Telegram's most peers per `messages.getPeerDialogs` call.
pub const PEER_DIALOGS_CHUNK: usize = 100;
/// The most Chats of one folder list_chats reads: Telegram allows 100 per
/// folder, 200 with Premium. With the folder itself, one list_chats makes at
/// most 1 + 3 Telegram calls.
pub const MAX_FOLDER_CHATS: usize = 3 * PEER_DIALOGS_CHUNK;

/// What list_chats says of a folder defined by rules.
pub const RULES_NOT_SUPPORTED: &str = "this Chat folder is defined by rules, not supported yet \
     (only folders that are a list of chats are): use list_chats without chat_folder \
     to read the whole chat list instead";

/// Where the next page of a folder's listing starts: the folder's pinned
/// Chats from position `pinned_from` (absent: none left), then its other
/// Chats after `after` (absent: from the most recent). Its own shape, with
/// keys no other cursor has, so a cursor of the other kind never passes for
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderCursor {
    #[serde(rename = "f")]
    pub chat_folder: i32,
    #[serde(rename = "u", default, skip_serializing_if = "std::ops::Not::not")]
    pub unread_only: bool,
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub pinned_from: Option<usize>,
    /// The last unpinned Chat shown: its last message's date and its id.
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    pub after: Option<(i32, ChatId)>,
}

impl FolderCursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor serializes"))
    }

    /// A cursor of a folder's listing, else `None`.
    pub fn decode(s: &str) -> Option<Self> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(s).ok()?).ok()
    }
}

/// What a cursor of a folder's listing passed without `chat_folder` (or with
/// another one) is.
pub fn other_folder(cursor: &FolderCursor) -> TgError {
    TgError::InvalidInput(format!(
        "this cursor continues the listing of Chat folder {0}: pass chat_folder: {0} with it",
        cursor.chat_folder
    ))
}

/// One page of the Chats of Chat folder `folder_id`, for list_chats: the
/// folder's pinned Chats in its order, then the others most recent first,
/// each once, `query.unread_only` keeping only unread or marked-unread ones.
/// `list_folders` is one `messages.getDialogFilters`; the Chats come from
/// `peer_dialogs`, one `messages.getPeerDialogs` per [`PEER_DIALOGS_CHUNK`]
/// Chats still to show, at most [`MAX_FOLDER_CHATS`] of the folder. A Chat
/// Telegram answers no dialog for (left, deleted, not addressable) is not
/// listed, as in the app. Archived Chats are listed among the others: a
/// folder leaves them out only by a rule. Every page reads the folder anew,
/// so a Chat that moves up meanwhile may be missed, as with the main list.
pub async fn list_folder_chats<L, LFut, P, PFut>(
    query: &ChatsQuery,
    folder_id: i32,
    list_folders: L,
    mut peer_dialogs: P,
) -> Result<ChatPage, TgError>
where
    L: FnOnce() -> LFut,
    LFut: Future<Output = Result<Vec<RawChatFolder>, TgError>>,
    P: FnMut(Vec<ChatRef>) -> PFut,
    PFut: Future<Output = Result<DialogsAnswer, TgError>>,
{
    if query.archived {
        return Err(TgError::InvalidInput(
            "chat_folder can't be combined with archived: a Chat folder lists its archived \
             chats among the others"
                .into(),
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|c| folder_cursor(c, folder_id, query.unread_only))
        .transpose()?;
    let folder = list_folders()
        .await?
        .into_iter()
        .find(|f| f.folder.id == folder_id)
        .ok_or_else(|| {
            TgError::InvalidInput(format!(
                "{folder_id} is not one of the account's Chat folders: ids come from \
                 whoami's chat_folders"
            ))
        })?;
    if folder.folder.rules {
        return Err(TgError::InvalidInput(RULES_NOT_SUPPORTED.into()));
    }
    let (pinned, unpinned) = members(&folder);
    let (pinned_from, after) = match &cursor {
        None => (Some(0), None),
        Some(c) => (c.pinned_from, c.after),
    };
    let pinned_from = pinned_from.map_or(pinned.len(), |k| k.min(pinned.len()));
    let wanted: Vec<ChatRef> = pinned[pinned_from..]
        .iter()
        .chain(&unpinned)
        .copied()
        .collect();
    let mut answer = DialogsAnswer::default();
    for chunk in wanted.chunks(PEER_DIALOGS_CHUNK) {
        let part = peer_dialogs(chunk.to_vec()).await?;
        answer.dialogs.extend(part.dialogs);
        answer.peers.extend(part.peers);
        answer.messages.extend(part.messages);
    }

    let peers = answer.peers.iter().map(|p| (p.id, p)).collect();
    let messages: HashMap<(ChatId, i32), &RawMessage> = answer
        .messages
        .iter()
        .map(|m| ((m.chat, m.id), m))
        .collect();
    let mut dialogs = HashMap::new();
    for dialog in &answer.dialogs {
        dialogs.entry(dialog.chat).or_insert(dialog);
    }
    let shown = |chat: &ChatRef| {
        dialogs
            .get(&chat.chat())
            .copied()
            .filter(|d| !query.unread_only || d.unread_count > 0 || d.unread_mark)
    };
    let date = |dialog: &RawDialog| {
        messages
            .get(&(dialog.chat, dialog.top_message))
            .map_or(0, |m| m.date)
    };
    // Unpinned Chats: most recent first, ties by id, so a cursor's key is a
    // place in a total order.
    let key = |date: i32, chat: ChatId| (Reverse(date), chat.to_string());
    let mut rest: Vec<(i32, ChatId, ChatSummary)> = unpinned
        .iter()
        .filter_map(shown)
        .map(|d| (date(d), d.chat, summary(d, &peers, &messages)))
        .filter(|(date, chat, _)| after.is_none_or(|(d, c)| key(*date, *chat) > key(d, c)))
        .collect();
    rest.sort_by_key(|(date, chat, _)| key(*date, *chat));

    let mut listed: Vec<(Position, ChatSummary)> = pinned
        .iter()
        .enumerate()
        .skip(pinned_from)
        .filter_map(|(i, chat)| {
            let mut chat = summary(shown(chat)?, &peers, &messages);
            chat.pinned = true;
            Some((Position::Pinned(i), chat))
        })
        .chain(rest.into_iter().map(|(date, id, mut chat)| {
            // `pinned` is the folder's pin, not the main list's.
            chat.pinned = false;
            (Position::After(date, id), chat)
        }))
        .collect();
    let more = listed.len() > query.limit;
    listed.truncate(query.limit);
    let next_cursor = listed.last().filter(|_| more).map(|(position, _)| {
        let (pinned_from, after) = match *position {
            Position::Pinned(i) => (Some(i + 1), None),
            Position::After(date, chat) => (None, Some((date, chat))),
        };
        FolderCursor {
            chat_folder: folder_id,
            unread_only: query.unread_only,
            pinned_from,
            after,
        }
        .encode()
    });
    Ok(ChatPage {
        chats: listed.into_iter().map(|(_, chat)| chat).collect(),
        next_cursor,
        unread_scan_truncated: false,
    })
}

/// Where a listed Chat is in its folder's order.
#[derive(Debug, Clone, Copy)]
enum Position {
    /// The `i`-th of the folder's pinned Chats.
    Pinned(usize),
    /// Not pinned, its last message at this date.
    After(i32, ChatId),
}

/// A folder's pinned Chats in its order and its other Chats, each once,
/// none excluded, at most [`MAX_FOLDER_CHATS`] in all.
fn members(folder: &RawChatFolder) -> (Vec<ChatRef>, Vec<ChatRef>) {
    let excluded: HashSet<ChatId> = folder.excluded.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut keep = |chat: &&ChatRef| !excluded.contains(&chat.chat()) && seen.insert(chat.chat());
    let mut pinned: Vec<ChatRef> = folder.pinned.iter().filter(&mut keep).copied().collect();
    pinned.truncate(MAX_FOLDER_CHATS);
    let mut unpinned: Vec<ChatRef> = folder.included.iter().filter(&mut keep).copied().collect();
    unpinned.truncate(MAX_FOLDER_CHATS - pinned.len());
    (pinned, unpinned)
}

/// `cursor` as a cursor of folder `folder_id`'s listing with `unread_only`
/// as it is; anything else is the caller's to fix.
fn folder_cursor(cursor: &str, folder_id: i32, unread_only: bool) -> Result<FolderCursor, TgError> {
    let Some(cursor) = FolderCursor::decode(cursor) else {
        return Err(match Cursor::decode(cursor) {
            Ok(_) => TgError::InvalidInput(
                "this cursor continues a listing without chat_folder: leave chat_folder out \
                 with it, or start over without a cursor"
                    .into(),
            ),
            Err(err) => err,
        });
    };
    if cursor.chat_folder != folder_id {
        return Err(other_folder(&cursor));
    }
    if cursor.unread_only != unread_only {
        return Err(other_unread_listing(cursor.unread_only));
    }
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::{
        limits::MethodClass,
        tg::chats::{ChatKind, RawPeer},
    };

    fn user(n: i64) -> ChatRef {
        ChatRef::new(ChatId::User(n), Some(n * 7))
    }

    /// Folder 3 with `pinned` and `included` users.
    fn folder(pinned: std::ops::Range<i64>, included: std::ops::Range<i64>) -> RawChatFolder {
        RawChatFolder {
            folder: ChatFolder {
                id: 3,
                title: "Work".into(),
                emoji: None,
                rules: false,
            },
            pinned: pinned.map(user).collect(),
            included: included.map(user).collect(),
            excluded: Vec::new(),
        }
    }

    /// Telegram's answer for `peers`: each one's dialog, its last message
    /// `n` seconds into the epoch (a higher `n` is more recent).
    fn answer(peers: &[ChatRef]) -> DialogsAnswer {
        DialogsAnswer {
            sent: peers.len(),
            dialogs: peers
                .iter()
                .map(|p| RawDialog {
                    chat: p.chat(),
                    top_message: 1,
                    unread_count: 0,
                    pinned: false,
                    folder_id: 0,
                    unread_mark: false,
                    muted: false,
                })
                .collect(),
            peers: peers
                .iter()
                .map(|p| RawPeer {
                    id: p.chat(),
                    title: p.chat().to_string(),
                    kind: ChatKind::Private,
                    username: None,
                    access_hash: p.access_hash(),
                    is_self: false,
                })
                .collect(),
            messages: peers
                .iter()
                .map(|p| {
                    let ChatId::User(n) = p.chat() else {
                        unreachable!()
                    };
                    RawMessage {
                        chat: p.chat(),
                        id: 1,
                        date: n as i32,
                        text: String::new(),
                        media: None,
                    }
                })
                .collect(),
        }
    }

    fn query(limit: usize, cursor: Option<String>) -> ChatsQuery {
        ChatsQuery {
            limit,
            cursor,
            chat_folder: Some(3),
            ..ChatsQuery::default()
        }
    }

    /// One page of `folder`, and the peers of each getPeerDialogs call.
    async fn read_page(
        folder: &RawChatFolder,
        query: ChatsQuery,
    ) -> (Result<ChatPage, TgError>, Vec<Vec<ChatRef>>) {
        let calls = RefCell::new(Vec::new());
        let page = list_folder_chats(
            &query,
            3,
            || std::future::ready(Ok(vec![folder.clone()])),
            |peers| {
                calls.borrow_mut().push(peers.clone());
                std::future::ready(Ok(answer(&peers)))
            },
        )
        .await;
        (page, calls.into_inner())
    }

    fn ids(page: &ChatPage) -> Vec<String> {
        page.chats.iter().map(|c| c.id.clone()).collect()
    }

    #[tokio::test]
    async fn a_folder_is_read_in_chunks_of_telegrams_limit_and_at_most_three() {
        let (page, calls) = read_page(&folder(1..51, 51..251), query(5, None)).await;
        let page = page.unwrap();
        assert_eq!(
            calls.iter().map(Vec::len).collect::<Vec<_>>(),
            [100, 100, 50]
        );
        // 50 pinned first, in the folder's order.
        assert_eq!(
            ids(&page),
            (1..6).map(|n| user(n).to_string()).collect::<Vec<_>>()
        );

        // Past Telegram's 200, at most 300 Chats are read: three calls.
        let (page, calls) = read_page(&folder(0..0, 1..400), query(1, None)).await;
        assert_eq!(
            calls.iter().map(Vec::len).collect::<Vec<_>>(),
            [100, 100, 100]
        );
        assert_eq!(ids(&page.unwrap()), [user(300).to_string()]);
    }

    #[tokio::test]
    async fn past_the_pinned_chats_a_page_asks_only_for_the_rest() {
        let folder = folder(1..3, 3..6);
        let (first, calls) = read_page(&folder, query(2, None)).await;
        let first = first.unwrap();
        assert_eq!(ids(&first), [user(1).to_string(), user(2).to_string()]);
        assert_eq!(calls, [(1..6).map(user).collect::<Vec<_>>()]);
        assert!(first.chats.iter().all(|c| c.pinned));

        let (next, calls) = read_page(&folder, query(2, first.next_cursor)).await;
        let next = next.unwrap();
        assert_eq!(calls, [(3..6).map(user).collect::<Vec<_>>()]);
        assert_eq!(ids(&next), [user(5).to_string(), user(4).to_string()]);
        let (last, calls) = read_page(&folder, query(2, next.next_cursor)).await;
        let last = last.unwrap();
        assert_eq!(calls, [(3..6).map(user).collect::<Vec<_>>()]);
        assert_eq!(ids(&last), [user(3).to_string()]);
        assert_eq!(last.next_cursor, None);
    }

    #[tokio::test]
    async fn a_flood_ends_the_listing_without_more_calls() {
        let mut calls = 0;
        let flooded = list_folder_chats(
            &query(5, None),
            3,
            || std::future::ready(Ok(vec![folder(0..0, 1..251)])),
            |_| {
                calls += 1;
                std::future::ready(Err(TgError::FloodWait {
                    seconds: 30,
                    class: MethodClass::Default,
                }))
            },
        )
        .await;
        assert!(matches!(
            flooded,
            Err(TgError::FloodWait { seconds: 30, .. })
        ));
        assert_eq!(calls, 1, "never retried around");
    }

    #[tokio::test]
    async fn a_wrong_cursor_or_archived_is_refused_before_asking_telegram() {
        let plain = Cursor {
            offset_date: 0,
            offset_id: 0,
            offset_peer: None,
            offset_access_hash: None,
            archived: false,
            unread_only: false,
        }
        .encode();
        let other = FolderCursor {
            chat_folder: 4,
            unread_only: false,
            pinned_from: None,
            after: None,
        }
        .encode();
        let archived = ChatsQuery {
            archived: true,
            ..query(5, None)
        };
        for query in [
            query(5, Some(plain)),
            query(5, Some(other)),
            query(5, Some("made-up".into())),
            archived,
        ] {
            let page = list_folder_chats(
                &query,
                3,
                || async { panic!("{query:?} reached Telegram") },
                |_| async { panic!("{query:?} reached Telegram") },
            )
            .await;
            assert!(matches!(page, Err(TgError::InvalidInput(_))), "{query:?}");
        }
    }

    #[test]
    fn a_folder_cursor_is_never_a_main_list_cursor_and_back() {
        let folder = FolderCursor {
            chat_folder: 3,
            unread_only: true,
            pinned_from: None,
            after: Some((1_700_000_000, ChatId::Channel(5))),
        };
        let encoded = folder.encode();
        assert_eq!(FolderCursor::decode(&encoded), Some(folder));
        assert!(Cursor::decode(&encoded).is_err());
        let plain = Cursor {
            offset_date: 1,
            offset_id: 2,
            offset_peer: Some("user:1".into()),
            offset_access_hash: None,
            archived: false,
            unread_only: true,
        };
        assert_eq!(FolderCursor::decode(&plain.encode()), None);
    }
}
