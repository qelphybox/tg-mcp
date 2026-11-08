//! In-memory stand-ins for Telegram, used by tests and by `TG_MCP_FAKE_TELEGRAM=1`.
//!
//! A fake "Telegram session" is a [`FakeData`] as JSON (the Account and its Chats),
//! so a Grant created by a fake login connects back to exactly that data.

use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

use grammers_client::tl;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{
    Account, Connector, LoggedIn, Login, LoginAttempt, LoginState, Telegram, TgError,
    chats::{
        self, ChatId, ChatInfo, ChatKind, ChatPage, ChatRef, ChatSummary, ChatsQuery,
        DialogsAnswer, DialogsRequest, MessagePreview, RawDialog, RawMessage, RawPeer, RawProfile,
        chat_info, fallback_kind, user_info,
    },
    compose,
    download::{self, FileError, FilePart, FileRequest, MediaFile, RawMediaMessage},
    drafts::{self, Drafts, DraftsAnswer, NewDraft, RawDraft, SavedDraft},
    find::{self, FindRequest, FoundAnswer, FoundChats},
    folders::{self, ChatFolder, RawChatFolder},
    markdown::FormattedText,
    media::{FileLocation, StoredFile},
    members::{
        self, ChannelAccess, GroupAnswer, MemberPage, MemberSource, MembersQuery,
        ParticipantsAnswer, ParticipantsFilter, ParticipantsRequest, RawMember, Role,
    },
    messages::{
        HistoryAnswer, HistoryQuery, HistoryRequest, MediaKind, MediaSummary, MessageAnswer,
        MessageAt, MessagePage, MessageRequest, RawHistoryMessage, read_history,
    },
    search::{
        self, ChatType, MediaFilter, QUERY_REQUIRED, RawFoundMessage, RequestScope, SearchAnswer,
        SearchFrom, SearchPage, SearchQuery, SearchRequest, SearchScope,
    },
    topics::{self, RawForums, RawTopic, topic_of},
    transcribe::{
        self, Refusal, TranscribedAudio, Transcription, TranscriptionFeed, TranscriptionUpdate,
        Trial,
    },
    view::{self, Album, Picture, PictureSize, RawViewMessage, SizeForm},
};
use crate::{
    limits::MethodClass,
    sessions::{GrantId, OpenedGrant},
};

/// Everything a fake Account has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeData {
    pub account: Account,
    /// The Account's Chats. Their ids may be written without access hash
    /// (`user:101`); the fake hands them out with [`fake_access_hash`].
    #[serde(default)]
    pub chats: Vec<ChatSummary>,
    /// Each Chat's history, in any order, as Telegram sends it: in Saved
    /// Messages (the Account's own id) every message has the Account as
    /// `from` and no `out`, a forward also its original author (#52).
    #[serde(default)]
    pub messages: std::collections::HashMap<ChatId, Vec<RawHistoryMessage>>,
    /// Descriptions and member counts, for get_chat_info.
    #[serde(default)]
    pub details: std::collections::HashMap<ChatId, FakeDetails>,
    /// Users' profiles and the Chats they share with the Account, for
    /// get_chat_info. A user without one has an empty profile.
    #[serde(default)]
    pub profiles: std::collections::HashMap<ChatId, FakeProfile>,
    /// Members of groups, supergroups and channels, for get_members. A Chat
    /// of the Account's without them has none listed.
    #[serde(default)]
    pub members: std::collections::HashMap<ChatId, FakeMembers>,
    /// Chats in the Archive; the rest are in the main list.
    #[serde(default)]
    pub archived: Vec<ChatId>,
    /// Contacts the Account has no chat with: not in list_chats, but found by
    /// find_chats, with a profile (get_chat_info) and an empty history.
    #[serde(default)]
    pub contacts: Vec<ChatSummary>,
    /// Public Chats and bots the Account isn't in: only the global part of
    /// `contacts.search` knows them.
    #[serde(default)]
    pub public: Vec<ChatSummary>,
    /// Peers `contacts.search` describes as "min": their access hash only
    /// works within that answer.
    #[serde(default)]
    pub min_peers: Vec<ChatId>,
    /// When set, every reading call (method class Default) answers like
    /// Telegram's FLOOD_WAIT_<seconds>.
    #[serde(default)]
    pub flood_wait: Option<u32>,
    /// When set, every search (method class Search) answers like
    /// FLOOD_WAIT_<seconds>, and nothing else does.
    #[serde(default)]
    pub search_flood_wait: Option<u32>,
    /// How getHistory answers a slice that reaches past the newest message.
    #[serde(default)]
    pub past_newest: PastNewest,
    /// When set, such a slice's answer describes its users and channels as
    /// "min" (no hash to hand out), as Telegram did in production (#51).
    #[serde(default)]
    pub min_past_newest: bool,
    /// Each Chat's messages' albums and picture sizes (#34), beyond their
    /// `media`.
    #[serde(default)]
    pub pictures: std::collections::HashMap<ChatId, Vec<FakePicture>>,
    /// What Telegram hears in each Chat's voice messages and video messages
    /// (#33). A voice message or video message without one is one Telegram
    /// fails to transcribe.
    #[serde(default)]
    pub transcriptions: std::collections::HashMap<ChatId, Vec<FakeTranscription>>,
    /// Whether the Account may transcribe: Premium, a weekly trial, or not.
    #[serde(default)]
    pub transcribe: FakeTranscribeAccess,
    /// The forums among the Chats and their Topics, General (id 1)
    /// included, in Telegram's order (`messages.getForumTopics`). A message
    /// of a forum is in the Topic its `topic` names, else in General.
    #[serde(default)]
    pub topics: std::collections::HashMap<ChatId, Vec<FakeTopic>>,
    /// When set, history and search answers describe the Topics of their
    /// messages (`messages.messages.topics`); otherwise titles take a
    /// `messages.getForumTopicsByID`.
    #[serde(default)]
    pub topics_in_answers: bool,
    /// The Account's Chat folders, in the user's order (#29).
    #[serde(default)]
    pub chat_folders: Vec<FakeChatFolder>,
    /// When set, `messages.getDialogFilters` fails, and not with a flood.
    #[serde(default)]
    pub chat_folders_fail: bool,
    /// The Account's Drafts when its first Live client connects (#43), as
    /// if the user typed them in an app. From then on Telegram keeps them:
    /// [`FakeDrafts`].
    #[serde(default)]
    pub drafts: Vec<FakeDraft>,
    /// When set, every write (method class Writes, e.g. `messages.saveDraft`)
    /// answers like FLOOD_WAIT_<seconds>, and nothing else does.
    #[serde(default)]
    pub writes_flood_wait: Option<u32>,
}

/// One Draft of a fake Account: a Chat's input field, or a forum Topic's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeDraft {
    pub chat: ChatId,
    /// The Topic's top message (`top_msg_id`); `None` in General and outside
    /// forums.
    #[serde(default)]
    pub topic: Option<i32>,
    #[serde(default)]
    pub reply_to: Option<i32>,
    /// The text and its entities, as `messages.saveDraft` got them.
    pub text: FormattedText,
    /// Unix seconds.
    pub date: i32,
}

impl FakeDraft {
    pub fn new(chat: ChatId, text: &str, date: i32) -> Self {
        Self {
            chat,
            topic: None,
            reply_to: None,
            text: FormattedText::plain(text),
            date,
        }
    }
}

/// Telegram's Drafts as the fake models them, shared by every Grant like
/// Telegram is: per Account, one Draft per Chat or forum Topic, the newest
/// replacing the one there, synced to every device (every Grant of the
/// Account sees it).
#[derive(Debug, Default)]
pub struct FakeDrafts {
    state: Mutex<DraftsState>,
}

#[derive(Debug, Default)]
struct DraftsState {
    /// Each Account's Drafts, once its first Live client connected.
    drafts: std::collections::HashMap<i64, Vec<FakeDraft>>,
    /// The last date given to a Draft: each save gets a later one, so the
    /// newest is always first even within one second.
    last_date: i32,
}

impl FakeDrafts {
    fn state(&self) -> std::sync::MutexGuard<'_, DraftsState> {
        self.state.lock().unwrap()
    }

    /// An Account's Drafts, the most recently saved first.
    pub fn of(&self, account: i64) -> Vec<FakeDraft> {
        let mut drafts = self
            .state()
            .drafts
            .get(&account)
            .cloned()
            .unwrap_or_default();
        drafts.sort_by_key(|d| std::cmp::Reverse(d.date));
        drafts
    }

    /// The Drafts an Account has when it first connects.
    fn seed(&self, account: i64, drafts: &[FakeDraft]) {
        let mut state = self.state();
        if !state.drafts.contains_key(&account) {
            let latest = drafts.iter().map(|d| d.date).max().unwrap_or_default();
            state.last_date = state.last_date.max(latest);
            state.drafts.insert(account, drafts.to_vec());
        }
    }

    /// `messages.saveDraft`: replace the Draft of `chat` / `topic`, or clear
    /// it with empty text.
    fn save(
        &self,
        account: i64,
        chat: ChatId,
        topic: Option<i32>,
        reply_to: Option<i32>,
        text: FormattedText,
    ) {
        let mut state = self.state();
        let now = i32::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
        let date = now.max(state.last_date + 1);
        let drafts = state.drafts.entry(account).or_default();
        drafts.retain(|d| (d.chat, d.topic) != (chat, topic));
        if !text.text.is_empty() {
            drafts.push(FakeDraft {
                chat,
                topic,
                reply_to,
                text,
                date,
            });
            state.last_date = date;
        }
    }
}

/// One Chat folder of a fake Account. Its Chats are named by id; the fake
/// gives them their [`fake_access_hash`], as Telegram's folders carry one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeChatFolder {
    pub id: i32,
    pub title: String,
    #[serde(default)]
    pub emoji: Option<String>,
    /// Defined by a rule (kinds of chats, leaving out muted, read or
    /// archived ones) besides its lists.
    #[serde(default)]
    pub rules: bool,
    #[serde(default)]
    pub pinned: Vec<ChatId>,
    #[serde(default)]
    pub included: Vec<ChatId>,
    #[serde(default)]
    pub excluded: Vec<ChatId>,
}

impl FakeChatFolder {
    fn raw(&self) -> RawChatFolder {
        let refs = |ids: &[ChatId]| {
            ids.iter()
                .map(|&id| ChatRef::new(id, fake_access_hash(id)))
                .collect()
        };
        RawChatFolder {
            folder: ChatFolder {
                id: self.id,
                title: self.title.clone(),
                emoji: self.emoji.clone(),
                rules: self.rules,
            },
            pinned: refs(&self.pinned),
            included: refs(&self.included),
            excluded: self.excluded.clone(),
        }
    }
}

/// One Topic of a fake forum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeTopic {
    pub id: i32,
    pub title: String,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub pinned: bool,
}

impl FakeTopic {
    pub fn new(id: i32, title: &str) -> Self {
        Self {
            id,
            title: title.into(),
            closed: false,
            pinned: false,
        }
    }

    fn raw(&self, chat: ChatId) -> RawTopic {
        RawTopic {
            chat,
            id: self.id,
            title: self.title.clone(),
            closed: self.closed,
            pinned: self.pinned,
        }
    }
}

/// What Telegram hears in one message, and how long it takes to say.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeTranscription {
    pub message_id: i32,
    pub text: String,
    /// From the first `transcribeAudio` for the message to the final text:
    /// zero answers it at once, `None` never finishes by itself
    /// ([`FakeTranscriber::finish`] does).
    pub takes_ms: Option<u64>,
}

impl FakeTranscription {
    pub fn new(message_id: i32, text: &str, takes_ms: Option<u64>) -> Self {
        Self {
            message_id,
            text: text.into(),
            takes_ms,
        }
    }
}

/// Whether an Account may transcribe (core.telegram.org/api/transcribe).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FakeTranscribeAccess {
    /// Telegram Premium: any number of messages, no trial counter.
    #[default]
    Premium,
    /// Without Premium: `remaining` free transcriptions this week, each of
    /// a message up to [`FAKE_TRIAL_DURATION_MAX`] seconds long.
    Trial { remaining: u32 },
    /// Without Premium and without a trial: `PREMIUM_ACCOUNT_REQUIRED`.
    NoTrial,
}

/// The longest message a trial transcribes (Telegram's
/// `transcribe_audio_trial_duration_max`).
pub const FAKE_TRIAL_DURATION_MAX: u32 = 300;

/// When the fake's weekly trial resets, from the first transcription of an
/// Account.
pub const FAKE_TRIAL_RESETS_IN_S: i64 = 3 * 86_400 + 3_600;

