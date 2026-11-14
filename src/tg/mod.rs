//! The `tg` boundary: everything the server needs from Telegram, behind traits.
//!
//! - [`Login`] / [`LoginAttempt`]: the Telegram part of the Login flow (QR login, then
//!   the Cloud password if the Account has one).
//! - [`Connector`]: turns an opened Grant into a Live client.
//! - [`Telegram`]: calls on one Account: reading, writing Drafts, sending messages,
//!   reacting and marking as read.
//!
//! Tests drive the whole server through the fakes in [`fake`]; [`grammers`] is the
//! real Telegram (QR login + Client pool).

pub mod chats;
pub mod compose;
pub mod document;
pub mod download;
pub mod drafts;
pub mod fake;
pub mod find;
pub mod folders;
pub mod grammers;
pub mod mark_read;
pub mod markdown;
pub mod media;
pub mod members;
pub mod messages;
mod pool;
pub mod reactions;
pub mod rich;
pub mod sanitise;
pub mod search;
pub mod send;
mod session;
pub mod topics;
pub mod transcribe;
pub mod view;

pub use fake::{FakeConnector, FakeLogin, FakePassword, FakeTelegram};

use std::{fmt, sync::Arc, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    limits::{MethodClass, WriteLimit},
    sessions::{GrantId, OpenedGrant},
};

