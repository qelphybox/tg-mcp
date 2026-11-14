//! Chat folders: the Account's named groupings of Chats (`messages.getDialogFilters`),
//! listed by whoami, and the Chats of one defined as an explicit list, listed
//! by list_chats `chat_folder`.
//!
//! The Telegram adapter extracts [`RawChatFolder`]s; which Chats a folder
//! lists, in which order, and where its pages continue is decided here.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use tokio::time::Instant;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{
        ChatId, ChatPage, ChatRef, ChatsQuery, Cursor, DialogsAnswer, RawDialog, RawMessage,
        other_unread_listing, summary,
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
/// folder, 200 with Premium, so reading a folder whole takes at most 3
/// `messages.getPeerDialogs`.
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

/// How long a Live client trusts the folders' definitions it remembers from
/// the last `messages.getDialogFilters` (whoami's or list_chats').
pub const FOLDER_DEFINITIONS_TTL: Duration = Duration::from_secs(60);
/// How long a folder listing's order, read on its first page, serves its
/// later pages.
pub const FOLDER_LISTING_TTL: Duration = Duration::from_secs(120);
/// Telegram allows 30 folders with Premium.
const MAX_REMEMBERED_FOLDERS: usize = 64;
/// Listings (folder, `unread_only`) remembered at once; the oldest goes.
const MAX_REMEMBERED_LISTINGS: usize = 8;

/// What a Live client remembers of the Account's Chat folders between calls,
/// so paging a folder doesn't read it whole for every page: which Chats each
/// folder has and whether it has rules, and the order of the listings being
/// paged. Identifiers, dates and positions only: no title, emoji or message
/// content is kept. In memory, for as long as the Live client lives.
#[derive(Debug, Default)]
pub struct FolderCache(Mutex<Remembered>);

#[derive(Debug, Default)]
struct Remembered {
    /// When the definitions were read, and each folder's.
    definitions: Option<(Instant, HashMap<i32, Definition>)>,
    listings: Vec<Snapshot>,
}

/// Which Chats a folder lists: its pinned ones in its order, then the others,
/// each once, none excluded, at most [`MAX_FOLDER_CHATS`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Definition {
    rules: bool,
    pinned: Vec<ChatRef>,
    unpinned: Vec<ChatRef>,
}

impl Definition {
    fn of(folder: &RawChatFolder) -> Self {
        let excluded: HashSet<ChatId> = folder.excluded.iter().copied().collect();
        let mut seen = HashSet::new();
        let mut keep =
            |chat: &&ChatRef| !excluded.contains(&chat.chat()) && seen.insert(chat.chat());
        let mut pinned: Vec<ChatRef> = folder.pinned.iter().filter(&mut keep).copied().collect();
        pinned.truncate(MAX_FOLDER_CHATS);
        let mut unpinned: Vec<ChatRef> =
            folder.included.iter().filter(&mut keep).copied().collect();
        unpinned.truncate(MAX_FOLDER_CHATS - pinned.len());
        Self {
            rules: folder.folder.rules,
            pinned,
            unpinned,
        }
    }
}

/// A listing's order as its first page (or a page that read the folder
/// whole) found it: the Chats it lists, each with its place.
#[derive(Debug, Clone)]
struct Snapshot {
    folder: i32,
    unread_only: bool,
    taken: Instant,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    chat: ChatRef,
    position: Position,
}

impl FolderCache {
    fn state(&self) -> std::sync::MutexGuard<'_, Remembered> {
        self.0.lock().expect("folder cache lock")
    }

    /// Remember a `messages.getDialogFilters` answer's folders (their Chats
    /// and rules only), in place of what was remembered.
    pub fn remember(&self, folders: &[RawChatFolder]) {
        let definitions = folders
            .iter()
            .take(MAX_REMEMBERED_FOLDERS)
            .map(|f| (f.folder.id, Definition::of(f)))
            .collect();
        self.state().definitions = Some((Instant::now(), definitions));
    }

    /// Folder `id`'s definition, if remembered within the TTL.
    fn definition(&self, id: i32) -> Option<Definition> {
        let state = self.state();
        let (read, definitions) = state.definitions.as_ref()?;
        if read.elapsed() >= FOLDER_DEFINITIONS_TTL {
            return None;
        }
        definitions.get(&id).cloned()
    }

    fn snapshot(&self, folder: i32, unread_only: bool) -> Option<Vec<Entry>> {
        self.state()
            .listings
            .iter()
            .find(|s| s.folder == folder && s.unread_only == unread_only)
            .filter(|s| s.taken.elapsed() < FOLDER_LISTING_TTL)
            .map(|s| s.entries.clone())
    }

    fn keep_snapshot(&self, folder: i32, unread_only: bool, entries: Vec<Entry>) {
        let mut state = self.state();
        let listings = &mut state.listings;
        listings.retain(|s| {
            !(s.folder == folder && s.unread_only == unread_only)
                && s.taken.elapsed() < FOLDER_LISTING_TTL
        });
        if listings.len() >= MAX_REMEMBERED_LISTINGS {
            listings.remove(0);
        }
        listings.push(Snapshot {
            folder,
            unread_only,
            taken: Instant::now(),
            entries,
        });
    }
}