/// What a fake message's media shows (#34), beyond its [`Media`]: the
/// album it is in, and the sizes Telegram keeps of its photo or the
/// thumbnails of its document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakePicture {
    pub message_id: i32,
    #[serde(default)]
    pub grouped_id: Option<i64>,
    /// `None`: a photo has the sizes Telegram makes of a photo its size
    /// ([`fake_photo_sizes`]), anything else none.
    #[serde(default)]
    pub sizes: Option<Vec<FakeSize>>,
}

/// One size or thumbnail, and what its bytes are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeSize {
    pub size: PictureSize,
    pub format: FakeFormat,
}

/// What a fake picture's bytes start like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FakeFormat {
    Jpeg,
    Webp,
    Png,
    /// An animated sticker's Lottie, gzipped: no image.
    Tgs,
}

impl FakeFormat {
    fn magic(self) -> &'static [u8] {
        match self {
            Self::Jpeg => b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00",
            Self::Webp => b"RIFF\x00\x00\x00\x00WEBPVP8 ",
            Self::Png => b"\x89PNG\r\n\x1a\n",
            Self::Tgs => b"\x1F\x8B\x08\x00",
        }
    }
}

impl FakeSize {
    pub fn new(r#type: &str, form: SizeForm, (w, h): (u32, u32), bytes: u64) -> Self {
        Self {
            size: PictureSize {
                r#type: r#type.into(),
                form,
                w,
                h,
                bytes,
            },
            format: FakeFormat::Jpeg,
        }
    }

    pub fn as_format(mut self, format: FakeFormat) -> Self {
        self.format = format;
        self
    }
}

/// The sizes Telegram makes of a `w`×`h` photo of `bytes`: a stripped `i`
/// thumbnail, then `m` (320 px on the longer side), `x` (800), `y` (1280)
/// and `w` (2560) up to the photo's own size, which is the last one at its
/// full dimensions (progressive from `y` on). No `x` or `y` for a photo
/// smaller than their box.
pub fn fake_photo_sizes(w: u32, h: u32, bytes: u64) -> Vec<FakeSize> {
    let mut sizes = vec![FakeSize::new("i", SizeForm::Stripped, (0, 0), 40)];
    let long = w.max(h).max(1);
    let pixels = u64::from(w) * u64::from(h);
    for (r#type, side) in [("m", 320), ("x", 800), ("y", 1280), ("w", 2560)] {
        let form = if side >= 1280 {
            SizeForm::Progressive
        } else {
            SizeForm::File
        };
        if long <= side {
            sizes.push(FakeSize::new(r#type, form, (w, h), bytes));
            break;
        }
        let (sw, sh) = (w * side / long, h * side / long);
        let scaled = bytes * u64::from(sw) * u64::from(sh) / pixels.max(1);
        sizes.push(FakeSize::new(r#type, form, (sw, sh), scaled.max(1_000)));
    }
    sizes
}

/// What Telegram does when `add_offset` puts a getHistory slice's start above
/// the newest message. Its docs don't say, so the server must cope with both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PastNewest {
    /// The slice keeps its older end and starts at the newest message: fewer
    /// than `limit` messages.
    #[default]
    Shorten,
    /// The slice starts at the newest message and still holds `limit`, so it
    /// reaches further down than asked.
    ShiftOlder,
}

/// Telegram's largest getHistory `limit`; larger ones are cut to it.
const HISTORY_MAX_LIMIT: i64 = 100;

/// `messages.getHistory` as Telegram answers it (core.telegram.org/api/offsets),
/// or `messages.search` with the pinned filter (`request.pinned`) on the
/// pinned messages only, or `messages.getReplies` on a Topic (`request.topic`)
/// on that Topic's messages only, on a history given in any order: newest first, slice at
/// `position(offset_id) + add_offset` (`position` counts the messages down to
/// `offset_id` inclusive, whether or not that id exists; `offset_id` wins over
/// `offset_date`), keep `limit`, then drop ids outside `min_id`/`max_id`.
pub fn get_history(
    history: &[RawHistoryMessage],
    request: &HistoryRequest,
    past_newest: PastNewest,
) -> Vec<RawHistoryMessage> {
    let mut list = history.to_vec();
    // `messages.search` with the pinned filter: the same slicing, among the
    // pinned messages only.
    list.retain(|m| m.pinned || !request.pinned);
    list.retain(|m| request.topic == 0 || topic_of(m.topic) == request.topic);
    list.sort_by_key(|m| std::cmp::Reverse(m.id));
    let limit = i64::from(request.limit).clamp(0, HISTORY_MAX_LIMIT);
    let mut start = slice_start(&list, request);
    let mut take = limit;
    if start < 0 {
        if past_newest == PastNewest::Shorten {
            take = (take + start).max(0);
        }
        start = 0;
    }
    list.into_iter()
        .skip(start as usize)
        .take(take as usize)
        .filter(|m| request.max_id == 0 || m.id < request.max_id)
        .filter(|m| request.min_id == 0 || m.id > request.min_id)
        .collect()
}

/// Whether a getHistory slice starts above the newest message of `history`.
fn reaches_past_newest(history: &[RawHistoryMessage], request: &HistoryRequest) -> bool {
    let mut list = history.to_vec();
    list.retain(|m| request.topic == 0 || topic_of(m.topic) == request.topic);
    list.sort_by_key(|m| std::cmp::Reverse(m.id));
    slice_start(&list, request) < 0
}

/// Where a getHistory slice starts in `list` (newest first): below 0 when
/// `add_offset` reaches past the newest message.
fn slice_start(list: &[RawHistoryMessage], request: &HistoryRequest) -> i64 {
    let position = if request.offset_id != 0 {
        list.iter()
            .take_while(|m| m.id >= request.offset_id)
            .count()
    } else if request.offset_date != 0 {
        list.iter()
            .take_while(|m| m.date >= request.offset_date)
            .count()
    } else {
        0
    };
    position as i64 + i64::from(request.add_offset)
}

/// `messages.getMessages` / `channels.getMessages` for one id as Telegram
/// answers it (core.telegram.org/method/messages.getMessages,
/// /method/channels.getMessages, /constructor/messageEmpty), on the
/// Account's histories:
///
/// - in a channel or supergroup (`channels.getMessages`), the id is looked up
///   in that Chat only;
/// - anywhere else (`messages.getMessages`, which takes no peer), ids count
///   across all of the Account's private chats and basic groups, so the
///   message found may be another Chat's: the caller has to check its Chat;
/// - a deleted or unknown id is a `messageEmpty`, i.e. no message.
///
/// Real ids outside channels are unique across those Chats; should a fixture
/// repeat one, it is always the same Chat's (by [`peer_key`]).
pub fn get_message(
    histories: &std::collections::HashMap<ChatId, Vec<RawHistoryMessage>>,
    chat: ChatId,
    request: &MessageRequest,
) -> Vec<RawFoundMessage> {
    let mut chats: Vec<ChatId> = match chat {
        ChatId::Channel(_) => vec![chat],
        _ => histories
            .keys()
            .copied()
            .filter(|c| !matches!(c, ChatId::Channel(_)))
            .collect(),
    };
    chats.sort_by_key(|&c| peer_key(c));
    chats
        .into_iter()
        .find_map(|c| {
            let message = histories.get(&c)?.iter().find(|m| m.id == request.id)?;
            Some(RawFoundMessage {
                chat: c,
                message: message.clone(),
            })
        })
        .into_iter()
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeDetails {
    pub description: Option<String>,
    pub member_count: Option<i32>,
}

/// A user as `users.getFullUser` describes them to the Account, and the Chats
/// `messages.getCommonChats` lists for them (Chats of the Account's list; the
/// profile's count is Telegram's, so it is set separately).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeProfile {
    #[serde(default)]
    pub profile: RawProfile,
    #[serde(default)]
    pub common_chats: Vec<ChatId>,
}

/// A group's members as Telegram shows them to the Account
/// (`channels.getParticipants`, `messages.getFullChat`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeMembers {
    /// In Telegram's order: most recent first.
    #[serde(default)]
    pub members: Vec<FakeMember>,
    /// A broadcast channel the Account doesn't administer: Telegram shows its
    /// members (admins too) only to admins, `CHAT_ADMIN_REQUIRED`.
    #[serde(default)]
    pub admins_required: bool,
    /// The group hides its members (`participants_hidden`) and the Account is
    /// no admin: Telegram lists only the admins and bots.
    #[serde(default)]
    pub hidden: bool,
    /// Telegram lists no more than this many of a large group's members,
    /// whatever its count says.
    #[serde(default)]
    pub cap: Option<usize>,
    /// A basic group the Account left: `chatParticipantsForbidden`.
    #[serde(default)]
    pub forbidden: bool,
}

/// One member: a user, and what they are in the group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeMember {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bot: bool,
    pub role: Role,
}

impl FakeMember {
    /// Telegram's member search: the beginning of a word of the name, or of
    /// the @username, ignoring case.
    fn found_by(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.name
            .split_whitespace()
            .any(|word| word.to_lowercase().starts_with(&q))
            || self
                .username
                .as_deref()
                .is_some_and(|u| u.to_lowercase().starts_with(&q))
    }
}

impl From<Account> for FakeData {
    fn from(account: Account) -> Self {
        Self::new(account)
    }
}

impl FakeData {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            chats: Vec::new(),
            messages: Default::default(),
            details: Default::default(),
            profiles: Default::default(),
            members: Default::default(),
            archived: Vec::new(),
            contacts: Vec::new(),
            public: Vec::new(),
            min_peers: Vec::new(),
            flood_wait: None,
            search_flood_wait: None,
            past_newest: PastNewest::default(),
            min_past_newest: false,
            pictures: Default::default(),
            topics: Default::default(),
            topics_in_answers: false,
            transcriptions: Default::default(),
            transcribe: FakeTranscribeAccess::default(),
            chat_folders: Vec::new(),
            chat_folders_fail: false,
            drafts: Vec::new(),
            writes_flood_wait: None,
        }
    }

    pub fn with_drafts(mut self, drafts: Vec<FakeDraft>) -> Self {
        self.drafts = drafts;
        self
    }

    pub fn with_chat_folders(mut self, folders: Vec<FakeChatFolder>) -> Self {
        self.chat_folders = folders;
        self
    }

    pub fn with_topics(mut self, forum: ChatId, topics: Vec<FakeTopic>) -> Self {
        self.topics.insert(forum, topics);
        self
    }

    pub fn with_topics_in_answers(mut self) -> Self {
        self.topics_in_answers = true;
        self
    }

    pub fn with_transcriptions(mut self, chat: ChatId, heard: Vec<FakeTranscription>) -> Self {
        self.transcriptions.insert(chat, heard);
        self
    }

    pub fn with_transcribe_access(mut self, access: FakeTranscribeAccess) -> Self {
        self.transcribe = access;
        self
    }

    pub fn with_pictures(mut self, chat: ChatId, pictures: Vec<FakePicture>) -> Self {
        self.pictures.insert(chat, pictures);
        self
    }

    pub fn with_past_newest(mut self, past_newest: PastNewest) -> Self {
        self.past_newest = past_newest;
        self
    }

    pub fn with_min_past_newest(mut self) -> Self {
        self.min_past_newest = true;
        self
    }

    pub fn with_details(mut self, chat: ChatId, details: FakeDetails) -> Self {
        self.details.insert(chat, details);
        self
    }

    pub fn with_profile(mut self, user: ChatId, profile: FakeProfile) -> Self {
        self.profiles.insert(user, profile);
        self
    }

    pub fn with_members(mut self, chat: ChatId, members: FakeMembers) -> Self {
        self.members.insert(chat, members);
        self
    }

    pub fn with_messages(mut self, chat: ChatId, messages: Vec<RawHistoryMessage>) -> Self {
        self.messages.insert(chat, messages);
        self
    }

    pub fn with_chats(mut self, chats: Vec<ChatSummary>) -> Self {
        self.chats = chats;
        self
    }

    pub fn with_archived(mut self, chats: Vec<ChatId>) -> Self {
        self.archived = chats;
        self
    }

    pub fn with_contacts(mut self, contacts: Vec<ChatSummary>) -> Self {
        self.contacts = contacts;
        self
    }

    pub fn with_public(mut self, chats: Vec<ChatSummary>) -> Self {
        self.public = chats;
        self
    }

    pub fn with_min_peers(mut self, peers: Vec<ChatId>) -> Self {
        self.min_peers = peers;
        self
    }
}

