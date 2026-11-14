//! Choosing the Send allow-list on the Consent screen (ADR-0005, #44).
//!
//! With `send` ticked, the screen offers what the freshly logged-in Telegram
//! session has: its Chat folders that are explicit lists, its 100 most recent
//! Chats, and its own Chats found by name (`contacts.search`, then only those
//! with a dialog). Only what was offered can be picked: a Chat id posted that
//! the screen never showed is ignored, like a Scope it doesn't offer. A
//! folder ticked is read into its Chats when Allow is pressed.
//!
//! Everything here lives in the Login flow's memory and goes with it. Every
//! call goes to Telegram through the Account's limiter (a Live client from
//! [`crate::tg::Connector::connect_for_consent`]) and is bounded: a few calls
//! to load the screen, two per search, at most four per folder, and at most
//! [`MAX_TELEGRAM_ACTIONS`] searches and Allow attempts per Login flow. Logs
//! carry counts only, never a title, a Chat id or a query.

use std::{collections::HashSet, sync::Arc};

use crate::{
    allow_list::{AllowListError, MAX_CHATS, SendAllowList},
    tg::{
        Telegram, TgError,
        chats::{ChatId, ChatKind, ChatRef, ChatsQuery},
        folders::MAX_FOLDER_CHATS,
        sanitise::Sanitised,
    },
};

/// How many of the most recent Chats are offered (one `messages.getDialogs`).
pub const RECENT_CHATS: usize = 100;
/// How many Chats one search asks Telegram for.
pub const FOUND_PER_SEARCH: usize = 20;
/// How many Chats found by name the screen keeps offering at once.
pub const MAX_FOUND: usize = 100;
/// Searches and Allow attempts with `send` per Login flow: each reaches Telegram.
pub const MAX_TELEGRAM_ACTIONS: u32 = 30;
/// Longest name a search takes, in characters.
pub const MAX_QUERY_CHARS: usize = 64;
/// Longest title shown, in characters.
const MAX_TITLE_CHARS: usize = 80;

/// One Chat the screen offers.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatChoice {
    pub id: ChatRef,
    pub title: String,
    pub kind: ChatKind,
    /// The Account's own Saved Messages.
    pub is_self: bool,
}

/// One Chat folder the screen offers: an explicit list of Chats.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderChoice {
    pub id: i32,
    pub title: String,
    pub emoji: Option<String>,
}

/// What the screen offers, for the page to show.
#[derive(Clone, Copy)]
pub struct Choices<'a> {
    pub folders: &'a [FolderChoice],
    pub recent: &'a [ChatChoice],
    /// Chats found by name and not among `recent`.
    pub found: &'a [ChatChoice],
}

/// What the user ticked, as the Consent form posted it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    pub folders: Vec<i32>,
    pub chats: Vec<ChatRef>,
}

impl Picked {
    pub fn has_chat(&self, chat: ChatId) -> bool {
        self.chats.iter().any(|c| c.chat() == chat)
    }
}

/// Why the Consent screen can't do what was asked; the message is for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Telegram failed or asked to wait.
    Telegram(String),
    /// A search or a list that can't be accepted.
    Input(String),
}

impl Refused {
    pub fn message(&self) -> &str {
        match self {
            Refused::Telegram(message) | Refused::Input(message) => message,
        }
    }
}

/// The choices of one Login flow's Consent screen, loaded once.
pub struct Pickers {
    telegram: Sanitised,
    folders: Vec<FolderChoice>,
    recent: Vec<ChatChoice>,
    found: Vec<ChatChoice>,
    actions: u32,
}

impl Pickers {
    /// The Account's explicit-list Chat folders (`messages.getDialogFilters`)
    /// and its most recent Chats (`messages.getDialogs`), through `telegram`.
    pub async fn load(telegram: Arc<dyn Telegram>) -> Result<Self, Refused> {
        let telegram = Sanitised::new(telegram);
        let folders = telegram
            .chat_folders()
            .await
            .map_err(refused)?
            .into_iter()
            // Folders defined by rules are not lists of Chats (#40).
            .filter(|folder| !folder.rules)
            .map(|folder| FolderChoice {
                id: folder.id,
                title: shorten(&folder.title),
                emoji: folder.emoji,
            })
            .collect::<Vec<_>>();
        let recent = telegram
            .list_chats(ChatsQuery {
                limit: RECENT_CHATS,
                ..ChatsQuery::default()
            })
            .await
            .map_err(refused)?
            .chats
            .into_iter()
            .filter_map(|chat| {
                Some(ChatChoice {
                    id: chat.id.parse().ok()?,
                    title: shorten(&chat.title),
                    kind: chat.kind,
                    is_self: chat.is_self,
                })
            })
            .collect::<Vec<_>>();
        tracing::info!(
            folders = folders.len(),
            recent = recent.len(),
            "consent chat pickers loaded"
        );
        Ok(Self {
            telegram,
            folders,
            recent,
            found: Vec::new(),
            actions: 0,
        })
    }

