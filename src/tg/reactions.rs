//! The Account's own reaction on a message (#47, `react`), independent of
//! grammers/TL.
//!
//! A reaction is one standard emoji, as Telegram names it in its list of
//! available reactions (`messages.getAvailableReactions`): the Live client
//! fetches that list once and keeps it, and [`standard`] matches what the
//! caller asked for against it, ignoring emoji variation selectors (a model
//! types `❤️`, Telegram names it `❤`). Custom emoji and paid (Telegram Stars)
//! reactions are refused before anything reaches Telegram ([`requested`]).
//! Whether a Chat allows that reaction is Telegram's to say when it is set.

use super::{
    TgError,
    chats::{ChatId, ChatRef},
};

/// The longest emoji `react` takes, in characters (a ZWJ sequence is
/// several), like get_messages' reaction keys.
pub const MAX_EMOJI_CHARS: usize = 16;

/// How many of Telegram's standard reactions an error lists at most.
const MAX_LISTED: usize = 100;

/// The Account's reaction to set on a message: `emoji` replaces its previous
/// one there, `None` removes it. `emoji` is what [`requested`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReaction {
    pub chat: ChatRef,
    pub message_id: i32,
    pub emoji: Option<String>,
}

/// A reaction Telegram took (or already had).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reacted {
    pub chat: ChatId,
    pub message_id: i32,
    /// The emoji as Telegram names it; `None`: the Account's reaction is gone.
    pub emoji: Option<String>,
}

/// What the caller asked for as `emoji`, checked before Telegram is asked:
/// one emoji, without variation selectors. A custom emoji (get_messages
/// sums them as `custom`; a custom emoji's id is a number) and a paid
/// reaction (`paid`) are refused, and so is anything written in ASCII.
pub fn requested(emoji: &str) -> Result<String, TgError> {
    let invalid = |msg: &str| Err(TgError::InvalidInput(msg.into()));
    let trimmed = emoji.trim();
    let lower = trimmed.to_ascii_lowercase();
    if trimmed.is_empty() {
        return invalid(
            "emoji is empty: give one emoji such as 👍, or null to remove the reaction",
        );
    }
    if lower.starts_with("custom") || trimmed.chars().all(|c| c.is_ascii_digit()) {
        return invalid(
            "custom emoji reactions are not supported: react with a standard emoji such as 👍",
        );
    }
    if matches!(lower.as_str(), "paid" | "star" | "stars") {
        return invalid(
            "paid reactions (Telegram Stars) are not supported: react with a standard emoji \
             such as 👍",
        );
    }
    if trimmed.chars().any(|c| c.is_ascii() || c.is_control()) {
        return invalid("emoji is one emoji such as 👍, not a name or a code");
    }
    if trimmed.chars().count() > MAX_EMOJI_CHARS {
        return invalid("emoji is one emoji such as 👍, not several");
    }
    Ok(bare(trimmed))
}

/// The standard reaction `requested` is, as Telegram names it in
/// `available` (its active reactions, in its order); anything else is
/// [`TgError::InvalidInput`] listing them.
pub fn standard(requested: &str, available: &[String]) -> Result<String, TgError> {
    let wanted = bare(requested);
    if let Some(found) = available.iter().find(|e| bare(e) == wanted) {
        return Ok(found.clone());
    }
    let listed: Vec<&str> = available
        .iter()
        .take(MAX_LISTED)
        .map(String::as_str)
        .collect();
    Err(TgError::InvalidInput(format!(
        "{wanted} is not one of Telegram's standard reactions (custom emoji and paid reactions \
         are not supported); they are: {}",
        listed.join(" ")
    )))
}

/// What Telegram's `REACTION_INVALID` for a standard reaction says: the
/// Chat's admins turned reactions off or allow only some.
pub fn not_allowed(chat: ChatId, emoji: &str) -> TgError {
    TgError::InvalidInput(format!(
        "{chat} doesn't allow the reaction {emoji} (REACTION_INVALID): its admins turned \
         reactions off there or allow only some of them"
    ))
}

/// What a reaction on a message the Chat doesn't have says.
pub fn no_such_message(chat: ChatId, message_id: i32) -> TgError {
    TgError::InvalidInput(format!(
        "message {message_id} is not a message of {chat} (MESSAGE_ID_INVALID); ids come from \
         get_messages"
    ))
}

/// `emoji` without variation selectors (U+FE0E, U+FE0F).
fn bare(emoji: &str) -> String {
    emoji
        .chars()
        .filter(|c| !matches!(c, '\u{FE0E}' | '\u{FE0F}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(result: Result<String, TgError>) -> String {
        match result {
            Err(TgError::InvalidInput(msg)) => msg,
            other => panic!("expected invalid input, got {other:?}"),
        }
    }

    #[test]
    fn an_emoji_is_asked_for_without_its_variation_selector() {
        assert_eq!(requested("👍").unwrap(), "👍");
        assert_eq!(requested(" ❤\u{FE0F} ").unwrap(), "❤");
        // A ZWJ sequence is one emoji.
        assert_eq!(requested("❤\u{200D}🔥").unwrap(), "❤\u{200D}🔥");
    }

    #[test]
    fn custom_emoji_and_paid_reactions_are_refused_before_telegram() {
        for custom in ["custom", "Custom", "5368324170671202286", "custom:123"] {
            let msg = refused(requested(custom));
            assert!(
                msg.contains("custom emoji reactions are not supported"),
                "{msg}"
            );
        }
        for paid in ["paid", "Stars", "star"] {
            let msg = refused(requested(paid));
            assert!(msg.contains("paid reactions"), "{msg}");
        }
        for odd in ["thumbs up", ":+1:", "👍x", "\u{7}"] {
            let msg = refused(requested(odd));
            assert!(msg.contains("not a name or a code"), "{odd:?}: {msg}");
        }
        assert!(refused(requested("  ")).contains("null to remove"));
        assert!(refused(requested(&"👍".repeat(17))).contains("not several"));
    }

    #[test]
    fn a_reaction_is_the_standard_one_telegram_names() {
        let available: Vec<String> = ["👍", "❤", "🔥", "☺\u{FE0F}"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(standard("👍", &available).unwrap(), "👍");
        // Telegram's own spelling goes back, whichever the caller used.
        assert_eq!(
            standard(&requested("❤️").unwrap(), &available).unwrap(),
            "❤"
        );
        assert_eq!(standard("☺", &available).unwrap(), "☺\u{FE0F}");

        let msg = refused(standard("🦖", &available));
        assert!(
            msg.starts_with("🦖 is not one of Telegram's standard reactions"),
            "{msg}"
        );
        assert!(msg.ends_with("they are: 👍 ❤ 🔥 ☺\u{FE0F}"), "{msg}");
    }

    #[test]
    fn the_list_in_an_error_is_bounded() {
        let available: Vec<String> = (0..300).map(|n| format!("e{n}")).collect();
        let msg = refused(standard("🦖", &available));
        assert!(msg.contains(" e99"), "{msg}");
        assert!(!msg.contains(" e100"), "{msg}");
    }
}
