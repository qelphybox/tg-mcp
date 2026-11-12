//! Sent messages as the server sees them, independent of grammers/TL (#45,
//! for `send_message` in #46).
//!
//! A message to send is a [`NewMessage`]: its text already formatted and
//! checked ([`super::compose::prepare`]), placed by
//! [`super::compose::place`] like a Draft, and carrying the `random_id`
//! Telegram deduplicates sends by. Whoever makes a message keeps that id for
//! as long as it may be sent again (a pending send keeps one for its whole
//! life), so a retry or a double click can't send it twice: Telegram refuses
//! the repeat with `RANDOM_ID_DUPLICATE` ([`already_sent`]).

use super::{TgError, chats::ChatId, chats::ChatRef, markdown::FormattedText};

/// A message to send, as `messages.sendMessage` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub chat: ChatRef,
    /// Never empty: a message has text.
    pub text: FormattedText,
    pub reply_to: Option<i32>,
    pub topic: Option<i32>,
    /// Telegram's `random_id`: the same id is never sent twice.
    pub random_id: i64,
}

impl NewMessage {
    /// A message with a fresh `random_id`.
    pub fn new(
        chat: ChatRef,
        text: FormattedText,
        reply_to: Option<i32>,
        topic: Option<i32>,
    ) -> Self {
        Self {
            chat,
            text,
            reply_to,
            topic,
            random_id: random_id(),
        }
    }
}

/// A random, non-zero `random_id`.
pub fn random_id() -> i64 {
    loop {
        let id = i64::from_le_bytes(crate::sessions::random_bytes::<8>());
        if id != 0 {
            return id;
        }
    }
}

/// A message Telegram took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentMessage {
    pub chat: ChatId,
    /// The new message's id in its Chat.
    pub id: i32,
    /// When Telegram dated it, unix seconds.
    pub date: i32,
    /// Only in a forum: the Topic it went to (General is 1).
    pub topic: Option<i32>,
    pub reply_to: Option<i32>,
}

/// What Telegram's answer to `messages.sendMessage` says of the new message:
/// its id and date, from the `updateMessageID` that carries our `random_id`
/// and the new message itself, or from `updateShortSentMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendAnswer {
    /// `(random_id, id)` of each `updateMessageID`.
    pub ids: Option<(i64, i32)>,
    /// `(id, date)` of the new message, when the answer has it.
    pub message: Option<(i32, i32)>,
}

impl SendAnswer {
    /// The id and date of the message sent with `random_id`. Without a date
    /// in the answer, `now`.
    pub fn sent(&self, random_id: i64, now: i32) -> Result<(i32, i32), TgError> {
        let id = match (self.ids, self.message) {
            (Some((random, id)), _) if random == random_id => id,
            (None, Some((id, _))) => id,
            _ => {
                return Err(TgError::Upstream(
                    "telegram's answer to sendMessage names no new message".into(),
                ));
            }
        };
        let date = self
            .message
            .filter(|(message, _)| *message == id)
            .map_or(now, |(_, date)| date);
        Ok((id, date))
    }
}

/// What a send whose `random_id` Telegram already has says: it went out
/// before (a retry after a lost answer), and isn't sent again.
pub fn already_sent() -> TgError {
    TgError::InvalidInput(
        "Telegram already has this message (RANDOM_ID_DUPLICATE): it was sent before and is \
         not sent again"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_are_never_zero_and_differ() {
        let ids: std::collections::HashSet<i64> = (0..100).map(|_| random_id()).collect();
        assert_eq!(ids.len(), 100);
        assert!(!ids.contains(&0));
    }

    #[test]
    fn the_new_message_is_the_one_with_our_random_id() {
        let answer = SendAnswer {
            ids: Some((7, 501)),
            message: Some((501, 1_700_000_000)),
        };
        assert_eq!(answer.sent(7, 1).unwrap(), (501, 1_700_000_000));
        assert!(answer.sent(8, 1).is_err(), "another send's id");

        // A short answer has the message, not the random id.
        let short = SendAnswer {
            ids: None,
            message: Some((502, 1_700_000_001)),
        };
        assert_eq!(short.sent(7, 1).unwrap(), (502, 1_700_000_001));

        // No date in the answer: now.
        let bare = SendAnswer {
            ids: Some((7, 503)),
            message: None,
        };
        assert_eq!(bare.sent(7, 42).unwrap(), (503, 42));
        assert!(SendAnswer::default().sent(7, 1).is_err());
    }
}
