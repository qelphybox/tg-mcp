//! The Send allow-list (ADR-0005): the Chats a Grant with the `send` Scope may
//! write to (send, react, mark as read), chosen on the Consent screen. It
//! never changes: other Chats mean a new Grant. Writes into it go out at once
//! (ADR-0008).
//!
//! The vault seals it with the Grant's Session key ([`crate::sessions`]), so
//! only a Token opens it; it is in memory only while a request holds the
//! opened Grant. Chat ids are as sensitive as the Chats' names: logs carry
//! only how many there are.

use std::fmt;

use zeroize::Zeroizing;

use crate::{
    scope::{Scope, Scopes},
    tg::chats::ChatId,
};

/// The most Chats one list holds: a hundred recent Chats, a few Chat folders
/// (up to 200 Chats each with Premium) and some found by name.
pub const MAX_CHATS: usize = 1000;

/// Format of the sealed plaintext: `VERSION`, a flags byte (0), the Chat
/// count (u32 LE), then per Chat its kind (1 byte) and id (i64 LE), then zeros
/// up to a multiple of [`PAD_TO`] bytes.
///
/// Lists sealed before 0.3.0 still open, with what they held besides their
/// Chats ignored (ADR-0008): version 1 (#44) with the Auto-send flag, version
/// 2 (#45) also with the Confirmation cookie's 32-byte verifier after the
/// count when its flag is set.
const VERSION: u8 = 3;
const V1_AUTO_SEND: u8 = 1;
const V2_CONFIRM_COOKIE: u8 = 2;
const HEADER_LEN: usize = 6;
const V2_VERIFIER_LEN: usize = 32;
const ENTRY_LEN: usize = 9;
/// The plaintext is padded to a multiple of this, so the ciphertext's length
/// tells the list's size only to within 56 Chats.
pub const PAD_TO: usize = 512;

/// Chats a Grant may write to, each once. `Debug` shows only the count.
#[derive(Clone, PartialEq, Eq)]
pub struct SendAllowList {
    /// Sorted, each once.
    chats: Vec<ChatId>,
}

/// A list Consent can't make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AllowListError {
    #[error("no chat was chosen")]
    Empty,
    #[error("{0} chats were chosen, at most {MAX_CHATS} can be")]
    TooMany(usize),
}

impl SendAllowList {
    /// The list of `chats` (repeats count once). Never empty: `send` without a
    /// Chat to send to is refused on Consent.
    pub fn new(chats: impl IntoIterator<Item = ChatId>) -> Result<Self, AllowListError> {
        let mut chats: Vec<ChatId> = chats.into_iter().collect();
        chats.sort_by_key(|chat| key(*chat));
        chats.dedup();
        match chats.len() {
            0 => Err(AllowListError::Empty),
            n if n > MAX_CHATS => Err(AllowListError::TooMany(n)),
            _ => Ok(Self { chats }),
        }
    }

    pub fn contains(&self, chat: ChatId) -> bool {
        self.chats
            .binary_search_by_key(&key(chat), |c| key(*c))
            .is_ok()
    }

    pub fn len(&self) -> usize {
        self.chats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    /// The Chats, in no meaningful order.
    pub fn chats(&self) -> impl Iterator<Item = ChatId> + '_ {
        self.chats.iter().copied()
    }

    /// The plaintext the vault seals. Wiped on drop.
    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        let body = HEADER_LEN + self.chats.len() * ENTRY_LEN;
        let mut out = Zeroizing::new(Vec::with_capacity(body.next_multiple_of(PAD_TO)));
        out.push(VERSION);
        out.push(0);
        let count = u32::try_from(self.chats.len()).expect("at most MAX_CHATS");
        out.extend_from_slice(&count.to_le_bytes());
        for chat in &self.chats {
            let (kind, id) = key(*chat);
            out.push(kind);
            out.extend_from_slice(&id.to_le_bytes());
        }
        out.resize(body.next_multiple_of(PAD_TO), 0);
        out
    }

    /// Reverse of [`Self::encode`], and of the versions before it; `None`
    /// for anything none of them can have written.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (&[version, flags], rest) = bytes.split_first_chunk::<2>()?;
        let known_flags = match version {
            VERSION => 0,
            2 => V1_AUTO_SEND | V2_CONFIRM_COOKIE,
            1 => V1_AUTO_SEND,
            _ => return None,
        };
        if flags & !known_flags != 0 || !bytes.len().is_multiple_of(PAD_TO) {
            return None;
        }
        let (count, mut rest) = rest.split_first_chunk::<4>()?;
        let count = usize::try_from(u32::from_le_bytes(*count)).ok()?;
        if flags & V2_CONFIRM_COOKIE != 0 {
            // A cookie no page reads any more.
            rest = rest.get(V2_VERIFIER_LEN..)?;
        }
        if count > MAX_CHATS || count * ENTRY_LEN > rest.len() {
            return None;
        }
        let mut chats = Vec::with_capacity(count);
        for _ in 0..count {
            let (&[kind], after) = rest.split_first_chunk::<1>()?;
            let (id, after) = after.split_first_chunk::<8>()?;
            chats.push(chat_of(kind, i64::from_le_bytes(*id))?);
            rest = after;
        }
        if rest.iter().any(|&b| b != 0) {
            return None;
        }
        let list = Self::new(chats).ok()?;
        (list.len() == count).then_some(list)
    }
}

