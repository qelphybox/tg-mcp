//! The duplicate guard of `send_message` (#46): the same text to the same
//! Chat within [`DUPLICATE_WINDOW`] of a successful send is not sent again,
//! the tool answers "already sent: message N" instead.
//!
//! - **The same text** is the message as Telegram gets it: its text and its
//!   formatting (entities) once the Markdown is read. `**hi**` in Markdown
//!   and `hi` in plain are different messages; `hi` in Markdown and in plain
//!   are the same one, as the Chat shows them.
//! - **Per Account**, not per Grant: the Chat sees the Account write twice
//!   whichever app asked, as the Send cap counts the Account's messages.
//! - **Only a hash** is kept, in memory: a SHA-256 of the text under a random
//!   key of this process, so not even a short text can be guessed back from
//!   it. Never the text, never logged; a restart forgets every send.
//!
//! A send claims its key before it goes to Telegram, so two identical calls
//! at once send one, and is recorded when Telegram took it; a send Telegram
//! refuses lets the key go. One guard per server, shared by its MCP sessions.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::tg::{chats::ChatId, markdown::FormattedText};

/// How long after a send the same text to the same Chat is a duplicate.
pub const DUPLICATE_WINDOW: Duration = Duration::from_secs(60);

/// One message as the guard knows it: the Account, the Chat and the hash of
/// its text. Holds no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SendKey {
    account_id: i64,
    chat: ChatId,
    text: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
enum Recent {
    /// A send of it is with Telegram right now.
    Sending,
    /// Telegram took it as message `id` at `at`.
    Sent { id: i32, at: Instant },
}

/// Why a message is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Duplicate {
    #[error(
        "already sent: message {id} in {chat} has this very text, sent {ago} s ago; it is not \
         sent twice. get_messages shows it. Send the same text again only if the user asks for \
         it twice, after {left} s"
    )]
    Sent {
        chat: ChatId,
        id: i32,
        ago: u64,
        left: u64,
    },
    #[error(
        "already being sent: another call is sending this very text to {chat} right now; it is \
         not sent twice. get_messages shows it in a moment"
    )]
    Sending { chat: ChatId },
}

/// The messages sent in the last [`DUPLICATE_WINDOW`], as hashes.
pub struct RecentSends {
    /// This process's key for the hashes.
    salt: [u8; 32],
    entries: Mutex<HashMap<SendKey, Recent>>,
}

impl Default for RecentSends {
    fn default() -> Self {
        Self {
            salt: crate::sessions::random_bytes::<32>(),
            entries: Mutex::default(),
        }
    }
}

impl SendKey {
    /// The Chat it went to.
    pub fn chat(&self) -> ChatId {
        self.chat
    }
}

impl RecentSends {
    /// The key of `text` sent by `account_id` to `chat`.
    pub fn key(&self, account_id: i64, chat: ChatId, text: &FormattedText) -> SendKey {
        let mut hash = Sha256::new();
        hash.update(self.salt);
        hash.update(serde_json::to_vec(text).expect("formatted text serializes"));
        SendKey {
            account_id,
            chat,
            text: hash.finalize().into(),
        }
    }

    /// Lock the entries, forgetting sends older than the window.
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<SendKey, Recent>> {
        let mut entries = self.entries.lock().expect("recent sends lock");
        let now = Instant::now();
        entries.retain(|_, recent| match recent {
            Recent::Sending => true,
            Recent::Sent { at, .. } => now.duration_since(*at) < DUPLICATE_WINDOW,
        });
        entries
    }

    /// Err when this message was sent, or is being sent, within the window.
    pub fn check(&self, key: &SendKey) -> Result<(), Duplicate> {
        match self.entries().get(key) {
            None => Ok(()),
            Some(recent) => Err(duplicate(key, *recent)),
        }
    }

    /// Like [`Self::check`], and holds the key until the send is done: an
    /// identical send meanwhile is a duplicate. Dropping the claim without
    /// [`Claim::sent`] (the send failed) lets it go.
    pub fn claim(self: &Arc<Self>, key: SendKey) -> Result<Claim, Duplicate> {
        let mut entries = self.entries();
        if let Some(recent) = entries.get(&key) {
            return Err(duplicate(&key, *recent));
        }
        entries.insert(key, Recent::Sending);
        Ok(Claim {
            recent: self.clone(),
            key,
            done: false,
        })
    }

    /// Telegram took this message as message `id`, now.
    pub fn sent(&self, key: &SendKey, id: i32) {
        self.entries().insert(
            *key,
            Recent::Sent {
                id,
                at: Instant::now(),
            },
        );
    }