/// One page of the Chats of Chat folder `folder_id`, for list_chats: the
/// folder's pinned Chats in its order, then the others most recent first,
/// each once, `query.unread_only` keeping only unread or marked-unread ones.
/// A Chat Telegram answers no dialog for (left, deleted, not addressable) is
/// not listed, as in the app. Archived Chats are listed among the others: a
/// folder leaves them out only by a rule.
///
/// Telegram calls, each through `list_folders` (one
/// `messages.getDialogFilters`) or `peer_dialogs` (one
/// `messages.getPeerDialogs` of at most [`PEER_DIALOGS_CHUNK`] Chats):
/// - a first page, or a later one whose listing `cache` no longer has: the
///   folder's definition if `cache` has none within its TTL (or doesn't know
///   the id), then every Chat of the folder still to come, read to sort them
///   (≤ 1 + 3 calls); the order found is remembered for later pages;
/// - a later page within [`FOLDER_LISTING_TTL`]: one call for only that
///   page's Chats, in the remembered order, so previews, unread counts and
///   flags are fresh. A Chat gone (or, with `unread_only`, read) meanwhile is
///   left out, so a page may hold fewer than `limit`; one that became unread
///   after the first page waits for the next listing.
pub async fn list_folder_chats<L, LFut, P, PFut>(
    query: &ChatsQuery,
    folder_id: i32,
    cache: &FolderCache,
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
    let cursor = match query.cursor.as_deref() {
        Some(c) => folder_cursor(c, folder_id, query.unread_only)?,
        None => FolderCursor {
            chat_folder: folder_id,
            unread_only: query.unread_only,
            pinned_from: Some(0),
            after: None,
        },
    };
    let remembered = query
        .cursor
        .is_some()
        .then(|| cache.snapshot(folder_id, query.unread_only))
        .flatten();
    let (mut entries, read) = match remembered {
        Some(entries) => (entries, None),
        None => {
            let definition = match cache.definition(folder_id) {
                Some(definition) => definition,
                // Not remembered, stale, or a folder made since: ask again.
                None => {
                    let folders = list_folders().await?;
                    cache.remember(&folders);
                    folders
                        .iter()
                        .find(|f| f.folder.id == folder_id)
                        .map(Definition::of)
                        .ok_or_else(|| {
                            TgError::InvalidInput(format!(
                                "{folder_id} is not one of the account's Chat folders: ids \
                                 come from whoami's chat_folders"
                            ))
                        })?
                }
            };
            if definition.rules {
                return Err(TgError::InvalidInput(RULES_NOT_SUPPORTED.into()));
            }
            let (entries, answer) =
                read_whole(&definition, &cursor, query.unread_only, &mut peer_dialogs).await?;
            cache.keep_snapshot(folder_id, query.unread_only, entries.clone());
            (entries, Some(answer))
        }
    };
    entries.retain(|e| e.position.comes_after(&cursor));
    let more = entries.len() > query.limit;
    entries.truncate(query.limit);
    let answer = match read {
        Some(answer) => answer,
        None => read_chats(entries.iter().map(|e| e.chat).collect(), &mut peer_dialogs).await?,
    };

    let peers = answer.peers.iter().map(|p| (p.id, p)).collect();
    let messages: HashMap<(ChatId, i32), &RawMessage> = answer
        .messages
        .iter()
        .map(|m| ((m.chat, m.id), m))
        .collect();
    let dialogs = dialogs_by_chat(&answer, query.unread_only);
    let chats = entries
        .iter()
        .filter_map(|entry| {
            let mut chat = summary(dialogs.get(&entry.chat.chat())?, &peers, &messages);
            // `pinned` is the folder's pin, not the main list's.
            chat.pinned = matches!(entry.position, Position::Pinned(_));
            Some(chat)
        })
        .collect();
    let next_cursor = entries.last().filter(|_| more).map(|last| {
        let (pinned_from, after) = match last.position {
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
        chats,
        next_cursor,
        unread_scan_truncated: false,
    })
}

/// Every Chat of the folder that can still come after `cursor`, read (in
/// chunks) and put in the listing's order: pinned ones in the folder's order,
/// then the others most recent first, ties by id. Only Chats Telegram has a
/// dialog for, and with `unread_only` only unread ones.
async fn read_whole<P, PFut>(
    definition: &Definition,
    cursor: &FolderCursor,
    unread_only: bool,
    peer_dialogs: &mut P,
) -> Result<(Vec<Entry>, DialogsAnswer), TgError>
where
    P: FnMut(Vec<ChatRef>) -> PFut,
    PFut: Future<Output = Result<DialogsAnswer, TgError>>,
{
    let pinned_from = cursor
        .pinned_from
        .map_or(definition.pinned.len(), |k| k.min(definition.pinned.len()));
    let wanted = definition.pinned[pinned_from..]
        .iter()
        .chain(&definition.unpinned)
        .copied()
        .collect();
    let answer = read_chats(wanted, peer_dialogs).await?;
    let dialogs = dialogs_by_chat(&answer, unread_only);
    let dates: HashMap<(ChatId, i32), i32> = answer
        .messages
        .iter()
        .map(|m| ((m.chat, m.id), m.date))
        .collect();
    let date = |dialog: &RawDialog| {
        dates
            .get(&(dialog.chat, dialog.top_message))
            .copied()
            .unwrap_or(0)
    };
    let mut unpinned: Vec<Entry> = definition
        .unpinned
        .iter()
        .filter_map(|chat| {
            let dialog = dialogs.get(&chat.chat())?;
            Some(Entry {
                chat: *chat,
                position: Position::After(date(dialog), chat.chat()),
            })
        })
        .collect();
    unpinned.sort_by_key(|e| e.position.key());
    let entries = definition
        .pinned
        .iter()
        .enumerate()
        .skip(pinned_from)
        .filter(|(_, chat)| dialogs.contains_key(&chat.chat()))
        .map(|(i, chat)| Entry {
            chat: *chat,
            position: Position::Pinned(i),
        })
        .chain(unpinned)
        .collect();
    Ok((entries, answer))
}

/// The dialogs of `chats`, one `peer_dialogs` call per
/// [`PEER_DIALOGS_CHUNK`] of them, none for no Chats.
async fn read_chats<P, PFut>(
    chats: Vec<ChatRef>,
    peer_dialogs: &mut P,
) -> Result<DialogsAnswer, TgError>
where
    P: FnMut(Vec<ChatRef>) -> PFut,
    PFut: Future<Output = Result<DialogsAnswer, TgError>>,
{
    let mut answer = DialogsAnswer::default();
    for chunk in chats.chunks(PEER_DIALOGS_CHUNK) {
        let part = peer_dialogs(chunk.to_vec()).await?;
        answer.sent += part.sent;
        answer.dialogs.extend(part.dialogs);
        answer.peers.extend(part.peers);
        answer.messages.extend(part.messages);
    }
    Ok(answer)
}

/// Each Chat's dialog in `answer`, with `unread_only` only unread or
/// marked-unread ones.
fn dialogs_by_chat(answer: &DialogsAnswer, unread_only: bool) -> HashMap<ChatId, &RawDialog> {
    let mut dialogs = HashMap::new();
    for dialog in &answer.dialogs {
        if !unread_only || dialog.unread_count > 0 || dialog.unread_mark {
            dialogs.entry(dialog.chat).or_insert(dialog);
        }
    }
    dialogs
}

/// Where a listed Chat is in its folder's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// The `i`-th of the folder's pinned Chats.
    Pinned(usize),
    /// Not pinned, its last message at this date.
    After(i32, ChatId),
}