/// One Chat as the fake's search sees it.
#[derive(Debug, Clone, Copy)]
pub struct FakeSearchChat<'a> {
    pub id: ChatId,
    pub kind: ChatKind,
    pub archived: bool,
    pub messages: &'a [RawHistoryMessage],
}

/// The fake's "rate" of a message: what orders global results and what
/// `next_rate` hands out. Monotonic in the date but not the date, so a server
/// that passes a date where Telegram's rate belongs pages wrong.
pub fn fake_rate(date: i32) -> i32 {
    date - 1_000_000_000
}

/// Telegram's order across Chats for equal rates: by peer, as the Bot API
/// numbers them, then by message id.
fn peer_key(chat: ChatId) -> i64 {
    match chat {
        ChatId::User(id) => id,
        ChatId::Chat(id) => -id,
        ChatId::Channel(id) => -1_000_000_000_000 - id,
    }
}

/// Telegram's largest search `limit`, as for getHistory.
const SEARCH_MAX_LIMIT: i32 = 100;

/// `messages.searchGlobal` / `messages.search` as Telegram answers them, on
/// the account `me`'s Chats (core.telegram.org/method/messages.searchGlobal,
/// /method/messages.search, /constructor/messages.messagesSlice):
///
/// - matching: every query word begins a word of the text, case-insensitive
///   (Telegram also matches word forms; the fake doesn't); an empty query
///   matches everything, then `filter`, `min_date`/`max_date` (exclusive) and
///   the sender apply;
/// - global: results of the chosen Chats ordered by (rate, peer, id), newest
///   first; a page holds the `limit` results strictly after (`offset_rate`,
///   `offset_peer`, `offset_id`), and a slice's `next_rate` is the rate of its
///   last message;
/// - one Chat: newest first, ids strictly below `offset_id`;
/// - the answer is `messages.messages` when a first page holds every result,
///   else `messages.messagesSlice` (`messages.channelMessages` in a channel,
///   which has no `next_rate`). Never more than `limit` messages, and pages
///   never overlap.
pub fn search_answer(chats: &[FakeSearchChat], me: i64, request: &SearchRequest) -> SearchAnswer {
    let limit = request.limit.clamp(0, SEARCH_MAX_LIMIT) as usize;
    let matches = |chat: ChatId, m: &RawHistoryMessage, from: Option<SearchFrom>| {
        let sender = if m.out {
            ChatId::User(me)
        } else {
            m.from.unwrap_or(chat)
        };
        search::text_matches(&m.text, &request.q)
            && request.filter.is_none_or(|f| media_matches(f, m))
            && (request.min_date == 0 || m.date > request.min_date)
            && (request.max_date == 0 || m.date < request.max_date)
            && from.is_none_or(|from| match from {
                SearchFrom::Me => sender == ChatId::User(me),
                SearchFrom::Peer(peer) => sender == peer.chat(),
            })
    };
    let found = |chat: ChatId, m: &RawHistoryMessage| RawFoundMessage {
        chat,
        message: m.clone(),
    };
    let (page, first_page, rest, channel) = match request.scope {
        RequestScope::Global {
            chat_type,
            folder_id,
            offset_rate,
            offset_peer,
            offset_id,
        } => {
            let mut results: Vec<RawFoundMessage> = chats
                .iter()
                .filter(|c| match chat_type {
                    None => true,
                    Some(ChatType::Private) => matches!(c.id, ChatId::User(_)),
                    Some(ChatType::Groups) => {
                        matches!(c.kind, ChatKind::Group | ChatKind::Supergroup)
                    }
                    Some(ChatType::Channels) => c.kind == ChatKind::Channel,
                })
                .filter(|c| match folder_id {
                    Some(1) => c.archived,
                    Some(0) => !c.archived,
                    _ => true,
                })
                .flat_map(|c| {
                    c.messages
                        .iter()
                        .filter(|m| matches(c.id, m, None))
                        .map(|m| found(c.id, m))
                })
                .collect();
            let order =
                |f: &RawFoundMessage| (fake_rate(f.message.date), peer_key(f.chat), f.message.id);
            results.sort_by_key(|f| std::cmp::Reverse(order(f)));
            let first_page = offset_rate == 0 && offset_peer.is_none() && offset_id == 0;
            if !first_page {
                let offset = (
                    offset_rate,
                    offset_peer.map_or(i64::MAX, |p| peer_key(p.chat())),
                    offset_id,
                );
                results.retain(|f| order(f) < offset);
            }
            let rest = results.split_off(limit.min(results.len()));
            (results, first_page, rest.len(), false)
        }
        RequestScope::Chat {
            chat,
            from,
            topic,
            offset_id,
        } => {
            let mut results: Vec<RawFoundMessage> = chats
                .iter()
                .filter(|c| c.id == chat.chat())
                .flat_map(|c| {
                    c.messages
                        .iter()
                        .filter(|m| offset_id == 0 || m.id < offset_id)
                        .filter(|m| topic.is_none_or(|t| topic_of(m.topic) == t))
                        .filter(|m| matches(c.id, m, from))
                        .map(|m| found(c.id, m))
                })
                .collect();
            results.sort_by_key(|f| std::cmp::Reverse(f.message.id));
            let rest = results.split_off(limit.min(results.len()));
            let channel = matches!(chat.chat(), ChatId::Channel(_));
            (results, offset_id == 0, rest.len(), channel)
        }
    };
    let complete = first_page && rest == 0 && !channel;
    let global = matches!(request.scope, RequestScope::Global { .. });
    SearchAnswer {
        next_rate: (global && !complete)
            .then(|| page.last().map(|f| fake_rate(f.message.date)))
            .flatten(),
        returned: page.len(),
        complete,
        messages: page,
        peers: Vec::new(),
        forums: RawForums::default(),
    }
}

/// Whether `contacts.search` finds `peer` for `q`: every word of `q` begins
/// a word of the name or title, or `q` begins the @username, ignoring case.
/// Taken literally: `@alice` begins no username (the tool strips the `@`).
fn peer_matches(peer: &RawPeer, q: &str) -> bool {
    let q = q.to_lowercase();
    search::text_matches(&peer.title, &q)
        || peer
            .username
            .as_deref()
            .is_some_and(|u| u.to_lowercase().starts_with(&q))
}

/// `contacts.search` as Telegram answers it (core.telegram.org/method/contacts.search,
/// /constructor/contacts.found; how the lists are filled is not documented,
/// so this is the least polite reading):
///
/// - `my_results`: the Account's own Chats (main list and Archive) and
///   contacts that match, in the given order;
/// - `results`: public peers that match, the Account's own public Chats among
///   them (so a peer can be in both lists), and public ones it isn't in;
/// - each list holds at most `limit` peers; `users`/`chats` describe them all,
///   `min` ones without a hash.
pub fn contacts_search(
    own: &[RawPeer],
    public: &[RawPeer],
    min: &[ChatId],
    request: &FindRequest,
) -> FoundAnswer {
    let limit = request.limit.max(0) as usize;
    let found = |peers: &mut dyn Iterator<Item = &RawPeer>| -> Vec<RawPeer> {
        peers
            .filter(|p| peer_matches(p, &request.q))
            .take(limit)
            .cloned()
            .collect()
    };
    let mine = found(&mut own.iter());
    let global = found(
        &mut own
            .iter()
            .filter(|p| p.username.is_some())
            .chain(public.iter()),
    );
    let peers = mine
        .iter()
        .chain(&global)
        .map(|p| RawPeer {
            access_hash: p.access_hash.filter(|_| !min.contains(&p.id)),
            ..p.clone()
        })
        .collect();
    FoundAnswer {
        my_results: mine.iter().map(|p| p.id).collect(),
        results: global.iter().map(|p| p.id).collect(),
        peers,
    }
}

/// Telegram's `inputMessagesFilter*` on one message. Links: the fake counts
/// a `://` in the text (Telegram goes by its url entities).
fn media_matches(filter: MediaFilter, m: &RawHistoryMessage) -> bool {
    let kind = match filter {
        MediaFilter::Link => return m.text.contains("://"),
        MediaFilter::Photo => MediaKind::Photo,
        MediaFilter::Video => MediaKind::Video,
        MediaFilter::Document => MediaKind::Document,
        MediaFilter::Voice => MediaKind::Voice,
        MediaFilter::VideoNote => MediaKind::VideoNote,
        MediaFilter::Gif => MediaKind::Gif,
    };
    m.media.as_ref().is_some_and(|media| media.kind == kind)
}

#[derive(Debug, Clone)]
pub struct FakeTelegram {
    data: FakeData,
    known: KnownPeers,
    files: Arc<FakeFiles>,
    transcriber: Arc<FakeTranscriber>,
    drafts: Arc<FakeDrafts>,
}

/// Telegram's transcription service as the fake models it
/// (`messages.transcribeAudio` and `updateTranscribedAudio`), shared by every
/// Grant like Telegram is. Like Telegram it transcribes a message once and
/// keeps the text: asking again for the same message answers from that
/// (pending, or the final text) and spends no trial, while each new message
/// spends one; a used-up trial is `FLOOD_WAIT` until it resets (as TDLib
/// reads it); the text comes in pieces (a partial answer, a pending update,
/// then the final update) and other transcriptions' updates go to the same
/// session.
#[derive(Debug, Default)]
pub struct FakeTranscriber {
    state: Mutex<TranscriberState>,
}

#[derive(Debug, Default)]
struct TranscriberState {
    last_id: i64,
    /// Each message's transcription, once asked for.
    jobs: std::collections::HashMap<(i64, ChatId, i32), Job>,
    /// Each trial Account's `(remaining, resets_at)`, once it transcribed.
    trials: std::collections::HashMap<i64, (u32, i32)>,
    /// Each Account's session updates.
    feeds: std::collections::HashMap<i64, TranscriptionFeed>,
    calls: Vec<TranscribeSeen>,
    /// Every call answers `FLOOD_WAIT_<seconds>`.
    flood: Option<u32>,
}

#[derive(Debug)]
struct Job {
    transcription_id: i64,
    text: String,
    done: bool,
}

/// One `messages.transcribeAudio` the fake received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscribeSeen {
    pub account: i64,
    pub chat: ChatId,
    pub message_id: i32,
}