    pub fn choices(&self) -> Choices<'_> {
        Choices {
            folders: &self.folders,
            recent: &self.recent,
            found: &self.found,
        }
    }

    /// Find the Account's own Chats named like `query`, keeping only those it
    /// has a dialog with: one `contacts.search` and one
    /// `messages.getPeerDialogs`. What the screen found before stays offered
    /// if `picked` has it, the rest makes room for the new results. Returns
    /// how many Chats the search found.
    pub async fn search(&mut self, query: &str, picked: &Picked) -> Result<usize, Refused> {
        let query = query.trim().trim_start_matches('@').trim();
        if query.is_empty() {
            return Err(Refused::Input("Type a name to search for.".into()));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(Refused::Input(format!(
                "A name to search for has at most {MAX_QUERY_CHARS} characters."
            )));
        }
        self.spend()?;
        let found = self
            .telegram
            .find_chats(query, FOUND_PER_SEARCH)
            .await
            .map_err(refused)?;
        let named: Vec<(ChatRef, ChatChoice)> = found
            .chats
            .into_iter()
            .filter_map(|chat| {
                let id: ChatRef = chat.id.parse().ok()?;
                Some((
                    id,
                    ChatChoice {
                        id,
                        title: shorten(&chat.title),
                        kind: chat.kind,
                        is_self: false,
                    },
                ))
            })
            .collect();
        let with_dialog: HashSet<ChatId> = self
            .telegram
            .existing_dialogs(named.iter().map(|(id, _)| *id).collect())
            .await
            .map_err(refused)?
            .into_iter()
            .collect();
        let results: Vec<ChatChoice> = named
            .into_iter()
            .filter(|(id, _)| with_dialog.contains(&id.chat()))
            .map(|(_, choice)| choice)
            .collect();
        let count = results.len();

        let mut found: Vec<ChatChoice> = std::mem::take(&mut self.found)
            .into_iter()
            .filter(|chat| picked.has_chat(chat.id.chat()))
            .collect();
        for chat in results {
            let chat_id = chat.id.chat();
            let shown = self
                .recent
                .iter()
                .chain(&found)
                .any(|c| c.id.chat() == chat_id);
            if !shown && found.len() < MAX_FOUND {
                found.push(chat);
            }
        }
        self.found = found;
        tracing::info!(
            found = count,
            offered = self.found.len(),
            "consent chat search"
        );
        Ok(count)
    }

    /// The Send allow-list `picked` makes: its Chats the screen offered, and
    /// the Chats of its folders the screen offered, read now (at most four
    /// calls each). Nothing chosen, or more than [`MAX_CHATS`], is refused.
    pub async fn resolve(&mut self, picked: &Picked) -> Result<SendAllowList, Refused> {
        let offered: HashSet<ChatId> = self
            .recent
            .iter()
            .chain(&self.found)
            .map(|chat| chat.id.chat())
            .collect();
        let mut chats: Vec<ChatId> = picked
            .chats
            .iter()
            .map(ChatRef::chat)
            .filter(|chat| offered.contains(chat))
            .collect();
        let folders: Vec<i32> = self
            .folders
            .iter()
            .map(|folder| folder.id)
            .filter(|id| picked.folders.contains(id))
            .collect();
        if !folders.is_empty() {
            self.spend()?;
        }
        for folder in folders {
            let page = self
                .telegram
                .list_chats(ChatsQuery {
                    limit: MAX_FOLDER_CHATS,
                    chat_folder: Some(folder),
                    ..ChatsQuery::default()
                })
                .await
                .map_err(refused)?;
            chats.extend(
                page.chats
                    .iter()
                    .filter_map(|chat| chat.id.parse::<ChatRef>().ok())
                    .map(|id| id.chat()),
            );
        }
        SendAllowList::new(chats).map_err(|err| match err {
            AllowListError::Empty => Refused::Input(
                "Choose at least one chat it may send to, or untick “Send messages”.".into(),
            ),
            AllowListError::TooMany(n) => Refused::Input(format!(
                "That is {n} chats: at most {MAX_CHATS} can be chosen. Choose fewer, or \
                 smaller folders."
            )),
        })
    }

    fn spend(&mut self) -> Result<(), Refused> {
        if self.actions >= MAX_TELEGRAM_ACTIONS {
            return Err(Refused::Input(
                "Too many searches on this page. Start again from your app.".into(),
            ));
        }
        self.actions += 1;
        Ok(())
    }
}

