//! What every tool that writes text into a Chat checks before Telegram sees
//! it (#43): `save_draft` and `send_message` (#46). The text in its format
//! ([`prepare`] for a Draft, [`prepare_message`] for a message) and where in
//! the Chat it goes ([`place`]): a forum needs a Topic, given as `topic` or
//! by replying to a message in one.

use std::future::Future;

use super::{
    TgError,
    chats::ChatId,
    markdown::{FormattedText, TextFormat},
    topics::{self, GENERAL_TOPIC_ID},
};

/// Telegram's longest message, in characters, after formatting is applied.
pub const MAX_TEXT_CHARS: usize = 4096;
/// The longest `text` a tool takes, markup included: room for links' URLs
/// and markers around [`MAX_TEXT_CHARS`] of text, never a reason to parse
/// megabytes.
pub const MAX_INPUT_CHARS: usize = 16_384;

/// `text` read as `format`, checked: at most [`MAX_TEXT_CHARS`] once
/// formatted (never split), and not only whitespace. Empty stays empty (a
/// Draft's way to clear).
pub fn prepare(text: &str, format: TextFormat) -> Result<FormattedText, TgError> {
    let formatted = formatted(text, format)?;
    if !text.is_empty() && formatted.text.trim().is_empty() {
        return Err(TgError::InvalidInput(
            "text is only whitespace: write some text, or pass \"\" to clear".into(),
        ));
    }
    Ok(formatted)
}

/// A message's text: as [`prepare`], but never empty or only whitespace
/// once formatted, as a message (unlike a Draft) has nothing to clear.
pub fn prepare_message(text: &str, format: TextFormat) -> Result<FormattedText, TgError> {
    let formatted = formatted(text, format)?;
    if formatted.text.trim().is_empty() {
        return Err(TgError::InvalidInput(
            "text is empty once formatted: a message needs some text".into(),
        ));
    }
    Ok(formatted)
}

/// `text` read as `format`, within the limits.
fn formatted(text: &str, format: TextFormat) -> Result<FormattedText, TgError> {
    let given = text.chars().count();
    if given > MAX_INPUT_CHARS {
        return Err(TgError::InvalidInput(format!(
            "text is {given} characters; at most {MAX_INPUT_CHARS} are taken, and at most \
             {MAX_TEXT_CHARS} once formatted"
        )));
    }
    let formatted = FormattedText::new(text, format);
    let chars = formatted.text.chars().count();
    if chars > MAX_TEXT_CHARS {
        return Err(TgError::InvalidInput(too_long(chars)));
    }
    Ok(formatted)
}

/// What a text over Telegram's limit says.
pub fn too_long(chars: usize) -> String {
    format!(
        "text is {chars} characters once formatted; Telegram takes at most {MAX_TEXT_CHARS}. \
         It is not split: shorten it, or write it as several parts"
    )
}

/// Where in its Chat a text goes: the message it replies to, and the Topic
/// of a forum (General included). Outside forums `topic` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub reply_to: Option<i32>,
    pub topic: Option<i32>,
}

impl Placement {
    /// What Telegram's `reply_to` (`inputReplyToMessage`) carries:
    /// `(reply_to_msg_id, top_msg_id)`. A Topic is addressed by its top
    /// message, so a text in a Topic that replies to nothing "replies" to
    /// the Topic itself, as Telegram's apps do; General has no top message,
    /// and its texts carry no Topic. `None`: no `reply_to` at all.
    pub fn reply_header(&self) -> Option<(i32, Option<i32>)> {
        let top = self.topic.filter(|&t| t != GENERAL_TOPIC_ID);
        match (self.reply_to, top) {
            (Some(message), top) => Some((message, top)),
            (None, Some(topic)) => Some((topic, Some(topic))),
            (None, None) => None,
        }
    }
}

/// What a forum without a Topic to write in says.
pub fn forum_needs_topic(chat: ChatId) -> TgError {
    TgError::InvalidInput(format!(
        "{chat} is a forum: say which Topic to write in with topic (an id from get_chat_info \
         topics; General is 1), or give reply_to, a message in that Topic"
    ))
}