impl FakeTranscriber {
    fn state(&self) -> std::sync::MutexGuard<'_, TranscriberState> {
        self.state.lock().unwrap()
    }

    /// Every `transcribeAudio` so far, in order.
    pub fn calls(&self) -> Vec<TranscribeSeen> {
        self.state().calls.clone()
    }

    /// Every `transcribeAudio` from now on is answered `FLOOD_WAIT_<seconds>`.
    pub fn flood(&self, seconds: u32) {
        self.state().flood = Some(seconds);
    }

    /// The free transcriptions a trial Account has left, once it used one.
    pub fn trial_remaining(&self, account: i64) -> Option<u32> {
        self.state().trials.get(&account).map(|(left, _)| *left)
    }

    /// Telegram finishes a transcription that was still pending, and pushes
    /// its final text to the Account's session.
    pub fn finish(&self, account: i64, chat: ChatId, message_id: i32) {
        let mut state = self.state();
        let job = state
            .jobs
            .get_mut(&(account, chat, message_id))
            .expect("a transcription was asked for");
        job.done = true;
        let update = TranscriptionUpdate {
            transcription_id: job.transcription_id,
            pending: false,
            text: job.text.clone(),
        };
        let _ = state.feed(account).send(update);
    }

    /// The Account's session updates.
    pub fn feed(&self, account: i64) -> TranscriptionFeed {
        self.state().feed(account)
    }

    /// One `messages.transcribeAudio` for message `message_id` of `chat`.
    fn transcribe_audio(
        self: &Arc<Self>,
        data: &FakeData,
        chat: ChatId,
        message_id: i32,
    ) -> Result<TranscribedAudio, TgError> {
        let account = data.account.id;
        let mut state = self.state();
        state.calls.push(TranscribeSeen {
            account,
            chat,
            message_id,
        });
        if let Some(seconds) = state.flood {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Transcribe,
            });
        }
        let media = data
            .messages
            .get(&chat)
            .and_then(|history| history.iter().find(|m| m.id == message_id))
            .and_then(|m| m.media.clone());
        let Some(media) =
            media.filter(|m| matches!(m.kind, MediaKind::Voice | MediaKind::VideoNote))
        else {
            return Err(Refusal::VoiceMissing.error(message_id));
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let trial = |state: &TranscriberState| match data.transcribe {
            FakeTranscribeAccess::Trial { remaining } => {
                let (left, resets_at) = state
                    .trials
                    .get(&account)
                    .copied()
                    .unwrap_or((remaining, (now + FAKE_TRIAL_RESETS_IN_S) as i32));
                Some(Trial {
                    remaining: left,
                    resets_at,
                })
            }
            _ => None,
        };
        // Asked before: Telegram's own copy, pending or final; no trial spent.
        if let Some(job) = state.jobs.get(&(account, chat, message_id)) {
            return Ok(TranscribedAudio {
                pending: !job.done,
                transcription_id: job.transcription_id,
                text: if job.done {
                    job.text.clone()
                } else {
                    partial(&job.text)
                },
                trial: trial(&state),
            });
        }
        let heard = data
            .transcriptions
            .get(&chat)
            .and_then(|all| all.iter().find(|t| t.message_id == message_id))
            .cloned();
        match data.transcribe {
            FakeTranscribeAccess::Premium => {}
            FakeTranscribeAccess::NoTrial => return Err(Refusal::PremiumRequired.error(message_id)),
            FakeTranscribeAccess::Trial { .. } => {
                let Trial {
                    remaining,
                    resets_at,
                } = trial(&state).expect("a trial account");
                if media.duration_s.unwrap_or(0) > FAKE_TRIAL_DURATION_MAX {
                    return Err(Refusal::TooLong.error(message_id));
                }
                if remaining == 0 {
                    return Err(TgError::FloodWait {
                        seconds: u32::try_from((i64::from(resets_at) - now).max(1))
                            .unwrap_or(u32::MAX),
                        class: MethodClass::Transcribe,
                    });
                }
                if heard.is_some() {
                    state.trials.insert(account, (remaining - 1, resets_at));
                }
            }
        }
        // Nothing Telegram could make out.
        let Some(heard) = heard else {
            return Err(Refusal::Failed.error(message_id));
        };
        state.last_id += 1;
        let transcription_id = 9_000 + state.last_id;
        let done = heard.takes_ms == Some(0);
        state.jobs.insert(
            (account, chat, message_id),
            Job {
                transcription_id,
                text: heard.text.clone(),
                done,
            },
        );
        if let Some(ms) = heard.takes_ms.filter(|ms| *ms > 0) {
            let telegram = self.clone();
            let feed = state.feed(account);
            let text = heard.text.clone();
            tokio::spawn(async move {
                let half = std::time::Duration::from_millis(ms / 2);
                tokio::time::sleep(half).await;
                let _ = feed.send(TranscriptionUpdate {
                    transcription_id,
                    pending: true,
                    text: partial(&text),
                });
                tokio::time::sleep(std::time::Duration::from_millis(ms) - half).await;
                let mut state = telegram.state();
                if let Some(job) = state.jobs.get_mut(&(account, chat, message_id)) {
                    job.done = true;
                }
                let _ = feed.send(TranscriptionUpdate {
                    transcription_id,
                    pending: false,
                    text,
                });
            });
        }
        Ok(TranscribedAudio {
            pending: !done,
            transcription_id,
            text: if done {
                heard.text
            } else {
                partial(&heard.text)
            },
            trial: trial(&state),
        })
    }
}

impl TranscriberState {
    fn feed(&mut self, account: i64) -> TranscriptionFeed {
        self.feeds
            .entry(account)
            .or_insert_with(transcribe::feed)
            .clone()
    }
}

/// What Telegram has heard of `text` while it is still transcribing: the
/// first word.
fn partial(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The DC every fake message says its file is in.
pub const FAKE_FILE_DC: i32 = 2;

/// Telegram's file servers as the fake models them (`upload.getFile`), shared
/// by every Account like Telegram is: what a file reference is worth, which
/// DC the files are really in, and every request they got.
#[derive(Debug, Default)]
pub struct FakeFiles {
    state: Mutex<FilesState>,
}

#[derive(Debug, Default)]
struct FilesState {
    /// File references handed out before the last expiry are refused.
    generation: u64,
    /// Every file reference is refused, however fresh.
    references_never_last: bool,
    /// The DC the files are in, when not [`FAKE_FILE_DC`] as messages say.
    dc_id: Option<i32>,
    /// `(parts, seconds)`: once this many parts were served, every getFile
    /// answers `FLOOD_WAIT_<seconds>`.
    flood_after: Option<(usize, u32)>,
    served: usize,
    /// Files end after this many bytes, whatever size their message gives.
    cut_at: Option<u64>,
    requests: Vec<FileRequestSeen>,
    lookups: usize,
    /// The ids of each `getMessages` an album lookup made.
    album_fetches: Vec<Vec<i32>>,
}

/// One `upload.getFile` the fake received: where it was sent and which part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRequestSeen {
    pub dc_id: i32,
    pub offset: i64,
    pub limit: i32,
}

impl FakeFiles {
    fn state(&self) -> std::sync::MutexGuard<'_, FilesState> {
        self.state.lock().unwrap()
    }

    /// Every file reference handed out so far expires (Telegram does this
    /// after some hours); fetching the message again gives a fresh one.
    pub fn expire_references(&self) {
        self.state().generation += 1;
    }

    /// Refuse every file reference, as if each one expired at once.
    pub fn references_never_last(&self) {
        self.state().references_never_last = true;
    }

    /// The files are really in DC `dc_id`: requests to any other DC are
    /// answered `FILE_MIGRATE_<dc_id>`.
    pub fn move_to_dc(&self, dc_id: i32) {
        self.state().dc_id = Some(dc_id);
    }

    /// Once `parts` parts were served, every getFile is answered
    /// `FLOOD_WAIT_<seconds>` (method class Files).
    pub fn flood_after(&self, parts: usize, seconds: u32) {
        self.state().flood_after = Some((parts, seconds));
    }

    /// Every file ends after `bytes`, shorter than its message says.
    pub fn cut_files_at(&self, bytes: u64) {
        self.state().cut_at = Some(bytes);
    }

    /// Every getFile so far, in order.
    pub fn requests(&self) -> Vec<FileRequestSeen> {
        self.state().requests.clone()
    }

    /// How many times a message was fetched for its file.
    pub fn lookups(&self) -> usize {
        self.state().lookups
    }

    /// The ids each album lookup asked for, in order.
    pub fn album_fetches(&self) -> Vec<Vec<i32>> {
        self.state().album_fetches.clone()
    }
}

impl FilesState {
    /// The file reference Telegram hands out (and accepts) now.
    fn reference(&self) -> Vec<u8> {
        format!("fake-file-reference-{}", self.generation).into_bytes()
    }
}

/// The id of the fake file in message `id` of `chat`.
pub fn fake_file_id(chat: ChatId, id: i32) -> i64 {
    peer_key(chat)
        .wrapping_mul(1_000_003)
        .wrapping_add(i64::from(id))
}

/// The bytes of fake file `file_id`, `size` long: each 4-byte word holds its
/// own index (mixed with the file id), so a part served twice, out of order
/// or from another file shows.
pub fn fake_file_bytes(file_id: i64, size: u64) -> Vec<u8> {
    fake_file_range(file_id, 0..size)
}

/// Bytes `range` of fake file `file_id`.
fn fake_file_range(file_id: i64, range: std::ops::Range<u64>) -> Vec<u8> {
    let seed = (file_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as u32;
    range
        .map(|i| (((i / 4) as u32) ^ seed).to_le_bytes()[(i % 4) as usize])
        .collect()
}

/// The bytes of size `type` of fake picture `file_id`, `size` long: the
/// first bytes of a `format` file, then the same pattern as a file's.
pub fn fake_picture_bytes(file_id: i64, r#type: &str, size: u64, format: FakeFormat) -> Vec<u8> {
    let seed = r#type.bytes().fold(file_id, |seed, b| {
        seed.wrapping_mul(131).wrapping_add(i64::from(b))
    });
    let magic = format.magic();
    let mut bytes = magic.to_vec();
    bytes.extend(fake_file_range(
        seed,
        magic.len() as u64..size.max(magic.len() as u64),
    ));
    bytes.truncate(size as usize);
    bytes
}

/// The fake message's picture entry, if the fixture has one.
fn fake_picture_entry(data: &FakeData, chat: ChatId, id: i32) -> Option<&FakePicture> {
    data.pictures
        .get(&chat)?
        .iter()
        .find(|p| p.message_id == id)
}

/// The sizes of a fake message's picture, with what their bytes are: the
/// fixture's, or a photo's by its size.
fn fake_sizes(data: &FakeData, chat: ChatId, message: &RawHistoryMessage) -> Vec<FakeSize> {
    let Some(media) = &message.media else {
        return Vec::new();
    };
    if let Some(sizes) = fake_picture_entry(data, chat, message.id).and_then(|p| p.sizes.clone()) {
        return sizes;
    }
    match (media.kind, media.width, media.height, media.size_bytes) {
        (MediaKind::Photo, Some(w), Some(h), Some(bytes)) => fake_photo_sizes(w, h, bytes),
        _ => Vec::new(),
    }
}

/// What a view can show of a fake message's media, like
/// [`super::media::picture`]: its photo's sizes or its document's
/// thumbnails, at the message's file.
fn fake_picture(
    data: &FakeData,
    file_reference: &[u8],
    chat: ChatId,
    message: &RawHistoryMessage,
) -> Option<Picture> {
    let file = match message.media.as_ref()?.kind {
        MediaKind::Photo => StoredFile::Photo {
            thumb_size: String::new(),
        },
        MediaKind::Location | MediaKind::Contact | MediaKind::Poll | MediaKind::Other => {
            return None;
        }
        _ => StoredFile::DocumentThumb {
            thumb_size: String::new(),
        },
    };
    let sizes: Vec<PictureSize> = fake_sizes(data, chat, message)
        .into_iter()
        .map(|s| s.size)
        .collect();
    if sizes.is_empty() {
        return None;
    }
    let id = fake_file_id(chat, message.id);
    Some(Picture {
        location: FileLocation {
            file,
            id,
            access_hash: id.wrapping_mul(31),
            file_reference: file_reference.to_vec(),
            dc_id: FAKE_FILE_DC,
        },
        sizes,
    })
}

/// What a fake file's bytes are.
#[derive(Debug, Clone, Copy)]
enum FakeContent<'a> {
    /// The message's file: [`fake_file_bytes`].
    File,
    /// A size of its picture: [`fake_picture_bytes`].
    Picture(&'a str, FakeFormat),
}

/// The file of a fake message's media: kinds with a file whose size the
/// fixture gives. Without a size it is gone, as an expired self-destructing
/// photo is.
fn fake_file(
    file_reference: &[u8],
    chat: ChatId,
    message: &RawHistoryMessage,
) -> Option<(FileLocation, Option<u64>)> {
    let media = message.media.as_ref()?;
    let file = match media.kind {
        MediaKind::Photo => StoredFile::Photo {
            thumb_size: "y".into(),
        },
        MediaKind::Video
        | MediaKind::VideoNote
        | MediaKind::Voice
        | MediaKind::Audio
        | MediaKind::Document
        | MediaKind::Sticker
        | MediaKind::Gif => StoredFile::Document,
        MediaKind::Location | MediaKind::Contact | MediaKind::Poll | MediaKind::Other => {
            return None;
        }
    };
    let size = media.size_bytes?;
    let id = fake_file_id(chat, message.id);
    Some((
        FileLocation {
            file,
            id,
            access_hash: id.wrapping_mul(31),
            file_reference: file_reference.to_vec(),
            dc_id: FAKE_FILE_DC,
        },
        Some(size),
    ))
}

/// Access hashes of the users and channels a Grant's answers described, like
/// a Live client's peer cache.
type KnownPeers = Arc<Mutex<std::collections::HashMap<ChatId, i64>>>;

impl FakeTelegram {
    pub fn new(data: FakeData) -> Self {
        let drafts = Arc::new(FakeDrafts::default());
        drafts.seed(data.account.id, &data.drafts);
        Self {
            data,
            known: Arc::default(),
            files: Arc::default(),
            transcriber: Arc::default(),
            drafts,
        }
    }

    /// Where this Account's Drafts are kept.
    pub fn drafts(&self) -> &Arc<FakeDrafts> {
        &self.drafts
    }

    /// The transcription service this Account's transcriptions go to.
    pub fn transcriber(&self) -> &Arc<FakeTranscriber> {
        &self.transcriber
    }

    /// The file servers this Account's downloads go to.
    pub fn files(&self) -> &Arc<FakeFiles> {
        &self.files
    }
}

