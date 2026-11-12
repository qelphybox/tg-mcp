//! The Send allow-list (ADR-0005): the Chats a Grant with the `send` Scope may
//! write to (send, react, mark as read), chosen on the Consent screen, and
//! whether it may send there without asking the user each time. It never
//! changes: other Chats mean a new Grant.
//!
//! The vault seals it with the Grant's Session key ([`crate::sessions`]), so
//! only a Token opens it; it is in memory only while a request holds the
//! opened Grant. Chat ids are as sensitive as the Chats' names: logs carry
//! only how many there are.
//!
//! Sealed with it is the verifier of the Confirmation cookie Consent set in
//! its browser (#45, [`crate::confirm`]): the only browser where a pending
//! send can be confirmed. Inside the seal, a database writer can't swap in
//! a cookie of their own.

use std::fmt;

use zeroize::Zeroizing;

use crate::{
    scope::{Scope, Scopes},
    tg::chats::ChatId,
};

/// The most Chats one list holds: a hundred recent Chats, a few Chat folders
/// (up to 200 Chats each with Premium) and some found by name.
pub const MAX_CHATS: usize = 1000;

/// Format of the sealed plaintext: `VERSION`, flags, the Chat count (u32 LE),
/// with [`FLAG_CONFIRM_COOKIE`] the Confirmation cookie's 32-byte verifier,
/// then per Chat its kind (1 byte) and id (i64 LE), then zeros up to a
/// multiple of [`PAD_TO`] bytes. Version 1 (#44) had no verifier and no such
/// flag; it still opens, as a list no browser can confirm sends for.
const VERSION: u8 = 2;
const VERSION_WITHOUT_COOKIE: u8 = 1;
const FLAG_AUTO_SEND: u8 = 1;
const FLAG_CONFIRM_COOKIE: u8 = 2;
const HEADER_LEN: usize = 6;
const VERIFIER_LEN: usize = 32;
const ENTRY_LEN: usize = 9;
/// The plaintext is padded to a multiple of this, so the ciphertext's length
/// tells the list's size only to within 56 Chats.
pub const PAD_TO: usize = 512;

/// Chats a Grant may write to, each once, and whether it may send there
/// without the user confirming each message. `Debug` shows only the count.
#[derive(Clone, PartialEq, Eq)]
pub struct SendAllowList {
    /// Sorted, each once.
    chats: Vec<ChatId>,
    auto_send: bool,
    /// What checks the Confirmation cookie of the Consent browser
    /// ([`crate::confirm::cookie`]); never the cookie itself.
    confirm_cookie: Option<[u8; VERIFIER_LEN]>,
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
    pub fn new(
        chats: impl IntoIterator<Item = ChatId>,
        auto_send: bool,
    ) -> Result<Self, AllowListError> {
        let mut chats: Vec<ChatId> = chats.into_iter().collect();
        chats.sort_by_key(|chat| key(*chat));
        chats.dedup();
        match chats.len() {
            0 => Err(AllowListError::Empty),
            n if n > MAX_CHATS => Err(AllowListError::TooMany(n)),
            _ => Ok(Self {
                chats,
                auto_send,
                confirm_cookie: None,
            }),
        }
    }

    pub fn contains(&self, chat: ChatId) -> bool {
        self.chats
            .binary_search_by_key(&key(chat), |c| key(*c))
            .is_ok()
    }

    /// The user chose, on Consent, to let messages to these Chats go without
    /// confirming each one. Off unless they ticked it.
    pub fn auto_send(&self) -> bool {
        self.auto_send
    }

    /// The list with the verifier of the Confirmation cookie Consent set in
    /// its browser: sends are confirmed there, and nowhere else.
    pub fn with_confirm_cookie(mut self, verifier: [u8; VERIFIER_LEN]) -> Self {
        self.confirm_cookie = Some(verifier);
        self
    }

    /// The verifier of the Confirmation cookie, if Consent set one (every
    /// Grant since #45 has one).
    pub fn confirm_cookie(&self) -> Option<&[u8; VERIFIER_LEN]> {
        self.confirm_cookie.as_ref()
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
        let verifier = if self.confirm_cookie.is_some() {
            VERIFIER_LEN
        } else {
            0
        };
        let body = HEADER_LEN + verifier + self.chats.len() * ENTRY_LEN;
        let mut out = Zeroizing::new(Vec::with_capacity(body.next_multiple_of(PAD_TO)));
        out.push(VERSION);
        let mut flags = 0;
        if self.auto_send {
            flags |= FLAG_AUTO_SEND;
        }
        if self.confirm_cookie.is_some() {
            flags |= FLAG_CONFIRM_COOKIE;
        }
        out.push(flags);
        let count = u32::try_from(self.chats.len()).expect("at most MAX_CHATS");
        out.extend_from_slice(&count.to_le_bytes());
        if let Some(verifier) = &self.confirm_cookie {
            out.extend_from_slice(verifier);
        }
        for chat in &self.chats {
            let (kind, id) = key(*chat);
            out.push(kind);
            out.extend_from_slice(&id.to_le_bytes());
        }
        out.resize(body.next_multiple_of(PAD_TO), 0);
        out
    }

