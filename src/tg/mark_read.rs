//! Marking a Chat, or one Topic of a forum, as read (#47, `mark_read`),
//! independent of grammers/TL: `messages.readHistory` (users and basic
//! groups), `channels.readHistory` (supergroups and channels) or, for one
//! Topic, `messages.readDiscussion`. Telegram then clears the unread counter
//! on all the Account's devices.

use super::chats::{ChatId, ChatRef};

/// Mark `chat` read up to message `up_to` (its latest message when `None`);
/// with `topic`, only that Topic of a forum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkRead {
    pub chat: ChatRef,
    pub up_to: Option<i32>,
    pub topic: Option<i32>,
}

/// What was marked read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkedRead {
    pub chat: ChatId,
    pub topic: Option<i32>,
    /// The last message marked read; `None`: everything up to the latest.
    pub up_to: Option<i32>,
}