/// A Telegram failure as the Consent screen tells it.
pub(super) fn refused(err: TgError) -> Refused {
    match err {
        TgError::FloodWait { seconds, .. } | TgError::RateLimited { seconds } => {
            tracing::warn!(seconds, "telegram limits the consent chat pickers");
            Refused::Telegram(format!(
                "Telegram asks to slow down. Try again in {seconds} seconds."
            ))
        }
        TgError::InvalidInput(message) => Refused::Input(message),
        err => {
            tracing::warn!(error = %err, "consent chat pickers failed");
            Refused::Telegram("Telegram didn't answer. Try again.".into())
        }
    }
}

fn shorten(title: &str) -> String {
    match title.char_indices().nth(MAX_TITLE_CHARS) {
        Some((at, _)) => format!("{}…", &title[..at]),
        None => title.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::{
        Account, FakeTelegram,
        chats::ChatSummary,
        fake::{FakeChatFolder, FakeData},
    };

    fn chat(id: ChatId, title: &str) -> ChatSummary {
        ChatSummary {
            id: id.to_string(),
            title: title.into(),
            kind: ChatKind::Private,
            username: None,
            is_self: false,
            unread_count: 0,
            pinned: false,
            muted: false,
            marked_unread: false,
            last_message: None,
        }
    }

    fn me() -> Account {
        Account {
            id: 42,
            first_name: "Me".into(),
            last_name: None,
            username: None,
        }
    }

    async fn pickers(data: FakeData) -> Pickers {
        Pickers::load(Arc::new(FakeTelegram::new(data)))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_search_needs_a_name_of_bounded_length_and_a_few_per_login() {
        let data = FakeData::new(me()).with_chats(vec![chat(ChatId::User(7), "Ann")]);
        let mut pickers = pickers(data).await;
        let picked = Picked::default();
        for blank in ["", "  ", "@", " @ "] {
            assert!(matches!(
                pickers.search(blank, &picked).await,
                Err(Refused::Input(_))
            ));
        }
        let long = "a".repeat(MAX_QUERY_CHARS + 1);
        assert!(matches!(
            pickers.search(&long, &picked).await,
            Err(Refused::Input(_))
        ));
        // Those reached nothing and spent nothing.
        for _ in 0..MAX_TELEGRAM_ACTIONS {
            assert_eq!(pickers.search("@Ann", &picked).await, Ok(1));
        }
        let refused = pickers.search("Ann", &picked).await.unwrap_err();
        assert!(
            refused.message().contains("Too many searches"),
            "{refused:?}"
        );
        // A found chat is offered once, and not again beside the recent ones.
        assert!(pickers.choices().found.is_empty(), "Ann is a recent chat");
    }

    #[tokio::test]
    async fn too_many_chats_from_folders_is_refused() {
        let users: Vec<ChatId> = (1..=1200).map(ChatId::User).collect();
        let folders = users
            .chunks(300)
            .enumerate()
            .map(|(i, chunk)| FakeChatFolder {
                id: i as i32 + 2,
                title: format!("F{i}"),
                emoji: None,
                rules: false,
                pinned: Vec::new(),
                included: chunk.to_vec(),
                excluded: Vec::new(),
            })
            .collect();
        let data = FakeData::new(me())
            .with_chats(users.iter().map(|&u| chat(u, "x")).collect())
            .with_chat_folders(folders);
        let mut pickers = pickers(data).await;
        assert_eq!(pickers.choices().folders.len(), 4);
        assert_eq!(pickers.choices().recent.len(), RECENT_CHATS);

        let three = Picked {
            folders: vec![2, 3, 4],
            ..Picked::default()
        };
        assert_eq!(pickers.resolve(&three).await.unwrap().len(), 900);
        let all = Picked {
            folders: vec![2, 3, 4, 5],
            ..Picked::default()
        };
        let refused = pickers.resolve(&all).await.unwrap_err();
        assert!(
            refused.message().contains("That is 1200 chats"),
            "{refused:?}"
        );
    }
}