/// The Telegram Account a Grant reads on behalf of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    /// Telegram user id.
    pub id: i64,
    pub first_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Public @username, without the `@`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Account {
    pub fn display_name(&self) -> String {
        match &self.last_name {
            Some(last) => format!("{} {last}", self.first_name),
            None => self.first_name.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    /// Telegram is unreachable or answered with something we can't use.
    #[error("telegram request failed: {0}")]
    Upstream(String),
    /// The stored Telegram session can't be used (corrupt, or logged out elsewhere).
    #[error("telegram session is not usable: {0}")]
    BadSession(String),
    /// The Cloud password didn't match.
    #[error("the cloud password is wrong")]
    WrongPassword,
    /// Telegram's `FLOOD_WAIT`: nothing of `class` may be retried before `seconds`
    /// pass. Other classes of the same Account are not blocked by it.
    #[error("telegram asks to wait {seconds} s")]
    FloodWait { seconds: u32, class: MethodClass },
    /// Our own per-Account request budget is used up; retry after `seconds`.
    #[error("this account's request budget is used up; retry in {seconds} s")]
    RateLimited { seconds: u32 },
    /// A write refused by our Writes budget or a Chat's Send cap (ADR-0005):
    /// nothing reached Telegram; retry after `seconds`. Writes are never queued.
    #[error("too many writes ({}); retry in {seconds} s", limit.name())]
    WritesLimited { seconds: u32, limit: WriteLimit },
    /// Telegram's `SLOWMODE_WAIT_X`: the group is in slow mode, and this
    /// member may send its next message there in `seconds`. Limits one Chat,
    /// not the Account; never retried.
    #[error("this chat is in slow mode; the next message may go in {seconds} s")]
    SlowMode { seconds: u32 },
    /// Telegram's `PEER_FLOOD`: the Account is limited for spam and may not
    /// write there now. Not a wait: never retried.
    #[error("telegram limits this account for spam (PEER_FLOOD)")]
    PeerFlood,
    /// The caller passed something malformed (bad chat id, foreign cursor).
    #[error("{0}")]
    InvalidInput(String),
}

/// Access to one Account: reading, and the writes its Grant's Scopes allow
/// (the MCP layer checks those before calling a write).
#[async_trait::async_trait]
pub trait Telegram: Send + Sync + 'static {
    async fn whoami(&self) -> Result<Account, TgError>;

    /// The Account's Chat folders, in the user's order
    /// (`messages.getDialogFilters`): neither the Archive nor the "All chats"
    /// entry is one.
    async fn chat_folders(&self) -> Result<Vec<folders::ChatFolder>, TgError>;

    /// One page of the Account's Chats in the main list or the Archive: pinned
    /// first on the first page, then most recent first, each Chat once. At most
    /// `query.limit` Chats, and `next_cursor` continues right after the last
    /// one, so callers must not cut the page. `query.cursor` is a
    /// `next_cursor` from a previous page (it also decides `archived`). With
    /// `unread_only`, only unread Chats, scanned for by [`chats::list_chats`].
    /// With `chat_folder`, the Chats of that Chat folder instead
    /// ([`folders::list_folder_chats`]).
    async fn list_chats(&self, query: chats::ChatsQuery) -> Result<chats::ChatPage, TgError>;

    /// Messages of one Chat, newest first, within `query`'s bounds.
    async fn get_messages(
        &self,
        chat: chats::ChatRef,
        query: messages::HistoryQuery,
    ) -> Result<messages::MessagePage, TgError>;

    /// Details of one Chat: title, type, @username, description, member count;
    /// for a user, the profile Telegram shows the Account
    /// ([`chats::user_info`]), without `common_chats`.
    async fn get_chat_info(&self, chat: chats::ChatRef) -> Result<chats::ChatInfo, TgError>;

    /// The groups and channels the Account shares with a user
    /// (`messages.getCommonChats`), at most `limit`
    /// ([`find::MAX_COMMON_CHATS`]). Not a user is [`TgError::InvalidInput`].
    async fn common_chats(
        &self,
        user: chats::ChatRef,
        limit: usize,
    ) -> Result<find::FoundChats, TgError>;

    /// One page of a group's, supergroup's or channel's members
    /// ([`members::get_members`]): Telegram's listing for a supergroup or
    /// channel, ours over the whole list for a basic group and for admins. A
    /// user is [`TgError::InvalidInput`], and so is a Chat whose members
    /// Telegram doesn't show the Account.
    async fn get_members(
        &self,
        query: members::MembersQuery,
    ) -> Result<members::MemberPage, TgError>;

    /// One page of Search hits, Telegram's search across the Account's Chats or
    /// in one of them, newest first. At most `query.limit` hits; `next_cursor`
    /// continues right after the last one.
    async fn search_messages(
        &self,
        query: search::SearchQuery,
    ) -> Result<search::SearchPage, TgError>;

    /// The Account's own Chats and contacts whose name, title or @username
    /// matches `query` (trimmed, without a leading `@`), in Telegram's order,
    /// each once, at most `limit`. Public Chats the Account isn't in are never
    /// returned.
    async fn find_chats(&self, query: &str, limit: usize) -> Result<find::FoundChats, TgError>;

    /// Which of `chats` the Account has a dialog with, in its main list or
    /// the Archive (one `messages.getPeerDialogs`), in the order given: a
    /// contact it never wrote to, a Chat it left or a wrong access hash is
    /// left out. At most [`folders::PEER_DIALOGS_CHUNK`] Chats, else
    /// [`TgError::InvalidInput`]; none makes no call.
    async fn existing_dialogs(
        &self,
        chats: Vec<chats::ChatRef>,
    ) -> Result<Vec<chats::ChatId>, TgError>;

    /// The file of message `message_id`'s media in a Chat, fetched by id
    /// ([`download::locate`]): what it is (as get_messages describes it),
    /// its size and where it is. Not found, no media, or media without a
    /// file is [`TgError::InvalidInput`].
    async fn media_file(
        &self,
        chat: chats::ChatRef,
        message_id: i32,
    ) -> Result<download::MediaFile, TgError>;

    /// Message `message_id` of a Chat and, when it is in an album, the
    /// album's other messages, each with what a view can show of its media
    /// ([`view::locate_album`]: one `getMessages` call for the ids around
    /// it). Not found or no media is [`TgError::InvalidInput`].
    async fn media_album(
        &self,
        chat: chats::ChatRef,
        message_id: i32,
    ) -> Result<view::Album, TgError>;

    /// Telegram's transcription of message `message_id`'s voice message or
    /// video message ([`transcribe::transcribe`]): one `getMessages` for it
    /// (the Chat's own message only), then one `messages.transcribeAudio`
    /// (method class Transcribe), then the final text from Telegram's
    /// updates until `deadline`, else [`transcribe::Transcript::Pending`].
    /// Another kind of media is [`TgError::InvalidInput`] without asking
    /// Telegram, and so is Telegram's refusal ([`transcribe::Refusal`]).
    async fn transcribe(
        &self,
        chat: chats::ChatRef,
        message_id: i32,
        deadline: tokio::time::Instant,
    ) -> Result<transcribe::Transcription, TgError>;

    /// One `upload.getFile` call, in method class Files: through the
    /// Account's limiter, its `FLOOD_WAIT` honoured. [`download::Download`]
    /// makes these calls.
    async fn get_file(
        &self,
        request: download::FileRequest,
    ) -> Result<download::FilePart, download::FileError>;

    /// The Account's Drafts, newest first ([`drafts::assemble`]): one
    /// `messages.getAllDrafts` call.
    async fn get_drafts(&self) -> Result<drafts::Drafts, TgError>;

    /// Put `draft` in its Chat's input field on all the Account's devices
    /// (`messages.saveDraft`, method class Writes, so it spends the Writes
    /// budget), replacing the Draft there; empty text clears it. Where it
    /// goes is [`compose::place`]'s rule. Any Chat of the Account's; a Chat
    /// it doesn't have, or can't write in, is [`TgError::InvalidInput`].
    async fn save_draft(&self, draft: drafts::NewDraft) -> Result<drafts::SavedDraft, TgError>;

    /// Send `message` as the Account (`messages.sendMessage`, method class
    /// Writes, counted against its Chat's Send cap when it goes to Telegram).
    /// Where it goes is [`compose::place`]'s rule, as for a Draft. A
    /// `random_id` Telegram already has is [`send::already_sent`]: never sent
    /// twice. The MCP layer checks the Send allow-list before calling this;
    /// nothing here does.
    async fn send_message(&self, message: send::NewMessage) -> Result<send::SentMessage, TgError>;

    /// Set the Account's reaction on one message (`messages.sendReaction`,
    /// method class Writes, not counted against the Chat's Send cap),
    /// replacing its previous one there; `emoji: None` removes it. The emoji
    /// must be one of Telegram's standard reactions
    /// ([`reactions::standard`], against Telegram's list, fetched once per
    /// Live client); setting the reaction already there is no error. A Chat
    /// that doesn't allow it, or a message it doesn't have, is
    /// [`TgError::InvalidInput`]. The MCP layer checks the Send allow-list
    /// before calling this; nothing here does.
    async fn react(&self, reaction: reactions::NewReaction) -> Result<reactions::Reacted, TgError>;

    /// Mark a Chat read up to a message, or its latest
    /// (`messages.readHistory` / `channels.readHistory`), or with `topic`
    /// only that Topic of a forum (`messages.readDiscussion`, up to the
    /// Topic's latest message unless `up_to` says otherwise). Method class
    /// Writes, not counted against the Chat's Send cap. `topic` on a Chat
    /// that is no forum, or a Topic it doesn't have, is
    /// [`TgError::InvalidInput`]. The MCP layer checks the Send allow-list
    /// before calling this; nothing here does.
    async fn mark_read(&self, read: mark_read::MarkRead) -> Result<mark_read::MarkedRead, TgError>;
}

/// Turns a Grant opened with a valid Token into a Live client for its Account.
/// Takes the Grant: a new Live client keeps it (and its Session key) for as long
/// as it lives, to re-seal the Telegram session if it changes.
#[async_trait::async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError>;

    /// A Live client for a Telegram session just logged in by the Login flow,
    /// before any Grant exists: for the Consent screen's chat pickers. Its
    /// calls go through the Account's limiter like any Live client's; it is
    /// not pooled, dropping it closes its connection, and changes to its
    /// session are not kept (the Grant gets the login's own).
    async fn connect_for_consent(&self, logged_in: &LoggedIn)
    -> Result<Arc<dyn Telegram>, TgError>;

    /// End the Grant's Telegram session (`auth.logOut`, so it leaves the Account's
    /// Active sessions) and drop its Live client. A session Telegram already
    /// considers dead counts as logged out.
    async fn log_out(&self, grant: OpenedGrant) -> Result<(), TgError>;

    /// Best effort for a Grant revoked without a Token (a stolen refresh Token,
    /// ADR-0003): if the Client pool holds its Live client, drop it and end its
    /// Telegram session (`auth.logOut`) with the session in its memory. `Ok(false)`
    /// when there is none: nothing is called, and the Telegram session stays in
    /// the Account's Active sessions, since only a Token could open it. Once
    /// evicted, the Live client is gone even if logging out fails.
    async fn evict_and_log_out(&self, grant_id: &GrantId) -> Result<bool, TgError>;

    /// End a Telegram session the Login flow just made that no Grant holds
    /// (`auth.logOut`), such as the one `/forget` logs in with to check who
    /// asks. A session Telegram already considers dead counts as logged out.
    async fn log_out_login(&self, logged_in: LoggedIn) -> Result<(), TgError>;

    /// Graceful shutdown, once the HTTP server has stopped: close every Live client
    /// and re-seal each Telegram session that changed while it was live. Failures
    /// are logged (Grant ids only), not returned: the process is exiting anyway.
    /// Call it through [`shut_down`], which bounds how long it may take.
    async fn shutdown(&self);
}