/// Where a text with `reply_to` and `topic` goes in `chat`. A user or basic
/// group is never a forum; whether a channel is one, `is_forum` says. In a
/// forum, `reply_to`'s message decides the Topic (`message_topic`: `None`
/// when there is no such message in the Chat, else its
/// [`RawHistoryMessage::topic`](super::messages::RawHistoryMessage::topic));
/// without it, `topic` must be given. Elsewhere `topic` is an error and
/// `reply_to` is Telegram's to check.
pub async fn place<F, M>(
    chat: ChatId,
    reply_to: Option<i32>,
    topic: Option<i32>,
    is_forum: impl FnOnce() -> F,
    message_topic: impl FnOnce(i32) -> M,
) -> Result<Placement, TgError>
where
    F: Future<Output = Result<bool, TgError>>,
    M: Future<Output = Result<Option<Option<i32>>, TgError>>,
{
    let forum = matches!(chat, ChatId::Channel(_)) && is_forum().await?;
    if !forum {
        if topic.is_some() {
            return Err(topics::not_a_forum(chat));
        }
        return Ok(Placement {
            reply_to,
            topic: None,
        });
    }
    let Some(message) = reply_to else {
        return match topic {
            Some(topic) => Ok(Placement {
                reply_to: None,
                topic: Some(topic),
            }),
            None => Err(forum_needs_topic(chat)),
        };
    };
    let Some(raw) = message_topic(message).await? else {
        return Err(TgError::InvalidInput(format!(
            "reply_to {message} is not a message of {chat}"
        )));
    };
    let found = topics::topic_of(raw);
    if let Some(asked) = topic.filter(|&asked| asked != found) {
        return Err(TgError::InvalidInput(format!(
            "reply_to {message} is in Topic {found}, not in topic {asked}: leave out topic, or \
             reply to a message of Topic {asked}"
        )));
    }
    Ok(Placement {
        reply_to: Some(message),
        topic: Some(found),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::tg::markdown::{Entity, EntityKind};

    fn invalid(result: Result<impl std::fmt::Debug, TgError>) -> String {
        match result {
            Err(TgError::InvalidInput(msg)) => msg,
            other => panic!("expected invalid input, got {other:?}"),
        }
    }

    #[test]
    fn the_limit_counts_the_text_once_formatted() {
        let at_limit = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(
            prepare(&at_limit, TextFormat::Plain).unwrap().text,
            at_limit
        );
        // 4096 characters of text in bold: the markers don't count.
        let bold = prepare(&format!("**{at_limit}**"), TextFormat::Markdown).unwrap();
        assert_eq!(bold.text, at_limit);
        assert_eq!(
            bold.entities,
            [Entity {
                kind: EntityKind::Bold,
                offset: 0,
                length: 4096
            }]
        );
        // In plain they are text, so the same input is too long.
        let msg = invalid(prepare(&format!("**{at_limit}**"), TextFormat::Plain));
        assert!(msg.contains("4100 characters"), "{msg}");

        let msg = invalid(prepare(
            &"y".repeat(MAX_TEXT_CHARS + 1),
            TextFormat::Markdown,
        ));
        assert!(msg.contains("4097") && msg.contains("not split"), "{msg}");
        // Characters, not bytes or UTF-16 units.
        assert!(prepare(&"я".repeat(MAX_TEXT_CHARS), TextFormat::Plain).is_ok());
        assert!(prepare(&"😀".repeat(MAX_TEXT_CHARS), TextFormat::Plain).is_ok());

        let msg = invalid(prepare(
            &"z".repeat(MAX_INPUT_CHARS + 1),
            TextFormat::Markdown,
        ));
        assert!(msg.contains("16385"), "{msg}");
    }

    #[test]
    fn empty_clears_and_only_whitespace_is_refused() {
        assert_eq!(
            prepare("", TextFormat::Markdown).unwrap(),
            FormattedText::default()
        );
        for text in [" ", "\n\t "] {
            for format in [TextFormat::Markdown, TextFormat::Plain] {
                assert!(invalid(prepare(text, format)).contains("pass \"\""));
            }
        }
    }

    #[test]
    fn a_message_has_the_same_limits_and_never_no_text() {
        let at_limit = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(
            prepare_message(&format!("**{at_limit}**"), TextFormat::Markdown).unwrap(),
            prepare(&format!("**{at_limit}**"), TextFormat::Markdown).unwrap()
        );
        let msg = invalid(prepare_message(
            &"y".repeat(MAX_TEXT_CHARS + 1),
            TextFormat::Markdown,
        ));
        assert!(msg.contains("4097") && msg.contains("not split"), "{msg}");
        for text in ["", " ", "\n\t "] {
            for format in [TextFormat::Markdown, TextFormat::Plain] {
                let msg = invalid(prepare_message(text, format));
                assert!(msg.contains("a message needs some text"), "{msg}");
                assert!(!msg.contains("clear"), "{msg}");
            }
        }
    }

    async fn placed(
        chat: ChatId,
        reply_to: Option<i32>,
        topic: Option<i32>,
        forum: bool,
    ) -> (Result<Placement, TgError>, usize, usize) {
        let (asked, looked_up) = (Cell::new(0), Cell::new(0));
        // Message 5 is in General, 12 in Topic 10; others don't exist.
        let placement = place(
            chat,
            reply_to,
            topic,
            || {
                asked.set(asked.get() + 1);
                std::future::ready(Ok(forum))
            },
            |id| {
                looked_up.set(looked_up.get() + 1);
                std::future::ready(Ok(match id {
                    5 => Some(None),
                    12 => Some(Some(10)),
                    _ => None,
                }))
            },
        )
        .await;
        (placement, asked.get(), looked_up.get())
    }

    const FORUM: ChatId = ChatId::Channel(9);

    #[tokio::test]
    async fn outside_forums_nothing_is_looked_up_and_topic_is_an_error() {
        for chat in [ChatId::User(1), ChatId::Chat(2)] {
            let (placement, asked, looked_up) = placed(chat, Some(7), None, true).await;
            assert_eq!(
                placement.unwrap(),
                Placement {
                    reply_to: Some(7),
                    topic: None
                }
            );
            assert_eq!((asked, looked_up), (0, 0), "never a forum");
            assert!(invalid(placed(chat, None, Some(1), true).await.0).contains("not a forum"));
        }
        let channel = ChatId::Channel(3);
        let (placement, asked, looked_up) = placed(channel, None, None, false).await;
        assert_eq!(placement.unwrap().reply_header(), None);
        assert_eq!((asked, looked_up), (1, 0));
        assert!(invalid(placed(channel, None, Some(10), false).await.0).contains("not a forum"));
    }

    #[tokio::test]
    async fn a_forum_needs_a_topic_or_a_reply_in_one() {
        let msg = invalid(placed(FORUM, None, None, true).await.0);
        assert!(
            msg.contains("is a forum") && msg.contains("General is 1"),
            "{msg}"
        );

        let in_topic = placed(FORUM, None, Some(10), true).await.0.unwrap();
        assert_eq!(in_topic.topic, Some(10));
        assert_eq!(in_topic.reply_header(), Some((10, Some(10))));
        let in_general = placed(FORUM, None, Some(1), true).await.0.unwrap();
        assert_eq!(in_general.topic, Some(1));
        assert_eq!(
            in_general.reply_header(),
            None,
            "General has no top message"
        );

        let (reply, _, looked_up) = placed(FORUM, Some(12), None, true).await;
        let reply = reply.unwrap();
        assert_eq!((reply.reply_to, reply.topic), (Some(12), Some(10)));
        assert_eq!(reply.reply_header(), Some((12, Some(10))));
        assert_eq!(looked_up, 1);
        let general_reply = placed(FORUM, Some(5), None, true).await.0.unwrap();
        assert_eq!(general_reply.topic, Some(1));
        assert_eq!(general_reply.reply_header(), Some((5, None)));

        // Both given: they must agree.
        assert!(placed(FORUM, Some(12), Some(10), true).await.0.is_ok());
        let msg = invalid(placed(FORUM, Some(12), Some(20), true).await.0);
        assert!(msg.contains("in Topic 10, not in topic 20"), "{msg}");
        let msg = invalid(placed(FORUM, Some(99), None, true).await.0);
        assert!(msg.contains("reply_to 99 is not a message"), "{msg}");
    }
}