impl Position {
    /// Unpinned Chats: most recent first, ties by id, so a cursor's key is a
    /// place in a total order.
    fn key(self) -> (Reverse<i32>, String) {
        match self {
            Self::Pinned(_) => (Reverse(i32::MAX), String::new()),
            Self::After(date, chat) => (Reverse(date), chat.to_string()),
        }
    }

    fn comes_after(self, cursor: &FolderCursor) -> bool {
        match self {
            Self::Pinned(i) => cursor.pinned_from.is_some_and(|k| i >= k),
            Self::After(..) => cursor
                .after
                .is_none_or(|(date, chat)| self.key() > Self::After(date, chat).key()),
        }
    }
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

    /// One page of `folder` with nothing remembered, and the peers of each
    /// getPeerDialogs call.
    async fn read_page(
        folder: &RawChatFolder,
        query: ChatsQuery,
    ) -> (Result<ChatPage, TgError>, Vec<Vec<ChatRef>>) {
        let (page, filters, calls) =
            read_with(&FolderCache::default(), folder, query, &HashSet::new()).await;
        assert_eq!(filters, 1, "nothing remembered: the folder is read");
        (page, calls)
    }

    /// One page of `folder` with `cache`, `unread` users unread: the page,
    /// how many getDialogFilters and the peers of each getPeerDialogs call.
    async fn read_with(
        cache: &FolderCache,
        folder: &RawChatFolder,
        query: ChatsQuery,
        unread: &HashSet<i64>,
    ) -> (Result<ChatPage, TgError>, usize, Vec<Vec<ChatRef>>) {
        let filters = RefCell::new(0);
        let calls = RefCell::new(Vec::new());
        let page = list_folder_chats(
            &query,
            3,
            cache,
            || {
                *filters.borrow_mut() += 1;
                std::future::ready(Ok(vec![folder.clone()]))
            },
            |peers| {
                calls.borrow_mut().push(peers.clone());
                let mut answer = answer(&peers);
                for dialog in &mut answer.dialogs {
                    if let ChatId::User(n) = dialog.chat {
                        dialog.unread_count = i32::from(unread.contains(&n));
                    }
                }
                std::future::ready(Ok(answer))
            },
        )
        .await;
        (page, filters.into_inner(), calls.into_inner())
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

    /// Walks folder `folder` with `limit` and `cache` to its end: each page's
    /// Chats, getDialogFilters count and getPeerDialogs calls.
    async fn walk(
        cache: &FolderCache,
        folder: &RawChatFolder,
        limit: usize,
    ) -> Vec<(Vec<String>, usize, Vec<Vec<ChatRef>>)> {
        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let (page, filters, calls) =
                read_with(cache, folder, query(limit, cursor), &HashSet::new()).await;
            let page = page.unwrap();
            pages.push((ids(&page), filters, calls));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return pages,
            }
            assert!(pages.len() < 100, "the walk never ends");
        }
    }

    #[tokio::test]
    async fn a_walk_reads_the_folder_once_then_only_each_pages_chats() {
        // 3 pinned, 37 more, most recent last: 40 Chats, 14 pages of 3.
        let folder = folder(1..4, 4..41);
        let cache = FolderCache::default();
        let pages = walk(&cache, &folder, 3).await;

        let order: Vec<i64> = (1..4).chain((4..41).rev()).collect();
        let seen: Vec<String> = pages.iter().flat_map(|(ids, ..)| ids.clone()).collect();
        assert_eq!(
            seen,
            order
                .iter()
                .map(|&n| user(n).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(pages.len(), 14);
        // First page: the folder, then all its Chats in one call.
        assert_eq!(pages[0].1, 1);
        assert_eq!(pages[0].2.len(), 1);
        assert_eq!(pages[0].2[0].len(), 40);
        // Every later page: one getPeerDialogs of exactly its own Chats.
        for (ids, filters, calls) in &pages[1..] {
            assert_eq!(*filters, 0, "{ids:?}");
            assert_eq!(calls.len(), 1, "{ids:?}");
            assert_eq!(
                calls[0].iter().map(ToString::to_string).collect::<Vec<_>>(),
                *ids
            );
        }

        // whoami's getDialogFilters spares the next listing its own.
        let cache = FolderCache::default();
        cache.remember(std::slice::from_ref(&folder));
        let (_, filters, calls) = read_with(&cache, &folder, query(3, None), &HashSet::new()).await;
        assert_eq!((filters, calls.len()), (0, 1));
    }

    #[tokio::test]
    async fn an_unknown_folder_is_asked_for_again_even_when_the_definitions_are_fresh() {
        let cache = FolderCache::default();
        cache.remember(&[folder(1..2, 2..3)]);
        let unknown = ChatsQuery {
            chat_folder: Some(9),
            ..query(3, None)
        };
        let filters = RefCell::new(0);
        let page = list_folder_chats(
            &unknown,
            9,
            &cache,
            || {
                *filters.borrow_mut() += 1;
                std::future::ready(Ok(vec![folder(1..2, 2..3)]))
            },
            |_| async { panic!("an unknown folder has no Chats to read") },
        )
        .await;
        assert!(matches!(page, Err(TgError::InvalidInput(_))));
        assert_eq!(filters.into_inner(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn past_its_ttl_a_listing_reads_the_folder_again_and_goes_on_after_its_cursor() {
        let folder = folder(1..2, 2..11);
        let cache = FolderCache::default();
        let none = HashSet::new();
        let (first, ..) = read_with(&cache, &folder, query(3, None), &none).await;
        let first = first.unwrap();
        assert_eq!(ids(&first), [1, 10, 9].map(|n| user(n).to_string()));

        // The definitions are stale, the listing's order isn't: one call.
        tokio::time::advance(FOLDER_DEFINITIONS_TTL).await;
        let (second, filters, calls) =
            read_with(&cache, &folder, query(3, first.next_cursor), &none).await;
        let second = second.unwrap();
        assert_eq!(ids(&second), [8, 7, 6].map(|n| user(n).to_string()));
        assert_eq!((filters, calls.len()), (0, 1));

        // Both stale: the folder and every Chat still to come are read again.
        tokio::time::advance(FOLDER_LISTING_TTL).await;
        let (third, filters, calls) =
            read_with(&cache, &folder, query(3, second.next_cursor), &none).await;
        let third = third.unwrap();
        assert_eq!(ids(&third), [5, 4, 3].map(|n| user(n).to_string()));
        assert_eq!(filters, 1);
        assert_eq!(calls, [(2..11).map(user).collect::<Vec<_>>()]);
        // ... and that read serves the next page.
        let (last, filters, calls) =
            read_with(&cache, &folder, query(3, third.next_cursor), &none).await;
        assert_eq!(ids(&last.unwrap()), [user(2).to_string()]);
        assert_eq!((filters, calls), (0, vec![vec![user(2)]]));
    }

    #[tokio::test]
    async fn unread_only_keeps_its_first_pages_unread_chats_and_drops_those_read_since() {
        let folder = folder(0..0, 1..11);
        let cache = FolderCache::default();
        let unread: HashSet<i64> = [2, 4, 6, 8, 10].into();
        let unread_query = |cursor| ChatsQuery {
            unread_only: true,
            ..query(2, cursor)
        };
        let (first, ..) = read_with(&cache, &folder, unread_query(None), &unread).await;
        let first = first.unwrap();
        assert_eq!(ids(&first), [10, 8].map(|n| user(n).to_string()));

        // 6 was read meanwhile and 9 got unread: the page asks for 6 and 4
        // only, shows 4, and goes on to 2; 9 waits for the next listing.
        let now: HashSet<i64> = [2, 4, 9].into();
        let (second, filters, calls) =
            read_with(&cache, &folder, unread_query(first.next_cursor), &now).await;
        let second = second.unwrap();
        assert_eq!(ids(&second), [user(4).to_string()]);
        assert_eq!((filters, calls), (0, vec![vec![user(6), user(4)]]));
        let (last, _, calls) =
            read_with(&cache, &folder, unread_query(second.next_cursor), &now).await;
        let last = last.unwrap();
        assert_eq!(ids(&last), [user(2).to_string()]);
        assert_eq!(calls, [vec![user(2)]]);
        assert_eq!(last.next_cursor, None);
    }

    #[tokio::test]
    async fn a_flood_ends_the_listing_without_more_calls() {
        let mut calls = 0;
        let flooded = list_folder_chats(
            &query(5, None),
            3,
            &FolderCache::default(),
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
            pinned_from: None,
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
                &FolderCache::default(),
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
            pinned_from: None,
        };
        assert_eq!(FolderCursor::decode(&plain.encode()), None);
    }
}