impl fmt::Debug for SendAllowList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendAllowList")
            .field("chats", &format_args!("<{}>", self.chats.len()))
            .finish()
    }
}

fn key(chat: ChatId) -> (u8, i64) {
    match chat {
        ChatId::User(id) => (1, id),
        ChatId::Chat(id) => (2, id),
        ChatId::Channel(id) => (3, id),
    }
}

fn chat_of(kind: u8, id: i64) -> Option<ChatId> {
    if id <= 0 {
        return None;
    }
    match kind {
        1 => Some(ChatId::User(id)),
        2 => Some(ChatId::Chat(id)),
        3 => Some(ChatId::Channel(id)),
        _ => None,
    }
}

/// Why a Grant may not write to a Chat. The messages are for the model, and
/// name the rule; the Chat in them is the caller's own input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefused {
    #[error(
        "this connection was not granted the `send` Scope, so it can't write to chats: the \
         user connects again and allows sending on the consent screen"
    )]
    NoSendScope,
    #[error(
        "{0} is not one of the chats this connection may send to (its Send allow-list, chosen \
         on the consent screen when it was connected): the user connects again to choose \
         other chats"
    )]
    NotInAllowList(ChatId),
}

/// May a Grant with `scopes` and `list` write to `chat` (send, react, mark as
/// read)? Only with the `send` Scope and only into its Send allow-list. A
/// Grant with `send` but no list may write nowhere.
pub fn may_write(
    scopes: Scopes,
    list: Option<&SendAllowList>,
    chat: ChatId,
) -> Result<(), WriteRefused> {
    if !scopes.contains(Scope::Send) {
        return Err(WriteRefused::NoSendScope);
    }
    match list {
        Some(list) if list.contains(chat) => Ok(()),
        _ => Err(WriteRefused::NotInAllowList(chat)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: ChatId = ChatId::User(7);
    const FAMILY: ChatId = ChatId::Chat(8);
    const TEAM: ChatId = ChatId::Channel(9);

    fn sending() -> Scopes {
        [Scope::Send].into_iter().collect()
    }

    #[test]
    fn a_list_holds_its_chats_once_and_never_none() {
        let list = SendAllowList::new([TEAM, ALICE, TEAM, FAMILY]).unwrap();
        assert_eq!(list.len(), 3);
        for chat in [ALICE, FAMILY, TEAM] {
            assert!(list.contains(chat), "{chat}");
        }
        // Same number, other kind: another Chat.
        assert!(!list.contains(ChatId::Channel(7)));
        assert!(!list.contains(ChatId::User(9)));

        assert_eq!(SendAllowList::new([]), Err(AllowListError::Empty));
        let many = (1..=MAX_CHATS as i64 + 1).map(ChatId::User);
        assert_eq!(
            SendAllowList::new(many),
            Err(AllowListError::TooMany(MAX_CHATS + 1))
        );
        let most = (1..=MAX_CHATS as i64).map(ChatId::User);
        assert_eq!(SendAllowList::new(most).unwrap().len(), MAX_CHATS);
    }

    #[test]
    fn the_helper_allows_only_listed_chats_and_only_with_send() {
        let list = SendAllowList::new([ALICE, TEAM]).unwrap();
        assert_eq!(may_write(sending(), Some(&list), ALICE), Ok(()));
        assert_eq!(may_write(sending(), Some(&list), TEAM), Ok(()));
        assert_eq!(
            may_write(sending(), Some(&list), FAMILY),
            Err(WriteRefused::NotInAllowList(FAMILY))
        );
        assert_eq!(
            may_write(sending(), None, ALICE),
            Err(WriteRefused::NotInAllowList(ALICE))
        );
        let drafts: Scopes = [Scope::Draft].into_iter().collect();
        for scopes in [Scopes::READ_ONLY, drafts] {
            assert_eq!(
                may_write(scopes, Some(&list), ALICE),
                Err(WriteRefused::NoSendScope)
            );
        }
        let refused = may_write(sending(), Some(&list), FAMILY)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("chat:8 is not one of the chats"),
            "{refused}"
        );
        assert!(refused.contains("Send allow-list"), "{refused}");
    }

    #[test]
    fn the_encoding_round_trips_and_is_padded() {
        for chats in [
            vec![ALICE],
            vec![ALICE, FAMILY, TEAM],
            (1..=MAX_CHATS as i64).map(ChatId::Channel).collect(),
        ] {
            let list = SendAllowList::new(chats).unwrap();
            let encoded = list.encode();
            assert_eq!(encoded.len() % PAD_TO, 0);
            assert_eq!(SendAllowList::decode(&encoded), Some(list));
        }
        // One Chat or fifty: the same length.
        let one = SendAllowList::new([ALICE]).unwrap().encode();
        let fifty = SendAllowList::new((1..=50).map(ChatId::User))
            .unwrap()
            .encode();
        assert_eq!(one.len(), PAD_TO);
        assert_eq!(fifty.len(), PAD_TO);
    }

    #[test]
    fn anything_else_does_not_decode() {
        // ALICE (kind 1, id 7) at bytes 6..15, TEAM (kind 3, id 9) at 15..24.
        let good = SendAllowList::new([ALICE, TEAM]).unwrap().encode();
        let changed = |at: usize, value: u8| {
            let mut bytes = good.to_vec();
            bytes[at] = value;
            bytes
        };
        let mut repeated = good.to_vec();
        repeated.copy_within(6..15, 15);
        for (bad, why) in [
            (Vec::new(), "empty"),
            (good[..PAD_TO - 1].to_vec(), "not padded"),
            ([good.to_vec(), vec![0]].concat(), "padded wrong"),
            (changed(0, 4), "unknown version"),
            (changed(0, 0), "version 0"),
            (changed(1, 1), "a flag, since version 3"),
            (changed(1, 4), "unknown flag"),
            (changed(2, 0), "no chats"),
            (changed(2, 3), "more chats than entries"),
            (changed(6, 4), "unknown kind"),
            (changed(7, 0), "id 0"),
            (changed(24, 1), "padding not zero"),
            (repeated, "a chat twice"),
        ] {
            assert_eq!(SendAllowList::decode(&bad), None, "{why}");
        }
        assert!(SendAllowList::decode(&good).is_some());
    }

    /// A list sealed by an earlier version: `version`, `flags`, the count,
    /// `verifier`, then the Chats, padded.
    fn sealed_before(version: u8, flags: u8, verifier: &[u8], chats: &[(u8, i64)]) -> Vec<u8> {
        let mut bytes = vec![version, flags];
        bytes.extend_from_slice(&(chats.len() as u32).to_le_bytes());
        bytes.extend_from_slice(verifier);
        for (kind, id) in chats {
            bytes.push(*kind);
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes.resize(bytes.len().next_multiple_of(PAD_TO), 0);
        bytes
    }

    #[test]
    fn lists_sealed_before_0_3_0_open_with_their_chats_and_nothing_else() {
        // Version 1 (#44): ALICE with auto-send. Version 2 (#45): the
        // Confirmation cookie's verifier too, with or without auto-send.
        // Both are ignored now (ADR-0008): every Grant sends at once.
        let v1 = sealed_before(1, 1, &[], &[(1, 7)]);
        let v2 = sealed_before(2, 2, &[0xA5; 32], &[(1, 7), (3, 9)]);
        let v2_auto = sealed_before(2, 3, &[0xA5; 32], &[(1, 7), (3, 9)]);
        assert_eq!(
            SendAllowList::decode(&v1),
            Some(SendAllowList::new([ALICE]).unwrap())
        );
        for v2 in [v2, v2_auto] {
            assert_eq!(
                SendAllowList::decode(&v2),
                Some(SendAllowList::new([ALICE, TEAM]).unwrap())
            );
        }
        for (bad, why) in [
            (sealed_before(1, 3, &[], &[(1, 7)]), "v1 had no verifier"),
            (sealed_before(1, 4, &[], &[(1, 7)]), "unknown v1 flag"),
            (sealed_before(2, 6, &[0; 32], &[(1, 7)]), "unknown v2 flag"),
            (
                sealed_before(2, 2, &[], &[(1, 7)]),
                "v2 without its verifier",
            ),
        ] {
            assert_eq!(SendAllowList::decode(&bad), None, "{why}");
        }
        // Sealed again, it is today's version, with no flags.
        let resealed = SendAllowList::decode(&v1).unwrap().encode();
        assert_eq!(&resealed[..2], &[3, 0]);
    }

    #[test]
    fn debug_shows_only_the_count() {
        let list = SendAllowList::new([ALICE, TEAM]).unwrap();
        let shown = format!("{list:?}");
        assert!(shown.contains("<2>"), "{shown}");
        assert!(!shown.contains('7') && !shown.contains('9'), "{shown}");
    }
}