    /// Reverse of [`Self::encode`]; `None` for anything it can't have written.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (&[version, flags], rest) = bytes.split_first_chunk::<2>()?;
        let known_flags = match version {
            VERSION => FLAG_AUTO_SEND | FLAG_CONFIRM_COOKIE,
            VERSION_WITHOUT_COOKIE => FLAG_AUTO_SEND,
            _ => return None,
        };
        if flags & !known_flags != 0 || !bytes.len().is_multiple_of(PAD_TO) {
            return None;
        }
        let (count, mut rest) = rest.split_first_chunk::<4>()?;
        let count = usize::try_from(u32::from_le_bytes(*count)).ok()?;
        let mut confirm_cookie = None;
        if flags & FLAG_CONFIRM_COOKIE != 0 {
            let (verifier, after) = rest.split_first_chunk::<VERIFIER_LEN>()?;
            confirm_cookie = Some(*verifier);
            rest = after;
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
        let mut list = Self::new(chats, flags & FLAG_AUTO_SEND != 0).ok()?;
        list.confirm_cookie = confirm_cookie;
        (list.len() == count).then_some(list)
    }
}

impl fmt::Debug for SendAllowList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendAllowList")
            .field("chats", &format_args!("<{}>", self.chats.len()))
            .field("auto_send", &self.auto_send)
            .field("confirm_cookie", &self.confirm_cookie.is_some())
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
/// Grant with `send` but no list may write nowhere. Whether the user must
/// confirm a send is [`SendAllowList::auto_send`]'s business, not this.
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
        let list = SendAllowList::new([TEAM, ALICE, TEAM, FAMILY], false).unwrap();
        assert_eq!(list.len(), 3);
        for chat in [ALICE, FAMILY, TEAM] {
            assert!(list.contains(chat), "{chat}");
        }
        // Same number, other kind: another Chat.
        assert!(!list.contains(ChatId::Channel(7)));
        assert!(!list.contains(ChatId::User(9)));
        assert!(!list.auto_send());

        assert_eq!(SendAllowList::new([], true), Err(AllowListError::Empty));
        let many = (1..=MAX_CHATS as i64 + 1).map(ChatId::User);
        assert_eq!(
            SendAllowList::new(many, false),
            Err(AllowListError::TooMany(MAX_CHATS + 1))
        );
        let most = (1..=MAX_CHATS as i64).map(ChatId::User);
        assert_eq!(SendAllowList::new(most, false).unwrap().len(), MAX_CHATS);
    }

    #[test]
    fn the_helper_allows_only_listed_chats_and_only_with_send() {
        let list = SendAllowList::new([ALICE, TEAM], false).unwrap();
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
        for (chats, auto_send) in [
            (vec![ALICE], false),
            (vec![ALICE, FAMILY, TEAM], true),
            ((1..=MAX_CHATS as i64).map(ChatId::Channel).collect(), true),
        ] {
            let list = SendAllowList::new(chats, auto_send).unwrap();
            let encoded = list.encode();
            assert_eq!(encoded.len() % PAD_TO, 0);
            assert_eq!(SendAllowList::decode(&encoded), Some(list));
        }
        // One Chat or fifty: the same length.
        let one = SendAllowList::new([ALICE], false).unwrap().encode();
        let fifty = SendAllowList::new((1..=50).map(ChatId::User), false)
            .unwrap()
            .encode();
        assert_eq!(one.len(), PAD_TO);
        assert_eq!(fifty.len(), PAD_TO);
    }

    #[test]
    fn anything_else_does_not_decode() {
        // ALICE (kind 1, id 7) at bytes 6..15, TEAM (kind 3, id 9) at 15..24.
        let good = SendAllowList::new([ALICE, TEAM], true).unwrap().encode();
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
            (changed(0, 3), "unknown version"),
            (changed(0, 0), "version 0"),
            (changed(1, 4), "unknown flag"),
            (changed(1, 3), "a verifier the bytes don't have"),
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

    #[test]
    fn the_confirmation_cookies_verifier_is_sealed_with_the_list() {
        let verifier = [0xA5; 32];
        let list = SendAllowList::new([ALICE, TEAM], false)
            .unwrap()
            .with_confirm_cookie(verifier);
        assert_eq!(list.confirm_cookie(), Some(&verifier));
        let encoded = list.encode();
        assert_eq!(encoded.len(), PAD_TO);
        let opened = SendAllowList::decode(&encoded).unwrap();
        assert_eq!(opened.confirm_cookie(), Some(&verifier));
        assert_eq!(opened, list);
        assert!(opened.contains(TEAM) && !opened.auto_send());
        assert_eq!(
            SendAllowList::new([ALICE], false).unwrap().confirm_cookie(),
            None
        );
    }

    #[test]
    fn a_list_sealed_before_confirmation_cookies_opens_without_one() {
        // Version 1 (#44): ALICE with auto-send, no verifier.
        let mut v1 = vec![1u8, 1, 1, 0, 0, 0, 1];
        v1.extend_from_slice(&7i64.to_le_bytes());
        v1.resize(PAD_TO, 0);
        let list = SendAllowList::decode(&v1).unwrap();
        assert!(list.contains(ALICE) && list.auto_send());
        assert_eq!(list.confirm_cookie(), None);
        v1[1] = 3;
        assert_eq!(SendAllowList::decode(&v1), None, "v1 had no such flag");
    }

    #[test]
    fn debug_shows_only_the_count() {
        let list = SendAllowList::new([ALICE, TEAM], true).unwrap();
        let list = list.with_confirm_cookie([0x37; 32]);
        let shown = format!("{list:?}");
        assert!(shown.contains("<2>"), "{shown}");
        assert!(shown.contains("confirm_cookie: true"), "{shown}");
        assert!(!shown.contains('7') && !shown.contains('9'), "{shown}");
    }
}