/// The access hash the fake Telegram gives a user or channel: fixed per id (and
/// often negative, like real ones), so tests can predict the ids it issues.
/// Basic groups have none.
pub fn fake_access_hash(chat: ChatId) -> Option<i64> {
    match chat {
        ChatId::User(id) | ChatId::Channel(id) => {
            Some((id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64)
        }
        ChatId::Chat(_) => None,
    }
}

/// The id the fake hands out for `chat`, access hash included.
pub fn fake_issued_id(chat: ChatId) -> String {
    ChatRef::new(chat, fake_access_hash(chat)).to_string()
}

#[async_trait::async_trait]
impl Telegram for FakeTelegram {
    async fn whoami(&self) -> Result<Account, TgError> {
        self.flood()?;
        Ok(self.data.account.clone())
    }

    async fn chat_folders(&self) -> Result<Vec<ChatFolder>, TgError> {
        Ok(self
            .dialog_filters()?
            .into_iter()
            .map(|f| f.folder)
            .collect())
    }

    /// The same `messages.getDialogs` calls as the Live client, answered by
    /// [`Self::get_dialogs`].
    async fn list_chats(&self, query: ChatsQuery) -> Result<ChatPage, TgError> {
        if let Some(folder) = query.chat_folder {
            return folders::list_folder_chats(
                &query,
                folder,
                || std::future::ready(self.dialog_filters()),
                |peers| std::future::ready(self.peer_dialogs(&peers)),
            )
            .await;
        }
        chats::list_chats(&query, |request| {
            std::future::ready(self.get_dialogs(&request))
        })
        .await
    }

    /// The same getHistory calls as the Live client, answered by [`get_history`].
    async fn get_messages(&self, id: ChatRef, query: HistoryQuery) -> Result<MessagePage, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        if let Some(topic) = query.topic {
            self.check_topic(chat, topic)?;
        }
        let history = self.data.messages.get(&chat).map_or(&[][..], Vec::as_slice);
        let peers = self.peers();
        let mut page = read_history(
            id,
            &query,
            &|chat| self.known(chat),
            |request| {
                // The pinned messages are a search (method class Search).
                if let Some(seconds) = self.data.search_flood_wait.filter(|_| request.pinned) {
                    return std::future::ready(Err(TgError::FloodWait {
                        seconds,
                        class: MethodClass::Search,
                    }));
                }
                let messages = get_history(history, &request, self.data.past_newest);
                // The Chat, the senders of these messages and the original
                // authors of forwarded ones, as Telegram describes them in a
                // getHistory answer.
                let min = self.data.min_past_newest && reaches_past_newest(history, &request);
                let named = |id| {
                    messages
                        .iter()
                        .any(|m| m.from == Some(id) || m.forwarded_from == Some(id))
                };
                let described: Vec<RawPeer> = peers
                    .iter()
                    .filter(|p| p.id == chat || named(p.id))
                    .map(|p| RawPeer {
                        access_hash: p.access_hash.filter(|_| !min),
                        ..p.clone()
                    })
                    .collect();
                self.remember(&described);
                let forums = self.forums_described(&[chat], messages.iter().map(|m| (chat, m)));
                std::future::ready(Ok(HistoryAnswer {
                    returned: messages.len(),
                    messages,
                    peers: described,
                    forums,
                }))
            },
            |request| {
                self.remember(&peers);
                let messages = get_message(&self.data.messages, chat, &request);
                // getMessages answers describe no Topics.
                let forums = self.forums_described(&[chat], std::iter::empty());
                std::future::ready(Ok(MessageAnswer {
                    messages,
                    peers: peers.clone(),
                    forums,
                }))
            },
        )
        .await?;
        topics::complete(&mut page, |ids| {
            std::future::ready(self.topics_by_id(chat, &ids))
        })
        .await?;
        Ok(page)
    }

    async fn get_chat_info(&self, id: ChatRef) -> Result<ChatInfo, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        let peer = self
            .peers()
            .into_iter()
            .find(|p| p.id == chat)
            .ok_or_else(|| {
                TgError::InvalidInput(format!(
                    "{chat} is not one of your chats; chat ids come from list_chats"
                ))
            })?;
        self.remember(std::slice::from_ref(&peer));
        if let ChatId::User(_) = chat {
            let profile = self.data.profiles.get(&chat).cloned().unwrap_or_default();
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            return Ok(user_info(&peer, profile.profile, now));
        }
        let details = self.data.details.get(&chat).cloned().unwrap_or_default();
        let mut info = chat_info(&peer, details.description, details.member_count);
        // A forum's Topics: one `messages.getForumTopics` of 50.
        if let Some(forum) = self.data.topics.get(&chat) {
            let listed = forum
                .iter()
                .take(topics::MAX_LISTED_TOPICS)
                .map(|t| t.raw(chat))
                .collect();
            let (listed, truncated) = topics::listed(listed, forum.len());
            (info.topics, info.topics_truncated) = (Some(listed), truncated);
        }
        Ok(info)
    }

    /// Like Telegram: the user's common Chats as `messages.getCommonChats`
    /// describes them, cut to `limit`.
    async fn common_chats(&self, user: ChatRef, limit: usize) -> Result<FoundChats, TgError> {
        self.flood()?;
        self.check_hash(user)?;
        let chat = user.chat();
        if !matches!(chat, ChatId::User(_)) {
            return Err(TgError::InvalidInput(format!("{chat} is not a user")));
        }
        let common = self
            .data
            .profiles
            .get(&chat)
            .map_or(&[][..], |p| p.common_chats.as_slice());
        let known = self.peers();
        let peers: Vec<RawPeer> = common
            .iter()
            .filter_map(|id| known.iter().find(|p| p.id == *id).cloned())
            .take(limit)
            .collect();
        self.remember(&peers);
        Ok(find::common_chats(&peers, &|chat| self.known(chat), limit))
    }

    /// The same calls as the Live client, answered by [`MemberSource`] below.
    async fn get_members(&self, query: MembersQuery) -> Result<MemberPage, TgError> {
        members::get_members(&query, &|chat| self.known(chat), self).await
    }

    /// The same search calls as the Live client, answered by [`search_answer`].
    async fn search_messages(&self, query: SearchQuery) -> Result<SearchPage, TgError> {
        if let Some(seconds) = self.data.search_flood_wait {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Search,
            });
        }
        if let SearchScope::Chat { chat, from, topic } = query.scope {
            self.check_hash(chat)?;
            if !self.knows(chat.chat()) {
                return Err(TgError::InvalidInput(format!(
                    "{} is not one of your chats; chat ids come from list_chats",
                    chat.chat()
                )));
            }
            if let Some(SearchFrom::Peer(peer)) = from {
                self.check_hash(peer)?;
            }
            if let Some(topic) = topic {
                self.check_topic(chat.chat(), topic)?;
            }
        }
        let chats: Vec<FakeSearchChat> = self
            .data
            .messages
            .iter()
            .map(|(&id, messages)| FakeSearchChat {
                id,
                kind: self
                    .data
                    .chats
                    .iter()
                    .find(|c| c.id.parse::<ChatRef>().is_ok_and(|r| r.chat() == id))
                    .map_or_else(|| fallback_kind(id), |c| c.kind),
                archived: self.data.archived.contains(&id),
                messages,
            })
            .collect();
        let peers = self.peers();
        let me = self.data.account.id;
        let mut page = search::search(&query, &|chat| self.known(chat), |request| {
            // SEARCH_QUERY_EMPTY, as the Live client reports it.
            let answer = if request.q.is_empty()
                && request.filter.is_none()
                && !matches!(request.scope, RequestScope::Chat { from: Some(_), .. })
            {
                Err(TgError::InvalidInput(QUERY_REQUIRED.into()))
            } else {
                self.remember(&peers);
                let answer = search_answer(&chats, me, &request);
                let in_answer: Vec<ChatId> = answer.messages.iter().map(|f| f.chat).collect();
                let forums = self.forums_described(
                    &in_answer,
                    answer.messages.iter().map(|f| (f.chat, &f.message)),
                );
                Ok(SearchAnswer {
                    peers,
                    forums,
                    ..answer
                })
            };
            std::future::ready(answer)
        })
        .await?;
        topics::complete_search(&mut page, |chat, ids| {
            std::future::ready(
                self.check_hash(chat)
                    .and_then(|()| self.topics_by_id(chat.chat(), &ids)),
            )
        })
        .await?;
        Ok(page)
    }

    /// The same `contacts.search` call as the Live client, answered by
    /// [`contacts_search`].
    async fn find_chats(&self, query: &str, limit: usize) -> Result<FoundChats, TgError> {
        if let Some(seconds) = self.data.search_flood_wait {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Search,
            });
        }
        let me = self.data.account.id;
        let own: Vec<RawPeer> = self
            .peers()
            .into_iter()
            .filter(|p| p.id != ChatId::User(me))
            .collect();
        let public = Self::raw_peers(&self.data.public);
        find::find_chats(query, limit, &|chat| self.known(chat), |request| {
            let answer = contacts_search(&own, &public, &self.data.min_peers, &request);
            self.remember(&answer.peers);
            std::future::ready(Ok(answer))
        })
        .await
    }

    /// The same getMessages call as the Live client (and a reply chain),
    /// answered by [`get_message`], each message with its [`fake_file`].
    async fn media_file(&self, id: ChatRef, message_id: i32) -> Result<MediaFile, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        download::locate(id, message_id, |request| {
            std::future::ready(Ok(self.media_messages(chat, &request)))
        })
        .await
    }

    /// The same getMessages call as for a file, then the transcription
    /// service (`transcribeAudio` and the Account's updates).
    async fn transcribe(
        &self,
        id: ChatRef,
        message_id: i32,
        deadline: tokio::time::Instant,
    ) -> Result<Transcription, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        let feed = self.transcriber.feed(self.data.account.id);
        transcribe::transcribe(
            id,
            message_id,
            deadline,
            |request| std::future::ready(Ok(self.media_messages(chat, &request))),
            &feed,
            || {
                std::future::ready(
                    self.transcriber
                        .transcribe_audio(&self.data, chat, message_id),
                )
            },
        )
        .await
    }

    /// The same getMessages call as the Live client, for the ids around the
    /// message (outside channels each id is the Account's, so it may find
    /// another Chat's message, as [`get_message`] models), each message with
    /// its album and [`fake_picture`].
    async fn media_album(&self, id: ChatRef, message_id: i32) -> Result<Album, TgError> {
        self.flood()?;
        self.check_hash(id)?;
        let chat = id.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        view::locate_album(id, message_id, |ids| {
            let reference = {
                let mut state = self.files.state();
                state.album_fetches.push(ids.clone());
                state.reference()
            };
            let found = ids
                .iter()
                .flat_map(|&id| get_message(&self.data.messages, chat, &MessageRequest { id }))
                .map(|f| RawViewMessage {
                    at: MessageAt {
                        chat: f.chat,
                        id: f.message.id,
                    },
                    grouped_id: fake_picture_entry(&self.data, f.chat, f.message.id)
                        .and_then(|p| p.grouped_id),
                    picture: fake_picture(&self.data, &reference, f.chat, &f.message),
                    media: f.message.media,
                })
                .collect();
            std::future::ready(Ok(found))
        })
        .await
    }

    /// `upload.getFile` as Telegram answers it (core.telegram.org/api/files):
    /// `FILE_MIGRATE_X` from any DC but the file's, `FLOOD_WAIT_X` when
    /// flooded, `FILE_REFERENCE_EXPIRED` for a stale reference, the limit
    /// divisible by 4 KB and dividing 1 MB, the offset divisible by 4 KB, no
    /// part across a 1 MB boundary, and then the bytes from `offset`: up to
    /// `limit`, fewer (or none) at the end of the file. Only this Account's
    /// files are served: a message's file, a photo's sizes and a document's
    /// thumbnails, never an inline size (stripped, cached, path).
    async fn get_file(&self, request: FileRequest) -> Result<FilePart, FileError> {
        const MB: i64 = 1024 * 1024;
        // A round trip: other tasks (and timeouts) get to run.
        tokio::task::yield_now().await;
        let FileRequest {
            location,
            dc_id,
            offset,
            limit,
        } = request;
        let mut state = self.files.state();
        state.requests.push(FileRequestSeen {
            dc_id,
            offset,
            limit,
        });
        let home = state.dc_id.unwrap_or(FAKE_FILE_DC);
        if dc_id != home {
            return Err(FileError::Migrate(home));
        }
        if let Some((parts, seconds)) = state.flood_after
            && state.served >= parts
        {
            return Err(FileError::Failed(TgError::FloodWait {
                seconds,
                class: MethodClass::Files,
            }));
        }
        let current = state.reference();
        let file = self.data.messages.iter().find_map(|(&chat, messages)| {
            messages.iter().find_map(|m| {
                let sizes = fake_sizes(&self.data, chat, m);
                let picture = fake_picture(&self.data, &current, chat, m);
                let pictures = picture.iter().flat_map(|p| {
                    sizes
                        .iter()
                        .filter(|s| matches!(s.size.form, SizeForm::File | SizeForm::Progressive))
                        .map(|s| {
                            let content = FakeContent::Picture(&s.size.r#type, s.format);
                            (p.at(&s.size), s.size.bytes, content)
                        })
                });
                let whole = fake_file(&current, chat, m)
                    .map(|(stored, size)| (stored, size.unwrap_or(0), FakeContent::File));
                pictures
                    .chain(whole)
                    .find(|(stored, _, _)| {
                        stored.same_file(&location) && stored.access_hash == location.access_hash
                    })
                    .map(|(stored, size, content)| match content {
                        FakeContent::File => (stored.id, size, None),
                        FakeContent::Picture(r#type, format) => {
                            (stored.id, size, Some((r#type.to_string(), format)))
                        }
                    })
            })
        });
        let Some((file_id, size, picture)) = file else {
            return Err(FileError::Failed(TgError::Upstream(
                "rpc error 400: FILE_ID_INVALID".into(),
            )));
        };
        if state.references_never_last || location.file_reference != current {
            return Err(FileError::ReferenceExpired);
        }
        if limit <= 0 || limit % 4096 != 0 || MB % i64::from(limit) != 0 {
            return Err(FileError::Failed(TgError::Upstream(
                "rpc error 400: LIMIT_INVALID".into(),
            )));
        }
        if offset < 0 || offset % 4096 != 0 || offset / MB != (offset + i64::from(limit) - 1) / MB {
            return Err(FileError::Failed(TgError::Upstream(
                "rpc error 400: OFFSET_INVALID".into(),
            )));
        }
        state.served += 1;
        let size = state.cut_at.map_or(size, |cut| cut.min(size)) as i64;
        let (start, end) = (offset.min(size), (offset + i64::from(limit)).min(size));
        Ok(FilePart::Bytes(match picture {
            None => fake_file_range(file_id, start as u64..end as u64),
            Some((r#type, format)) => fake_picture_bytes(file_id, &r#type, size as u64, format)
                [start as usize..end as usize]
                .to_vec(),
        }))
    }

    /// Like `messages.getAllDrafts`: one `updateDraftMessage` per Draft,
    /// with the Chats and users they are in.
    async fn get_drafts(&self) -> Result<Drafts, TgError> {
        self.flood()?;
        let peers = self.peers();
        self.remember(&peers);
        let drafts = self
            .drafts
            .of(self.data.account.id)
            .into_iter()
            .map(|d| RawDraft {
                chat: d.chat,
                topic: d.topic,
                reply_to: d.reply_to,
                text: d.text.text,
                date: d.date,
            })
            .collect();
        Ok(drafts::assemble(DraftsAnswer {
            drafts,
            peers,
            forums: self.data.topics.keys().copied().collect(),
        }))
    }

    /// Like the Live client: [`compose::place`] with what Telegram knows of
    /// the Chat, then one `messages.saveDraft`, which Telegram refuses for a
    /// reply to a message the Chat doesn't have or a Topic the forum doesn't.
    async fn save_draft(&self, draft: NewDraft) -> Result<SavedDraft, TgError> {
        self.check_hash(draft.chat)?;
        let chat = draft.chat.chat();
        if !self.knows(chat) {
            return Err(TgError::InvalidInput(format!(
                "{chat} is not one of your chats; chat ids come from list_chats"
            )));
        }
        let message_topic = |id| {
            get_message(&self.data.messages, chat, &MessageRequest { id })
                .into_iter()
                .find(|found| found.chat == chat)
                .map(|found| found.message.topic)
        };
        let placement = compose::place(
            chat,
            draft.reply_to,
            draft.topic,
            || std::future::ready(Ok(self.data.topics.contains_key(&chat))),
            |id| std::future::ready(Ok(message_topic(id))),
        )
        .await?;
        if let Some(seconds) = self.data.writes_flood_wait {
            return Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Writes,
            });
        }
        if let Some(id) = placement.reply_to
            && message_topic(id).is_none()
        {
            return Err(TgError::InvalidInput(format!(
                "reply_to {id} is not a message of {chat}"
            )));
        }
        if let (None, Some(topic)) = (placement.reply_to, placement.topic) {
            self.check_topic(chat, topic)?;
        }
        let top = placement.reply_header().and_then(|(_, top)| top);
        let cleared = draft.text.text.is_empty();
        self.drafts.save(
            self.data.account.id,
            chat,
            top,
            placement.reply_to,
            draft.text,
        );
        Ok(SavedDraft {
            chat,
            topic: placement.topic,
            reply_to: placement.reply_to,
            cleared,
        })
    }
}

/// `channels.getFullChannel`, `channels.getParticipants` and
/// `messages.getFullChat` as Telegram answers them, on [`FakeMembers`].
#[async_trait::async_trait]
impl MemberSource for FakeTelegram {
    async fn channel_access(&self, channel: ChatRef) -> Result<ChannelAccess, TgError> {
        let group = self.group(channel)?;
        Ok(ChannelAccess {
            can_list: !group.admins_required,
            hidden: group.hidden,
        })
    }

    async fn participants(
        &self,
        request: ParticipantsRequest,
    ) -> Result<ParticipantsAnswer, TgError> {
        let group = self.group(request.channel)?;
        let chat = request.channel.chat();
        let admins = request.filter == ParticipantsFilter::Admins;
        if group.admins_required {
            return Err(members::admin_required(chat, admins));
        }
        let matching: Vec<&FakeMember> = group
            .members
            .iter()
            .filter(|m| !group.hidden || m.bot || m.role != Role::Member)
            .filter(|m| match &request.filter {
                ParticipantsFilter::Recent => true,
                ParticipantsFilter::Search(q) => m.found_by(q),
                ParticipantsFilter::Admins => m.role != Role::Member,
            })
            .collect();
        let listable = group.cap.unwrap_or(usize::MAX).min(matching.len());
        let start = usize::try_from(request.offset).unwrap_or(0).min(listable);
        // Telegram's largest limit.
        let limit = usize::try_from(request.limit).unwrap_or(0).min(200);
        let page = &matching[start..(start + limit).min(listable)];
        let peers = self.member_peers(page.iter().copied());
        self.remember(&peers);
        Ok(ParticipantsAnswer {
            count: i32::try_from(matching.len()).unwrap_or(i32::MAX),
            returned: page.len(),
            members: page
                .iter()
                .map(|m| RawMember {
                    user: m.id,
                    role: m.role,
                })
                .collect(),
            peers,
        })
    }

    async fn group_members(&self, chat_id: i64) -> Result<GroupAnswer, TgError> {
        let group = self.group(ChatId::Chat(chat_id).into())?;
        if group.forbidden {
            return Ok(GroupAnswer::default());
        }
        let peers = self.member_peers(group.members.iter());
        self.remember(&peers);
        Ok(GroupAnswer {
            members: Some(
                group
                    .members
                    .iter()
                    .map(|m| RawMember {
                        user: m.id,
                        role: m.role,
                    })
                    .collect(),
            ),
            peers,
        })
    }
}

impl FakeTelegram {
    fn flood(&self) -> Result<(), TgError> {
        match self.data.flood_wait {
            Some(seconds) => Err(TgError::FloodWait {
                seconds,
                class: MethodClass::Default,
            }),
            None => Ok(()),
        }
    }

    /// What one getMessages for a message's file finds (as [`get_message`]
    /// models it), each message with its [`fake_file`].
    fn media_messages(&self, chat: ChatId, request: &MessageRequest) -> Vec<RawMediaMessage> {
        let reference = {
            let mut state = self.files.state();
            state.lookups += 1;
            state.reference()
        };
        get_message(&self.data.messages, chat, request)
            .into_iter()
            .map(|f| RawMediaMessage {
                at: MessageAt {
                    chat: f.chat,
                    id: f.message.id,
                },
                file: fake_file(&reference, f.chat, &f.message),
                media: f.message.media,
            })
            .collect()
    }

    /// Like Telegram: an id with a wrong access hash is `CHANNEL_INVALID` /
    /// `PEER_ID_INVALID`, which the Live client reports this way.
    fn check_hash(&self, chat: ChatRef) -> Result<(), TgError> {
        match chat.access_hash() {
            Some(hash) if Some(hash) != fake_access_hash(chat.chat()) => Err(
                TgError::InvalidInput(format!("{} can't be read by this account", chat.chat())),
            ),
            _ => Ok(()),
        }
    }

    /// The fake knows a Chat if it lists it, has history for it or it is a
    /// contact.
    fn knows(&self, chat: ChatId) -> bool {
        self.data.messages.contains_key(&chat)
            || self
                .data
                .chats
                .iter()
                .chain(&self.data.contacts)
                .any(|c| c.id.parse::<ChatRef>().is_ok_and(|id| id.chat() == chat))
    }

    /// One `messages.getDialogs` as Telegram answers it, `chats` being in
    /// Telegram's order: the listing's folder, its pinned Chats first unless
    /// left out, then `limit` unpinned ones after the offset Chat. A Chat's top
    /// message is its `last_message`, with id 1.
    fn get_dialogs(&self, request: &DialogsRequest) -> Result<DialogsAnswer, TgError> {
        self.flood()?;
        let listing = request.listing;
        let folder: Vec<(ChatId, &ChatSummary)> = self
            .data
            .chats
            .iter()
            .filter_map(|c| Some((c.id.parse::<ChatRef>().ok()?.chat(), c)))
            .filter(|(id, _)| self.data.archived.contains(id) == listing.archived)
            .collect();
        let unpinned: Vec<&(ChatId, &ChatSummary)> =
            folder.iter().filter(|(_, c)| !c.pinned).collect();
        let start = match request
            .after
            .as_ref()
            .and_then(|c| c.offset_peer.as_deref())
        {
            None => 0,
            Some(peer) => unpinned
                .iter()
                .position(|(id, _)| id.to_string() == peer)
                .map_or(unpinned.len(), |i| i + 1),
        };
        let listed: Vec<&(ChatId, &ChatSummary)> = folder
            .iter()
            .filter(|(_, c)| c.pinned && !listing.exclude_pinned())
            .chain(unpinned.into_iter().skip(start).take(request.limit))
            .collect();
        Ok(self.dialogs_answer(&listed))
    }

    /// One `messages.getPeerDialogs`: the dialogs of the Chats asked for that
    /// the Account has, in its main list or the Archive. Like Telegram, it
    /// answers none for a Chat it doesn't have or a wrong access hash.
    fn peer_dialogs(&self, peers: &[ChatRef]) -> Result<DialogsAnswer, TgError> {
        self.flood()?;
        assert!(
            peers.len() <= folders::PEER_DIALOGS_CHUNK,
            "Telegram's limit"
        );
        let listed: Vec<(ChatId, &ChatSummary)> = peers
            .iter()
            .filter(|peer| self.check_hash(**peer).is_ok())
            .filter_map(|peer| {
                let chat = self.data.chats.iter().find(|c| {
                    c.id.parse::<ChatRef>()
                        .is_ok_and(|id| id.chat() == peer.chat())
                })?;
                Some((peer.chat(), chat))
            })
            .collect();
        let listed: Vec<&(ChatId, &ChatSummary)> = listed.iter().collect();
        Ok(self.dialogs_answer(&listed))
    }

    /// The dialogs of `listed` Chats as Telegram describes them: each in its
    /// folder (main list or Archive), its top message its `last_message`,
    /// with id 1.
    fn dialogs_answer(&self, listed: &[&(ChatId, &ChatSummary)]) -> DialogsAnswer {
        let summaries: Vec<ChatSummary> = listed.iter().map(|(_, c)| (*c).clone()).collect();
        let peers = Self::raw_peers(&summaries);
        self.remember(&peers);
        DialogsAnswer {
            sent: listed.len(),
            dialogs: listed
                .iter()
                .map(|(id, c)| RawDialog {
                    chat: *id,
                    top_message: 1,
                    unread_count: c.unread_count,
                    pinned: c.pinned,
                    folder_id: i32::from(self.data.archived.contains(id)),
                    unread_mark: c.marked_unread,
                    muted: c.muted,
                })
                .collect(),
            peers,
            messages: listed
                .iter()
                .filter_map(|(id, c)| {
                    let last = c.last_message.as_ref()?;
                    let date = time::OffsetDateTime::parse(
                        &last.date,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .ok()?;
                    Some(RawMessage {
                        chat: *id,
                        id: 1,
                        date: i32::try_from(date.unix_timestamp()).ok()?,
                        text: last.text.clone(),
                        media: last.media.map(|m| m.kind),
                    })
                })
                .collect(),
        }
    }

    /// One `messages.getDialogFilters`: the Account's Chat folders.
    fn dialog_filters(&self) -> Result<Vec<RawChatFolder>, TgError> {
        self.flood()?;
        if self.data.chat_folders_fail {
            return Err(TgError::Upstream("rpc error 500: INTERNAL".into()));
        }
        Ok(self
            .data
            .chat_folders
            .iter()
            .map(FakeChatFolder::raw)
            .collect())
    }

    /// Like the Live client: every hash an answer gives is kept.
    fn remember(&self, peers: &[RawPeer]) {
        self.known
            .lock()
            .unwrap()
            .extend(peers.iter().filter_map(|p| Some((p.id, p.access_hash?))));
    }

    fn known(&self, chat: ChatId) -> Option<i64> {
        self.known.lock().unwrap().get(&chat).copied()
    }

    /// Like the Live client: `topic` needs a forum, and Telegram refuses a
    /// Topic the forum doesn't have.
    fn check_topic(&self, chat: ChatId, topic: i32) -> Result<(), TgError> {
        let forum = self
            .data
            .topics
            .get(&chat)
            .ok_or_else(|| topics::not_a_forum(chat))?;
        if !forum.iter().any(|t| t.id == topic) {
            return Err(topics::unknown_topic(topic));
        }
        Ok(())
    }

    /// What an answer about `chats` and `messages` says of forums: which of
    /// those Chats are forums and, when the fake's answers carry them, the
    /// Topics of the messages.
    fn forums_described<'a>(
        &self,
        chats: &[ChatId],
        messages: impl Iterator<Item = (ChatId, &'a RawHistoryMessage)>,
    ) -> RawForums {
        let mut forums: Vec<ChatId> = chats
            .iter()
            .copied()
            .filter(|c| self.data.topics.contains_key(c))
            .collect();
        forums.sort_by_key(|&c| peer_key(c));
        forums.dedup();
        let mut described: Vec<RawTopic> = Vec::new();
        if self.data.topics_in_answers {
            for (chat, m) in messages {
                let topic = topic_of(m.topic);
                let known = described.iter().any(|t| t.chat == chat && t.id == topic);
                if let (false, Some(t)) = (known, self.topic(chat, topic)) {
                    described.push(t.raw(chat));
                }
            }
        }
        RawForums {
            forums,
            topics: described,
        }
    }

    fn topic(&self, chat: ChatId, id: i32) -> Option<&FakeTopic> {
        self.data.topics.get(&chat)?.iter().find(|t| t.id == id)
    }

    /// `messages.getForumTopicsByID`, a reading call: the Topics of `ids`
    /// the forum has.
    fn topics_by_id(&self, chat: ChatId, ids: &[i32]) -> Result<Vec<RawTopic>, TgError> {
        self.flood()?;
        Ok(ids
            .iter()
            .filter_map(|&id| self.topic(chat, id))
            .map(|t| t.raw(chat))
            .collect())
    }

    /// One call's worth of a group's members: like any reading call, a
    /// flood or a wrong hash first; a Chat the fake doesn't know is not the
    /// Account's.
    fn group(&self, chat: ChatRef) -> Result<FakeMembers, TgError> {
        self.flood()?;
        self.check_hash(chat)?;
        let id = chat.chat();
        match self.data.members.get(&id) {
            Some(group) => Ok(group.clone()),
            None if self.knows(id) => Ok(FakeMembers::default()),
            None => Err(TgError::InvalidInput(format!(
                "{id} can't be read by this account"
            ))),
        }
    }

    /// Members as an answer's `users` describes them.
    fn member_peers<'a>(&self, members: impl Iterator<Item = &'a FakeMember>) -> Vec<RawPeer> {
        members.map(|m| self.member_peer(m)).collect()
    }

    fn member_peer(&self, m: &FakeMember) -> RawPeer {
        let id = ChatId::User(m.id);
        RawPeer {
            id,
            title: m.name.clone(),
            kind: if m.bot {
                ChatKind::Bot
            } else {
                ChatKind::Private
            },
            username: m.username.clone(),
            access_hash: fake_access_hash(id),
            is_self: m.id == self.data.account.id,
        }
    }

    fn raw_peers(chats: &[ChatSummary]) -> Vec<RawPeer> {
        chats
            .iter()
            .filter_map(|c| {
                let id = c.id.parse::<ChatRef>().ok()?.chat();
                Some(RawPeer {
                    id,
                    title: c.title.clone(),
                    kind: c.kind,
                    username: c.username.clone(),
                    access_hash: fake_access_hash(id),
                    is_self: c.is_self,
                })
            })
            .collect()
    }

    /// The Account's Chats, its contacts and itself.
    fn peers(&self) -> Vec<RawPeer> {
        let me = &self.data.account;
        Self::raw_peers(&self.data.chats)
            .into_iter()
            .chain(Self::raw_peers(&self.data.contacts))
            .chain(std::iter::once(RawPeer {
                id: ChatId::User(me.id),
                title: me.display_name(),
                kind: ChatKind::Private,
                username: me.username.clone(),
                access_hash: fake_access_hash(ChatId::User(me.id)),
                is_self: true,
            }))
            // Telegram describes every user a member list named.
            .chain(
                self.data
                    .members
                    .values()
                    .flat_map(|group| group.members.iter())
                    .map(|m| self.member_peer(m)),
            )
            .collect()
    }
}

pub fn default_account() -> Account {
    Account {
        id: 777_000,
        first_name: "Fake".into(),
        last_name: Some("Account".into()),
        username: Some("fake_account".into()),
    }
}

/// A few Chats, so the fake server has something to show.
pub fn sample_chats() -> Vec<ChatSummary> {
    let chat =
        |id: &str, title: &str, kind, unread, text: &str, media: Option<MediaKind>| ChatSummary {
            id: id.into(),
            title: title.into(),
            kind,
            username: None,
            is_self: false,
            unread_count: unread,
            pinned: false,
            muted: false,
            marked_unread: false,
            last_message: Some(MessagePreview {
                date: "2026-11-06T12:00:00Z".into(),
                text: text.into(),
                media: media.map(|kind| MediaSummary { kind }),
            }),
        };
    vec![
        chat(
            "user:101",
            "Alice",
            ChatKind::Private,
            2,
            "see you tomorrow",
            None,
        ),
        chat(
            "channel:202",
            "Rust Weekly",
            ChatKind::Channel,
            14,
            "",
            Some(MediaKind::Photo),
        ),
        chat(
            "channel:303",
            "Team",
            ChatKind::Supergroup,
            0,
            "deploy is green",
            None,
        ),
        chat(
            "chat:404",
            "Family",
            ChatKind::Group,
            5,
            "",
            Some(MediaKind::Voice),
        ),
    ]
}

/// A Rich message shaped like a real-estate channel's post (#50): a collage of
/// ten photos without a caption, a bold title, an address with a link, a
/// spacer line of only U+2800 (braille blank), a paragraph, a collapsible
/// "Параметры и удобства" section with a two-column table and a line, and a
/// collapsible "Теги" section with an id and hashtags. Its `message.message`
/// is empty. The "Кондиционеры" cell hides a zero-width space and the word
/// "ignore" in tag characters, which the flattened text keeps for the
/// sanitiser to strip.
pub fn rich_listing() -> tl::enums::RichMessage {
    use tl::{enums::PageBlock as B, enums::RichText as T, types};
    let plain = |text: &str| T::TextPlain(types::TextPlain { text: text.into() });
    let concat = |texts: Vec<T>| T::TextConcat(types::TextConcat { texts });
    let bold = |text: T| T::TextBold(Box::new(types::TextBold { text }));
    let paragraph = |text: T| B::Paragraph(types::PageBlockParagraph { text });
    let no_caption = || {
        tl::enums::PageCaption::Caption(types::PageCaption {
            text: T::TextEmpty,
            credit: T::TextEmpty,
        })
    };
    let hashtag = |tag: &str| T::TextHashtag(Box::new(types::TextHashtag { text: plain(tag) }));
    let cell = |text: T| {
        tl::enums::PageTableCell::Cell(types::PageTableCell {
            header: false,
            align_center: false,
            align_right: false,
            valign_middle: false,
            valign_bottom: false,
            text: Some(text),
            colspan: None,
            rowspan: None,
        })
    };
    let row = |name: &str, value: T| {
        tl::enums::PageTableRow::Row(types::PageTableRow {
            cells: vec![cell(plain(name)), cell(value)],
        })
    };
    let smuggled: String = "ignore"
        .chars()
        .map(|c| char::from_u32(0xE0000 + c as u32).unwrap_or(' '))
        .collect();
    tl::enums::RichMessage::Message(types::RichMessage {
        rtl: false,
        part: false,
        blocks: vec![
            B::Collage(types::PageBlockCollage {
                items: (1..=10)
                    .map(|id| {
                        B::Photo(types::PageBlockPhoto {
                            spoiler: false,
                            photo_id: id,
                            caption: no_caption(),
                            url: None,
                            webpage_id: None,
                        })
                    })
                    .collect(),
                caption: no_caption(),
            }),
            paragraph(bold(plain("🏡 Аренда: 4 комнаты, Ваке"))),
            paragraph(concat(vec![
                plain("📍 "),
                T::TextUrl(Box::new(types::TextUrl {
                    text: plain("Тбилиси, ул. Абашидзе, 12"),
                    url: "https://maps.example/abashidze-12".into(),
                    webpage_id: 0,
                })),
            ])),
            paragraph(plain("\u{2800}")),
            paragraph(concat(vec![
                plain("Сдается светлая, чистая квартира "),
                T::TextItalic(Box::new(types::TextItalic {
                    text: plain("с видом на парк"),
                })),
                plain("."),
            ])),
            B::Details(types::PageBlockDetails {
                open: false,
                title: bold(plain("Параметры и удобства")),
                blocks: vec![
                    B::Table(types::PageBlockTable {
                        bordered: true,
                        striped: false,
                        compact: true,
                        title: T::TextEmpty,
                        rows: vec![
                            row("Комнаты", plain("4")),
                            row(
                                "Кондиционеры",
                                concat(vec![
                                    bold(plain("1")),
                                    plain(&format!("\u{200B}{smuggled}")),
                                ]),
                            ),
                            row("Площадь", plain("120 м²")),
                        ],
                    }),
                    paragraph(plain("✅ Можно с животными")),
                ],
            }),
            B::Details(types::PageBlockDetails {
                open: false,
                title: plain("Теги"),
                blocks: vec![
                    paragraph(plain("ID: 19752")),
                    paragraph(concat(vec![
                        hashtag("#аренда"),
                        plain(" "),
                        hashtag("#ваке"),
                    ])),
                ],
            }),
        ],
        photos: Vec::new(),
        documents: Vec::new(),
    })
}

/// Logs in as a fixed Account: shows a fake QR once, then succeeds, after the
/// Cloud password if [`FakeLogin::with_password`] set one.
#[derive(Debug, Clone)]
pub struct FakeLogin {
    data: FakeData,
    password: Option<FakePassword>,
}

/// A Cloud password (2FA) on a fake Account.
#[derive(Clone)]
pub struct FakePassword {
    pub password: String,
    pub hint: Option<String>,
    /// Like Telegram's limit on wrong attempts: the attempt that follows this many
    /// wrong passwords is answered `FLOOD_WAIT_30` (once) instead of being checked.
    pub flood_after: Option<u32>,
}

/// Seconds of the fake `FLOOD_WAIT`.
pub const FAKE_FLOOD_WAIT: u32 = 30;

impl fmt::Debug for FakePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakePassword")
            .field("hint", &self.hint)
            .field("flood_after", &self.flood_after)
            .finish_non_exhaustive()
    }
}

impl FakeLogin {
    pub fn new(data: impl Into<FakeData>) -> Self {
        Self {
            data: data.into(),
            password: None,
        }
    }

    /// An Account with two-step verification: asks for `password` after the QR.
    pub fn with_password(data: impl Into<FakeData>, password: FakePassword) -> Self {
        Self {
            data: data.into(),
            password: Some(password),
        }
    }
}

impl Default for FakeLogin {
    fn default() -> Self {
        Self::new(FakeData::new(default_account()).with_chats(sample_chats()))
    }
}

#[async_trait::async_trait]
impl Login for FakeLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        Ok(Box::new(FakeAttempt {
            data: self.data.clone(),
            polled: false,
            password: self.password.clone(),
            wrong: 0,
            flooded: false,
        }))
    }
}