/// How long graceful shutdown waits for [`Connector::shutdown`]. Docker gives a
/// container 10 s between SIGTERM and SIGKILL unless told otherwise (Kamal's
/// `stop_timeout`); re-sealing normally takes milliseconds.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Run [`Connector::shutdown`], giving up after `within` so a hung save can't
/// block exit. Returns whether it finished in time.
pub async fn shut_down(connector: &dyn Connector, within: Duration) -> bool {
    let finished = tokio::time::timeout(within, connector.shutdown())
        .await
        .is_ok();
    if !finished {
        tracing::warn!(
            timeout_s = within.as_secs(),
            "flushing the client pool timed out; unsaved telegram session changes are lost"
        );
    }
    finished
}

/// Starts the Telegram part of a Login flow.
#[async_trait::async_trait]
pub trait Login: Send + Sync + 'static {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError>;
}

/// One in-progress Telegram login, driven by the login page polling it.
#[async_trait::async_trait]
pub trait LoginAttempt: Send + 'static {
    /// Advance as far as possible without user input and report where we are.
    async fn poll(&mut self) -> Result<LoginState, TgError>;

    /// Check the Cloud password once [`LoginState::PasswordNeeded`] was reported.
    /// Takes the password so it is wiped as soon as the check is over; it is never
    /// stored or logged. A wrong one is [`TgError::WrongPassword`] and may be retried.
    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError>;
}

