//! Members of a group, supergroup or channel (`get_members`), independent of
//! grammers/TL.
//!
//! A supergroup or channel is listed by Telegram, a page at a time
//! (`channels.getParticipants`: the recent members, or Telegram's member
//! search for `query`), and the cursor carries Telegram's offset. A basic
//! group's members all come in `messages.getFullChat` (at most 200), so they
//! are filtered and paged here, and so are a supergroup's admins: Telegram's
//! admins filter takes no query, and a supergroup has at most 50 admins, so
//! one call gets them all and `query` is applied to them here.
//!
//! Telegram hides members in some places: a broadcast channel's subscribers
//! are shown only to its admins (`can_view_participants`, else
//! `CHAT_ADMIN_REQUIRED`), and a supergroup that hides its members
//! (`participants_hidden`) shows non-admins only its admins and bots. The
//! first page of a supergroup or channel asks `channels.getFullChannel`
//! which of these applies, so the answer can say so.

use std::collections::HashSet;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    TgError,
    chats::{ChatId, ChatKind, ChatRef, RawPeer},
    messages::{Issuer, KnownHashes},
};

/// Most members one page holds, and Telegram's largest `limit` for
/// `channels.getParticipants`.
pub const MAX_MEMBERS_PER_PAGE: usize = 200;
/// Longest `query`, in characters.
pub const MAX_MEMBER_QUERY_CHARS: usize = 64;
/// Longest cursor this module issues is well under this; anything longer is
/// not one of ours.
pub const MAX_MEMBER_CURSOR_LEN: usize = 128;
/// Highest offset a cursor may carry: far past where Telegram stops listing a
/// large group (about 10,000 members).
const MAX_OFFSET: u32 = 1_000_000;

/// What a member is in the Chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Created the group or channel.
    Creator,
    Admin,
    Member,
}

impl Role {
    fn is_admin(self) -> bool {
        matches!(self, Role::Creator | Role::Admin)
    }
}

/// One member, as get_members shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Member {
    /// A `user:N~H` id: pass it to get_chat_info, or as search_messages `from`.
    pub id: String,
    /// First and last name.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub role: Role,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bot: bool,
    /// The connected account itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
}

/// One page of a Chat's members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemberPage {
    /// In Telegram's order.
    #[schemars(length(max = 200))]
    pub members: Vec<Member>,
    /// How many members match (all of them without `query` / `admins_only`),
    /// as Telegram counts them: in a large group more than can be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<i32>,
    /// The group hides its members from non-admins: Telegram lists only its
    /// admins and bots to this account.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub members_hidden: bool,
    /// Pass back as `cursor`, with the same `chat_id`, `query` and
    /// `admins_only`, for the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Which members to list. The tool has checked the bounds already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembersQuery {
    pub chat: ChatRef,
    /// A name or @username, or part of one.
    pub query: Option<String>,
    pub admins_only: bool,
    pub limit: usize,
    /// A `next_cursor` of the same listing.
    pub cursor: Option<String>,
}

// ── What goes to Telegram and comes back ────────────────────────────────────

/// Which of a channel's participants `channels.getParticipants` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipantsFilter {
    /// `channelParticipantsRecent`: everyone, most recent first.
    Recent,
    /// `channelParticipantsSearch`: Telegram's member search.
    Search(String),
    /// `channelParticipantsAdmins`: the creator and the admins.
    Admins,
}

/// One `channels.getParticipants` call, as its TL parameters (`hash` 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantsRequest {
    pub channel: ChatRef,
    pub filter: ParticipantsFilter,
    pub offset: i32,
    pub limit: i32,
}

/// A member as Telegram lists it: a user and their role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMember {
    pub user: i64,
    pub role: Role,
}

/// What Telegram answered to one [`ParticipantsRequest`].
#[derive(Debug, Clone, Default)]
pub struct ParticipantsAnswer {
    /// Telegram's `count`: how many participants match the filter.
    pub count: i32,
    /// How many participants Telegram sent, those that aren't members
    /// (banned, left) included: the next page's offset moves by this.
    pub returned: usize,
    /// The members among them, in Telegram's order.
    pub members: Vec<RawMember>,
    /// The answer's `users` and `chats`.
    pub peers: Vec<RawPeer>,
}

/// What `channels.getFullChannel` says of listing a channel's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelAccess {
    /// `can_view_participants`: Telegram lists the members to this account.
    pub can_list: bool,
    /// `participants_hidden`, and the account is no admin: Telegram lists
    /// only the admins and bots.
    pub hidden: bool,
}