struct FakeAttempt {
    data: FakeData,
    polled: bool,
    /// The Cloud password still to be entered; `None` once it was (or if there is none).
    password: Option<FakePassword>,
    wrong: u32,
    flooded: bool,
}

impl FakeAttempt {
    fn logged_in(&self) -> LoggedIn {
        let session = serde_json::to_vec(&self.data).expect("FakeData serializes");
        LoggedIn {
            account: self.data.account.clone(),
            telegram_session: Zeroizing::new(session),
        }
    }
}

#[async_trait::async_trait]
impl LoginAttempt for FakeAttempt {
    async fn poll(&mut self) -> Result<LoginState, TgError> {
        if !self.polled {
            self.polled = true;
            return Ok(LoginState::WaitingForScan {
                qr_url: "tg://login?token=fake".into(),
            });
        }
        if let Some(password) = &self.password {
            return Ok(LoginState::PasswordNeeded {
                hint: password.hint.clone(),
            });
        }
        Ok(LoginState::LoggedIn(self.logged_in()))
    }

    async fn submit_password(&mut self, password: Zeroizing<String>) -> Result<LoggedIn, TgError> {
        let Some(expected) = self.password.as_ref().filter(|_| self.polled) else {
            return Err(TgError::Upstream("not waiting for a cloud password".into()));
        };
        if expected.flood_after == Some(self.wrong) && !self.flooded {
            self.flooded = true;
            return Err(TgError::FloodWait {
                seconds: FAKE_FLOOD_WAIT,
                class: MethodClass::Default,
            });
        }
        if *password != expected.password {
            self.wrong += 1;
            return Err(TgError::WrongPassword);
        }
        self.password = None;
        Ok(self.logged_in())
    }
}