    /// Messages remembered (sends of the last minute, and those under way).
    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn duplicate(key: &SendKey, recent: Recent) -> Duplicate {
    match recent {
        Recent::Sending => Duplicate::Sending { chat: key.chat },
        Recent::Sent { id, at } => {
            let ago = at.elapsed();
            Duplicate::Sent {
                chat: key.chat,
                id,
                ago: ago.as_secs(),
                left: DUPLICATE_WINDOW.saturating_sub(ago).as_secs().max(1),
            }
        }
    }
}

/// A key held while its send is with Telegram.
pub struct Claim {
    recent: Arc<RecentSends>,
    key: SendKey,
    done: bool,
}

impl Claim {
    /// Telegram took it as message `id`: the window starts now.
    pub fn sent(mut self, id: i32) {
        self.recent.sent(&self.key, id);
        self.done = true;
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut entries = self.recent.entries();
        if matches!(entries.get(&self.key), Some(Recent::Sending)) {
            entries.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::markdown::{TextFormat, markdown};

    const ALICE: ChatId = ChatId::User(101);
    const BOB: ChatId = ChatId::User(102);

    fn recent() -> Arc<RecentSends> {
        Arc::new(RecentSends::default())
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_text_to_the_same_chat_within_a_minute_is_already_sent() {
        let recent = recent();
        let key = recent.key(1, ALICE, &markdown("**hi** there"));
        assert_eq!(recent.check(&key), Ok(()));
        recent.sent(&key, 501);

        tokio::time::advance(Duration::from_secs(20)).await;
        let err = recent.check(&key).unwrap_err();
        assert_eq!(
            err,
            Duplicate::Sent {
                chat: ALICE,
                id: 501,
                ago: 20,
                left: 40
            }
        );
        let said = err.to_string();
        assert!(said.starts_with("already sent: message 501"), "{said}");
        assert!(said.contains("after 40 s"), "{said}");

        tokio::time::advance(Duration::from_secs(40)).await;
        assert_eq!(recent.check(&key), Ok(()), "a minute later it may go again");
        assert!(recent.is_empty(), "and it is forgotten");
    }

    #[tokio::test]
    async fn another_text_chat_or_account_is_another_message() {
        let recent = recent();
        let hi = markdown("**hi** there");
        recent.sent(&recent.key(1, ALICE, &hi), 501);
        assert!(recent.check(&recent.key(1, ALICE, &hi)).is_err());
        assert!(recent.check(&recent.key(1, BOB, &hi)).is_ok());
        assert!(recent.check(&recent.key(2, ALICE, &hi)).is_ok());
        for other in [
            markdown("**hi** there!"),
            // The same characters, without the bold: not the same message.
            FormattedText::new("**hi** there", TextFormat::Plain),
            markdown("hi there"),
        ] {
            assert!(recent.check(&recent.key(1, ALICE, &other)).is_ok());
        }
        // What the Chat would show, not how it was written: plain or Markdown alike.
        let plain = FormattedText::new("see you", TextFormat::Plain);
        recent.sent(&recent.key(1, ALICE, &plain), 502);
        assert!(
            recent
                .check(&recent.key(1, ALICE, &markdown("see you")))
                .is_err()
        );
    }

    #[tokio::test]
    async fn only_a_keyed_hash_is_kept() {
        let (a, b) = (recent(), recent());
        let text = markdown("my secret plan");
        let key = a.key(1, ALICE, &text);
        assert_ne!(key, b.key(1, ALICE, &text), "each process has its own key");
        let unsalted: [u8; 32] = Sha256::digest(serde_json::to_vec(&text).unwrap()).into();
        assert_ne!(key.text, unsalted);
        assert!(!format!("{key:?}").contains("secret"));
    }

    #[tokio::test]
    async fn a_claim_holds_the_key_until_sent_and_lets_go_if_it_fails() {
        let recent = recent();
        let key = recent.key(1, ALICE, &markdown("once"));
        let claim = recent.claim(key).unwrap();
        assert_eq!(
            recent.claim(key).err(),
            Some(Duplicate::Sending { chat: ALICE })
        );
        assert!(recent.check(&key).is_err());
        drop(claim);
        assert!(recent.check(&key).is_ok(), "a failed send holds nothing");

        let claim = recent.claim(key).unwrap();
        claim.sent(503);
        assert!(matches!(
            recent.claim(key).err(),
            Some(Duplicate::Sent { id: 503, .. })
        ));
    }
}