pub enum LoginState {
    /// Show this `tg://login?token=…` URL as a QR code and keep polling.
    WaitingForScan { qr_url: String },
    /// Scanned, but the Account has a Cloud password: ask for it (with its hint).
    PasswordNeeded { hint: Option<String> },
    /// Done: the Account and its fresh Telegram session.
    LoggedIn(LoggedIn),
}

/// The result of a finished login. The session bytes are wiped on drop.
pub struct LoggedIn {
    pub account: Account,
    pub telegram_session: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for LoggedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggedIn")
            .field("account_id", &self.account.id)
            .field(
                "telegram_session",
                &format_args!("<{} bytes>", self.telegram_session.len()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector whose shutdown never finishes (e.g. the database hangs).
    struct Hung;

    #[async_trait::async_trait]
    impl Connector for Hung {
        async fn connect(&self, _: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
            unreachable!()
        }
        async fn connect_for_consent(&self, _: &LoggedIn) -> Result<Arc<dyn Telegram>, TgError> {
            unreachable!()
        }
        async fn log_out(&self, _: OpenedGrant) -> Result<(), TgError> {
            unreachable!()
        }
        async fn evict_and_log_out(&self, _: &GrantId) -> Result<bool, TgError> {
            unreachable!()
        }
        async fn log_out_login(&self, _: LoggedIn) -> Result<(), TgError> {
            unreachable!()
        }
        async fn shutdown(&self) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_shutdown_is_cut_off() {
        let started = tokio::time::Instant::now();
        let finished =
            tokio::time::timeout(2 * SHUTDOWN_TIMEOUT, shut_down(&Hung, SHUTDOWN_TIMEOUT))
                .await
                .expect("shut_down gives up on its own");
        assert!(!finished);
        assert_eq!(started.elapsed(), SHUTDOWN_TIMEOUT);
    }

    #[tokio::test]
    async fn a_finished_shutdown_is_reported() {
        let connector = FakeConnector::default();
        assert!(shut_down(&connector, SHUTDOWN_TIMEOUT).await);
        assert_eq!(connector.shutdowns(), 1);
    }
}