/// Connects a Grant to the data recorded in its fake Telegram session. Clones
/// share one fake "Telegram side": which Accounts' sessions are dead, which were
/// logged out, and whether Telegram is reachable.
#[derive(Debug, Clone, Default)]
pub struct FakeConnector {
    telegram: Arc<FakeTelegramSide>,
}

#[derive(Debug, Default)]
struct FakeTelegramSide {
    /// Accounts whose session Telegram considers ended.
    dead: Mutex<HashSet<i64>>,
    /// Accounts `auth.logOut` was called for, in order.
    logged_out: Mutex<Vec<i64>>,
    /// The Account of each Grant with a Live client, as the Client pool holds
    /// them: from its first connect until it is logged out or shut down.
    live: Mutex<std::collections::HashMap<String, i64>>,
    unreachable: std::sync::atomic::AtomicBool,
    /// How many times [`Connector::shutdown`] was called.
    shutdowns: std::sync::atomic::AtomicUsize,
    /// Each Grant's peer cache, kept across requests as the Live client
    /// pool keeps a Grant's Live client.
    known: Mutex<std::collections::HashMap<String, KnownPeers>>,
    /// Telegram's file servers, the same for every Grant.
    files: Arc<FakeFiles>,
    /// Telegram's transcription service, the same for every Grant.
    transcriber: Arc<FakeTranscriber>,
    /// Every Account's Drafts, kept by Telegram for all of its Grants.
    drafts: Arc<FakeDrafts>,
}