/// What `messages.getFullChat` says of a basic group's members.
#[derive(Debug, Clone, Default)]
pub struct GroupAnswer {
    /// `None`: `chatParticipantsForbidden`, the account isn't in the group.
    pub members: Option<Vec<RawMember>>,
    pub peers: Vec<RawPeer>,
}

/// Where members come from: the Live client, or the fake's model of Telegram.
#[async_trait::async_trait]
pub trait MemberSource: Sync {
    /// One `channels.getFullChannel` call.
    async fn channel_access(&self, channel: ChatRef) -> Result<ChannelAccess, TgError>;

    /// One `channels.getParticipants` call. Telegram's `CHAT_ADMIN_REQUIRED`
    /// is [`admin_required`].
    async fn participants(
        &self,
        request: ParticipantsRequest,
    ) -> Result<ParticipantsAnswer, TgError>;

    /// One `messages.getFullChat` call.
    async fn group_members(&self, chat_id: i64) -> Result<GroupAnswer, TgError>;
}

/// Telegram's `CHAT_ADMIN_REQUIRED` for a member listing, as the tools report it.
pub fn admin_required(chat: ChatId, admins_only: bool) -> TgError {
    let hint = if admins_only {
        "Telegram doesn't show its admins to this account either."
    } else {
        "In a group that hides its members, admins_only: true may still list the admins."
    };
    TgError::InvalidInput(format!(
        "Telegram shows the members of {chat} only to its admins, and this account isn't one \
         (a broadcast channel's subscribers, or a group that hides its members). {hint}"
    ))
}

/// What get_members on a user id says.
pub fn not_a_group(chat: ChatId) -> TgError {
    TgError::InvalidInput(format!(
        "{chat} is a user, not a group or channel: get_members lists the members of a group, \
         supergroup or channel (get_chat_info with common_chats: true lists the groups shared \
         with a user)"
    ))
}

/// What a reused cursor that belongs to another listing says.
pub const CURSOR_MISMATCH: &str = "cursor belongs to a different listing: chat_id, query or \
admins_only changed. Start again without cursor, or repeat the call exactly as before.";

/// `query` as it is matched: trimmed, without a leading `@`; `None` if
/// nothing is left.
pub fn canonical_query(query: Option<&str>) -> Option<String> {
    let query = query?.trim();
    let query = query.strip_prefix('@').unwrap_or(query).trim();
    (!query.is_empty()).then(|| query.to_string())
}