impl FakeConnector {
    /// The user ended the session in Telegram (Settings → Devices → Terminate).
    pub fn end_session_in_telegram(&self, account_id: i64) {
        self.telegram.dead.lock().unwrap().insert(account_id);
    }

    /// Accounts whose session was logged out through [`Connector::log_out`].
    pub fn logged_out(&self) -> Vec<i64> {
        self.telegram.logged_out.lock().unwrap().clone()
    }

    /// How many Grants have a Live client.
    pub fn live_clients(&self) -> usize {
        self.telegram.live.lock().unwrap().len()
    }

    /// How many times the server shut this connector down.
    pub fn shutdowns(&self) -> usize {
        self.telegram
            .shutdowns
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Telegram's file servers, where every Grant's downloads go.
    pub fn files(&self) -> Arc<FakeFiles> {
        self.telegram.files.clone()
    }

    /// Telegram's transcription service, where every Grant's transcriptions go.
    pub fn transcriber(&self) -> Arc<FakeTranscriber> {
        self.telegram.transcriber.clone()
    }

    /// Telegram's Drafts, where every Grant's Drafts go.
    pub fn drafts(&self) -> Arc<FakeDrafts> {
        self.telegram.drafts.clone()
    }

    /// Make every call to Telegram fail as if it were down.
    pub fn set_unreachable(&self, unreachable: bool) {
        self.telegram
            .unreachable
            .store(unreachable, std::sync::atomic::Ordering::SeqCst);
    }

    fn data(grant: &OpenedGrant) -> Result<FakeData, TgError> {
        serde_json::from_slice(&grant.telegram_session)
            .map_err(|e| TgError::BadSession(e.to_string()))
    }

    /// Drop a Grant's Live client; its Account, if it had one.
    fn evict(&self, grant_id: &GrantId) -> Option<i64> {
        self.telegram
            .known
            .lock()
            .unwrap()
            .remove(grant_id.as_str());
        self.telegram.live.lock().unwrap().remove(grant_id.as_str())
    }

    /// `auth.logOut` for an Account's session.
    fn end_session(&self, account_id: i64) -> Result<(), TgError> {
        self.reachable()?;
        self.telegram.dead.lock().unwrap().insert(account_id);
        self.telegram.logged_out.lock().unwrap().push(account_id);
        Ok(())
    }

    fn reachable(&self) -> Result<(), TgError> {
        if self
            .telegram
            .unreachable
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TgError::Upstream("fake telegram is unreachable".into()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, grant: OpenedGrant) -> Result<Arc<dyn Telegram>, TgError> {
        let data = Self::data(&grant)?;
        self.reachable()?;
        if self
            .telegram
            .dead
            .lock()
            .unwrap()
            .contains(&data.account.id)
        {
            return Err(TgError::BadSession("fake session was ended".into()));
        }
        let known = self
            .telegram
            .known
            .lock()
            .unwrap()
            .entry(grant.grant_id.as_str().to_string())
            .or_default()
            .clone();
        self.telegram
            .live
            .lock()
            .unwrap()
            .insert(grant.grant_id.as_str().to_string(), data.account.id);
        self.telegram.drafts.seed(data.account.id, &data.drafts);
        Ok(Arc::new(FakeTelegram {
            data,
            known,
            files: self.telegram.files.clone(),
            transcriber: self.telegram.transcriber.clone(),
            drafts: self.telegram.drafts.clone(),
        }))
    }

    async fn log_out(&self, grant: OpenedGrant) -> Result<(), TgError> {
        let account_id = Self::data(&grant)?.account.id;
        self.evict(&grant.grant_id);
        self.end_session(account_id)
    }

    async fn evict_and_log_out(&self, grant_id: &GrantId) -> Result<bool, TgError> {
        let Some(account_id) = self.evict(grant_id) else {
            return Ok(false);
        };
        self.end_session(account_id).map(|()| true)
    }

    /// Nothing is held open; the peer caches go, as with the Live clients.
    async fn shutdown(&self) {
        self.telegram.live.lock().unwrap().clear();
        self.telegram.known.lock().unwrap().clear();
        self.telegram
            .shutdowns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids with gaps (deleted messages, or a private chat's account-wide ids),
    /// shuffled: the model must not rely on the order it is given.
    fn history() -> Vec<RawHistoryMessage> {
        [3, 50, 1, 9, 40, 10, 2, 30, 20]
            .into_iter()
            .map(|id| RawHistoryMessage {
                id,
                date: 1000 + id,
                from: None,
                out: false,
                text: String::new(),
                media: None,
                reply_to: None,
                reply_elsewhere: None,
                forwarded: false,
                forwarded_from: None,
                edited: false,
                reactions: Vec::new(),
                pinned: false,
                topic: None,
            })
            .collect()
    }

    fn ids(request: HistoryRequest, past_newest: PastNewest) -> Vec<i32> {
        get_history(&history(), &request, past_newest)
            .iter()
            .map(|m| m.id)
            .collect()
    }

    fn slice(offset_id: i32, add_offset: i32, limit: i32) -> HistoryRequest {
        HistoryRequest {
            offset_id,
            add_offset,
            limit,
            ..HistoryRequest::default()
        }
    }

    #[test]
    fn the_offsets_page_examples() {
        // core.telegram.org/api/offsets: older than MSGID, newer than MSGID
        // (it counts MSGID itself in), around MSGID.
        let shorten = PastNewest::Shorten;
        assert_eq!(ids(slice(10, 0, 3), shorten), [9, 3, 2]);
        assert_eq!(ids(slice(10, -3, 3), shorten), [30, 20, 10]);
        assert_eq!(ids(slice(10, -2, 4), shorten), [20, 10, 9, 3]);
        // No offset: from the newest.
        assert_eq!(ids(slice(0, 0, 2), shorten), [50, 40]);
    }

    #[test]
    fn a_missing_offset_id_counts_like_the_next_older_one() {
        // 15 doesn't exist: it sits between 20 and 10.
        let shorten = PastNewest::Shorten;
        assert_eq!(ids(slice(15, 0, 2), shorten), [10, 9]);
        assert_eq!(ids(slice(15, -1, 2), shorten), [20, 10]);
    }

    #[test]
    fn a_slice_past_the_newest_message_shortens_or_shifts() {
        // Only 50 is above 40: four at and above 40 reach past the newest.
        assert_eq!(ids(slice(40, -4, 5), PastNewest::Shorten), [50, 40, 30]);
        assert_eq!(
            ids(slice(40, -4, 5), PastNewest::ShiftOlder),
            [50, 40, 30, 20, 10]
        );
        // Past the oldest: just ends.
        assert_eq!(ids(slice(2, 0, 5), PastNewest::Shorten), [1]);
    }

    #[test]
    fn min_and_max_id_apply_after_the_slice() {
        let request = HistoryRequest {
            min_id: 9,
            max_id: 40,
            ..slice(0, 0, 4)
        };
        // The slice is 50, 40, 30, 20; the bounds leave 30 and 20, not 4 messages.
        assert_eq!(ids(request, PastNewest::Shorten), [30, 20]);
    }

    #[test]
    fn offset_date_starts_strictly_before_it_unless_an_id_is_given() {
        let by_date = HistoryRequest {
            offset_date: 1030,
            ..slice(0, 0, 2)
        };
        assert_eq!(ids(by_date, PastNewest::Shorten), [20, 10]);
        let both = HistoryRequest {
            offset_id: 10,
            ..by_date
        };
        assert_eq!(ids(both, PastNewest::Shorten), [9, 3]);
    }

    #[test]
    fn get_messages_looks_in_the_channel_or_across_all_other_chats() {
        let message = |id| RawHistoryMessage {
            id,
            ..history()[0].clone()
        };
        let (alice, bob, group, channel) = (
            ChatId::User(1),
            ChatId::User(2),
            ChatId::Chat(3),
            ChatId::Channel(4),
        );
        let histories = std::collections::HashMap::from([
            (alice, vec![message(10)]),
            (bob, vec![message(11)]),
            (group, vec![message(12)]),
            (channel, vec![message(10), message(11)]),
        ]);
        let found = |chat, id| -> Vec<(ChatId, i32)> {
            get_message(&histories, chat, &MessageRequest { id })
                .into_iter()
                .map(|f| (f.chat, f.message.id))
                .collect()
        };
        // messages.getMessages: the Account's ids, whichever chat asks.
        for asking in [alice, bob, group] {
            assert_eq!(found(asking, 10), [(alice, 10)]);
            assert_eq!(found(asking, 11), [(bob, 11)]);
            assert_eq!(found(asking, 12), [(group, 12)]);
        }
        // channels.getMessages: that channel only.
        assert_eq!(found(channel, 10), [(channel, 10)]);
        assert_eq!(found(channel, 12), []);
        assert_eq!(found(ChatId::Channel(5), 10), []);
        // Deleted or never there: messageEmpty.
        assert_eq!(found(alice, 13), []);
    }

    #[test]
    fn limit_is_cut_to_telegrams_maximum() {
        let long: Vec<RawHistoryMessage> = (1..=150)
            .map(|id| RawHistoryMessage {
                id,
                ..history()[0].clone()
            })
            .collect();
        assert_eq!(
            get_history(&long, &slice(0, 0, 500), PastNewest::Shorten).len(),
            100
        );
    }
}