/// Answer `query` with the calls it takes on `source`; ids the answers leave
/// without a hash take the one in `known`.
pub async fn get_members(
    query: &MembersQuery,
    known: KnownHashes<'_>,
    source: &(dyn MemberSource + '_),
) -> Result<MemberPage, TgError> {
    let chat = query.chat.chat();
    if !(1..=MAX_MEMBERS_PER_PAGE).contains(&query.limit) {
        return Err(TgError::InvalidInput("limit is out of range".into()));
    }
    let text = canonical_query(query.query.as_deref());
    if text
        .as_deref()
        .is_some_and(|t| t.chars().count() > MAX_MEMBER_QUERY_CHARS)
    {
        return Err(TgError::InvalidInput("query is too long".into()));
    }
    let fingerprint = fingerprint(chat, text.as_deref(), query.admins_only);
    let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
    if cursor
        .as_ref()
        .is_some_and(|c| c.fingerprint != fingerprint)
    {
        return Err(TgError::InvalidInput(CURSOR_MISMATCH.into()));
    }
    let offset = cursor.as_ref().map_or(0, |c| c.offset);
    let paging = Paging {
        fingerprint,
        offset,
        limit: query.limit,
    };
    match chat {
        ChatId::User(_) => Err(not_a_group(chat)),
        ChatId::Chat(chat_id) => {
            let answer = source.group_members(chat_id).await?;
            let members = answer.members.ok_or_else(|| {
                TgError::InvalidInput(format!(
                    "{chat}: this account is no longer in this group, so Telegram doesn't show \
                     its members"
                ))
            })?;
            let issuer = Issuer::new(&answer.peers, known);
            Ok(paging.local(&issuer, &members, text.as_deref(), query.admins_only))
        }
        ChatId::Channel(_) if query.admins_only => {
            let answer = source
                .participants(ParticipantsRequest {
                    channel: query.chat,
                    filter: ParticipantsFilter::Admins,
                    offset: 0,
                    limit: MAX_MEMBERS_PER_PAGE as i32,
                })
                .await?;
            let issuer = Issuer::new(&answer.peers, known);
            Ok(paging.local(&issuer, &answer.members, text.as_deref(), true))
        }
        ChatId::Channel(_) => {
            // The first page finds out whether Telegram hides members; the
            // cursor remembers it for the next ones.
            let hidden = match &cursor {
                Some(cursor) => cursor.hidden,
                None => {
                    let access = source.channel_access(query.chat).await?;
                    if !access.can_list {
                        return Err(admin_required(chat, false));
                    }
                    access.hidden
                }
            };
            let answer = source
                .participants(ParticipantsRequest {
                    channel: query.chat,
                    filter: text.map_or(ParticipantsFilter::Recent, ParticipantsFilter::Search),
                    offset: i32::try_from(offset).unwrap_or(i32::MAX),
                    limit: query.limit as i32,
                })
                .await?;
            Ok(paging.telegrams(answer, known, hidden))
        }
    }
}

/// Where a page starts and how long it is, and which listing it belongs to.
struct Paging {
    fingerprint: u64,
    offset: u32,
    limit: usize,
}

impl Paging {
    fn cursor(&self, offset: usize, hidden: bool) -> Option<String> {
        let offset = u32::try_from(offset).ok().filter(|o| *o <= MAX_OFFSET)?;
        Some(
            Cursor {
                fingerprint: self.fingerprint,
                offset,
                hidden,
            }
            .encode(),
        )
    }

    /// A page of a list that came whole: filtered and paged here.
    fn local(
        &self,
        issuer: &Issuer<'_>,
        members: &[RawMember],
        query: Option<&str>,
        admins_only: bool,
    ) -> MemberPage {
        let query = query.map(str::to_lowercase);
        let mut seen = HashSet::new();
        let matching: Vec<Member> = members
            .iter()
            .filter(|m| !admins_only || m.role.is_admin())
            .filter(|m| seen.insert(m.user))
            .map(|m| member(issuer, *m))
            .filter(|m| query.as_deref().is_none_or(|q| matches(m, q)))
            .collect();
        let start = (self.offset as usize).min(matching.len());
        let end = (start + self.limit).min(matching.len());
        MemberPage {
            next_cursor: (end < matching.len())
                .then(|| self.cursor(end, false))
                .flatten(),
            total: i32::try_from(matching.len()).ok(),
            members: matching[start..end].to_vec(),
            members_hidden: false,
        }
    }

    /// A page as Telegram listed it: the next one starts after what it sent,
    /// while Telegram's count says there is more.
    fn telegrams(
        &self,
        answer: ParticipantsAnswer,
        known: KnownHashes<'_>,
        hidden: bool,
    ) -> MemberPage {
        let ParticipantsAnswer {
            count,
            returned,
            mut members,
            peers,
        } = answer;
        let issuer = Issuer::new(&peers, known);
        // Telegram sends at most `limit`; more would be cut, and the next
        // page starts after the cut.
        members.truncate(self.limit);
        let returned = returned.min(self.limit);
        let mut seen = HashSet::new();
        let members = members
            .into_iter()
            .filter(|m| seen.insert(m.user))
            .map(|m| member(&issuer, m))
            .collect();
        let next = self.offset as usize + returned;
        let more = returned > 0 && next < usize::try_from(count).unwrap_or(0);
        MemberPage {
            members,
            total: Some(count.max(0)),
            members_hidden: hidden,
            next_cursor: more.then(|| self.cursor(next, hidden)).flatten(),
        }
    }
}

fn member(issuer: &Issuer<'_>, raw: RawMember) -> Member {
    let id = ChatId::User(raw.user);
    let peer = issuer.peer(id);
    Member {
        id: issuer.issue(id, ChatRef::from(id)).to_string(),
        name: peer.map_or_else(|| id.to_string(), |p| p.title.clone()),
        username: peer.and_then(|p| p.username.clone()),
        role: raw.role,
        bot: peer.is_some_and(|p| p.kind == ChatKind::Bot),
        is_self: peer.is_some_and(|p| p.is_self),
    }
}

/// Our own name filter (lowercase `query`): any part of the name or @username.
fn matches(member: &Member, query: &str) -> bool {
    member.name.to_lowercase().contains(query)
        || member
            .username
            .as_deref()
            .is_some_and(|u| u.to_lowercase().contains(query))
}

/// What a cursor must have been issued for: the Chat, the query and
/// `admins_only`. Stable across releases (FNV-1a), so cursors survive a deploy.
fn fingerprint(chat: ChatId, query: Option<&str>, admins_only: bool) -> u64 {
    let canonical = format!("members {chat}\0{}\0{admins_only}", query.unwrap_or(""));
    canonical.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Where the next page starts, and which listing it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    #[serde(rename = "f")]
    fingerprint: u64,
    #[serde(rename = "o")]
    offset: u32,
    /// The listing's first page found the members hidden.
    #[serde(rename = "h", default, skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
}

impl Cursor {
    fn invalid() -> TgError {
        TgError::InvalidInput("cursor is not one this server returned".into())
    }

    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor serializes"))
    }

    fn decode(s: &str) -> Result<Self, TgError> {
        if s.len() > MAX_MEMBER_CURSOR_LEN {
            return Err(Self::invalid());
        }
        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| Self::invalid())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| Self::invalid())?;
        if cursor.offset > MAX_OFFSET {
            return Err(Self::invalid());
        }
        Ok(cursor)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const GROUP: ChatId = ChatId::Channel(5);

    fn user(id: i64, name: &str, hash: Option<i64>) -> RawPeer {
        RawPeer {
            id: ChatId::User(id),
            title: name.into(),
            kind: ChatKind::Private,
            username: None,
            access_hash: hash,
            is_self: false,
        }
    }

    fn unknown(_: ChatId) -> Option<i64> {
        None
    }

    /// Answers every call with what it was given, and keeps the requests.
    #[derive(Default)]
    struct Canned {
        access: Option<ChannelAccess>,
        participants: ParticipantsAnswer,
        group: GroupAnswer,
        requests: Mutex<Vec<ParticipantsRequest>>,
        access_calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl MemberSource for Canned {
        async fn channel_access(&self, _: ChatRef) -> Result<ChannelAccess, TgError> {
            *self.access_calls.lock().unwrap() += 1;
            Ok(self.access.unwrap_or(ChannelAccess {
                can_list: true,
                hidden: false,
            }))
        }

        async fn participants(
            &self,
            request: ParticipantsRequest,
        ) -> Result<ParticipantsAnswer, TgError> {
            self.requests.lock().unwrap().push(request);
            Ok(self.participants.clone())
        }

        async fn group_members(&self, _: i64) -> Result<GroupAnswer, TgError> {
            Ok(self.group.clone())
        }
    }

    fn query(chat: ChatId, text: Option<&str>, admins_only: bool) -> MembersQuery {
        MembersQuery {
            chat: chat.into(),
            query: text.map(str::to_string),
            admins_only,
            limit: 2,
            cursor: None,
        }
    }

    #[tokio::test]
    async fn a_telegram_page_moves_on_by_what_telegram_sent_not_what_is_kept() {
        // Three participants sent, one of them banned (not a member).
        let source = Canned {
            participants: ParticipantsAnswer {
                count: 10,
                returned: 3,
                members: vec![
                    RawMember {
                        user: 1,
                        role: Role::Creator,
                    },
                    RawMember {
                        user: 2,
                        role: Role::Member,
                    },
                ],
                peers: vec![user(1, "Ann", Some(11)), user(2, "Bo", None)],
            },
            ..Canned::default()
        };
        let mut q = query(GROUP, None, false);
        q.limit = 3;
        let known = |chat: ChatId| (chat == ChatId::User(2)).then_some(22);
        let page = get_members(&q, &known, &source).await.unwrap();
        let ids: Vec<&str> = page.members.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                ChatRef::new(ChatId::User(1), Some(11)).to_string(),
                ChatRef::new(ChatId::User(2), Some(22)).to_string(),
            ]
        );
        assert_eq!(page.total, Some(10));
        let next = Cursor::decode(page.next_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(next.offset, 3);

        // The next page asks Telegram from there, without asking about access
        // again.
        q.cursor = page.next_cursor;
        get_members(&q, &known, &source).await.unwrap();
        let requests = source.requests.lock().unwrap();
        assert_eq!(requests[1].offset, 3);
        assert_eq!(requests[1].filter, ParticipantsFilter::Recent);
        assert_eq!(*source.access_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_empty_page_or_the_count_reached_ends_the_listing() {
        for (count, returned) in [(10, 0), (2, 2), (0, 0)] {
            let source = Canned {
                participants: ParticipantsAnswer {
                    count,
                    returned,
                    ..ParticipantsAnswer::default()
                },
                ..Canned::default()
            };
            let page = get_members(&query(GROUP, None, false), &unknown, &source)
                .await
                .unwrap();
            assert_eq!(page.next_cursor, None, "{count} {returned}");
        }
    }

    #[tokio::test]
    async fn hidden_members_are_said_on_every_page() {
        let source = Canned {
            access: Some(ChannelAccess {
                can_list: true,
                hidden: true,
            }),
            participants: ParticipantsAnswer {
                count: 6,
                returned: 2,
                ..ParticipantsAnswer::default()
            },
            ..Canned::default()
        };
        let mut q = query(GROUP, None, false);
        let first = get_members(&q, &unknown, &source).await.unwrap();
        assert!(first.members_hidden);
        q.cursor = first.next_cursor;
        let second = get_members(&q, &unknown, &source).await.unwrap();
        assert!(second.members_hidden);
    }

    #[tokio::test]
    async fn a_channel_without_access_is_refused_before_listing() {
        let source = Canned {
            access: Some(ChannelAccess {
                can_list: false,
                hidden: false,
            }),
            ..Canned::default()
        };
        let err = get_members(&query(GROUP, None, false), &unknown, &source)
            .await
            .unwrap_err();
        assert!(matches!(err, TgError::InvalidInput(m) if m.contains("only to its admins")));
        assert!(source.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admins_are_fetched_whole_and_filtered_here() {
        let admin = |user, role| RawMember { user, role };
        let source = Canned {
            participants: ParticipantsAnswer {
                count: 3,
                returned: 3,
                members: vec![
                    admin(1, Role::Creator),
                    admin(2, Role::Admin),
                    admin(3, Role::Admin),
                ],
                peers: vec![
                    user(1, "Anna Lee", Some(1)),
                    user(2, "Leo", Some(2)),
                    RawPeer {
                        username: Some("mod_lea".into()),
                        ..user(3, "Moderator", Some(3))
                    },
                ],
            },
            ..Canned::default()
        };
        let mut q = query(GROUP, Some(" @LE "), true);
        let first = get_members(&q, &unknown, &source).await.unwrap();
        let names: Vec<&str> = first.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Anna Lee", "Leo"]);
        assert_eq!(first.total, Some(3));
        q.cursor = first.next_cursor;
        let second = get_members(&q, &unknown, &source).await.unwrap();
        assert_eq!(second.members[0].name, "Moderator");
        assert_eq!(second.next_cursor, None);
        let requests = source.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.filter == ParticipantsFilter::Admins && r.offset == 0 && r.limit == 200),
            "{requests:?}"
        );
        assert_eq!(*source.access_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn cursors_belong_to_their_listing_and_are_checked() {
        let source = Canned::default();
        let mut q = query(GROUP, Some("ann"), false);
        let other = fingerprint(GROUP, Some("bob"), false);
        let foreign = Cursor {
            fingerprint: other,
            offset: 2,
            hidden: false,
        }
        .encode();
        let too_far = Cursor {
            fingerprint: fingerprint(GROUP, Some("ann"), false),
            offset: MAX_OFFSET + 1,
            hidden: false,
        }
        .encode();
        for (cursor, says) in [
            (foreign, CURSOR_MISMATCH),
            (too_far, "not one this server returned"),
            ("bm9wZQ".into(), "not one this server returned"),
            ("!!".into(), "not one this server returned"),
            ("A".repeat(200), "not one this server returned"),
        ] {
            q.cursor = Some(cursor.clone());
            match get_members(&q, &unknown, &source).await {
                Err(TgError::InvalidInput(m)) => assert!(m.contains(says), "{cursor}: {m}"),
                other => panic!("{cursor} was accepted: {other:?}"),
            }
        }
        // The same listing, written differently, is the same listing.
        assert_eq!(
            fingerprint(GROUP, canonical_query(Some(" @ann ")).as_deref(), false),
            fingerprint(GROUP, Some("ann"), false)
        );
    }

    #[tokio::test]
    async fn a_group_the_account_left_and_a_user_are_errors() {
        let source = Canned::default();
        let left = get_members(&query(ChatId::Chat(4), None, false), &unknown, &source).await;
        assert!(matches!(left, Err(TgError::InvalidInput(m)) if m.contains("no longer")));
        let user = get_members(&query(ChatId::User(4), None, false), &unknown, &source).await;
        assert!(matches!(user, Err(TgError::InvalidInput(m)) if m.contains("is a user")));
    }
}
