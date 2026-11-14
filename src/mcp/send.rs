//! The tools of the `send` Scope, in their own router, so their Scope is the
//! block they are in:
//!
//! - `send_message` (#46) checks and formats its input as `save_draft` does,
//!   and sends it at once (ADR-0008): only into the Grant's Send allow-list,
//!   and never the same text to the same Chat twice within a minute of a
//!   send ([`crate::recent_sends`]);
//! - `react` and `mark_read` (#47) write only into the Send allow-list too.
//!
//! Whether a human approves each call is the MCP client's business: the
//! annotations ask clients to, and nothing here relies on it. Each spends the
//! Account's Writes budget; only a sent message counts against the Chat's
//! Send cap. Logs carry ids only.

use rmcp::{
    ErrorData,
    handler::server::{
        tool::Extension,
        wrapper::{Json, Parameters},
    },
    schemars, tool, tool_router,
};
use serde::{Deserialize, Deserializer, Serialize};

use super::{TgMcp, caller, telegram, tool_error};
use crate::{
    auth::Caller,
    recent_sends::{Duplicate, SendKey},
    tg::{
        Telegram, TgError,
        chats::{CHAT_REF_MAX_LEN, CHAT_REF_PATTERN, ChatRef, rfc3339},
        compose::{self, MAX_INPUT_CHARS},
        mark_read::MarkRead,
        markdown::TextFormat,
        reactions::{self, MAX_EMOJI_CHARS, NewReaction},
        send::NewMessage,
    },
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageParams {
    /// A chat id exactly as list_chats or find_chats gave it, e.g.
    /// `user:N~H`, `channel:N~H` or `chat:N`. Only the chats the user allowed
    /// this app to send to.
    #[schemars(length(max = CHAT_REF_MAX_LEN), regex(pattern = CHAT_REF_PATTERN))]
    pub chat_id: String,
    /// The message, 1 to 4096 characters once formatted.
    #[schemars(length(min = 1, max = MAX_INPUT_CHARS))]
    pub text: String,
    /// The id of a message in the same chat to reply to.
    #[schemars(range(min = 1))]
    pub reply_to: Option<i32>,
    /// Forums only: the Topic to send in (an id from get_chat_info `topics`;
    /// General is 1). Needed unless `reply_to` is given.
    #[schemars(range(min = 1))]
    pub topic: Option<i32>,
    /// `markdown` (default) or `plain`.
    pub format: Option<TextFormat>,
}

/// What became of a message send_message sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    /// Telegram took it: it is in the chat.
    Sent,
}

/// send_message's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SendResult {
    pub status: SendStatus,
    /// The chat, as given.
    #[schemars(length(max = CHAT_REF_MAX_LEN))]
    pub chat_id: String,
    /// The new message's id in the chat.
    pub message_id: i32,
    /// When Telegram dated it (UTC).
    #[schemars(length(max = 40))]
    pub date: String,
    /// In a forum: the Topic it went to (General is 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReactParams {
    /// A chat id exactly as list_chats or find_chats gave it. Only the chats
    /// the user allowed this app to send to.
    #[schemars(length(max = CHAT_REF_MAX_LEN), regex(pattern = CHAT_REF_PATTERN))]
    pub chat_id: String,
    /// The message to react to, an id from get_messages.
    #[schemars(range(min = 1))]
    pub message_id: i32,
    /// One standard reaction emoji, e.g. "👍"; null removes the account's
    /// reaction. Required: pass null explicitly to remove.
    #[serde(deserialize_with = "nullable")]
    #[schemars(required, schema_with = "emoji_or_null")]
    pub emoji: Option<String>,
}

/// A field that must be given, as a value or `null`.
fn nullable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(deserializer)
}

/// `emoji`: a short string, or `null`.
fn emoji_or_null(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["string", "null"],
        "minLength": 1,
        "maxLength": MAX_EMOJI_CHARS,
    })
}

/// What react did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReactStatus {
    /// The account's reaction on the message is `emoji`, replacing any
    /// other it had there.
    Set,
    /// The account has no reaction on the message.
    Removed,
}

/// react's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ReactResult {
    pub status: ReactStatus,
    /// The chat, as given.
    #[schemars(length(max = CHAT_REF_MAX_LEN))]
    pub chat_id: String,
    pub message_id: i32,
    /// `set`: the reaction, as Telegram names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = MAX_EMOJI_CHARS))]
    pub emoji: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkReadParams {
    /// A chat id exactly as list_chats or find_chats gave it. Only the chats
    /// the user allowed this app to send to.
    #[schemars(length(max = CHAT_REF_MAX_LEN), regex(pattern = CHAT_REF_PATTERN))]
    pub chat_id: String,
    /// Mark read up to and including this message (an id from
    /// get_messages). Omit for the latest message.
    #[schemars(range(min = 1))]
    pub up_to: Option<i32>,
    /// Forums only: mark only this Topic read (an id from get_chat_info
    /// `topics`; General is 1). Omit for the whole chat.
    #[schemars(range(min = 1))]
    pub topic: Option<i32>,
}

/// mark_read's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct MarkReadResult {
    /// The chat, as given.
    #[schemars(length(max = CHAT_REF_MAX_LEN))]
    pub chat_id: String,
    /// In a forum: the Topic marked read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<i32>,
    /// The last message marked read; absent: everything up to the latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to: Option<i32>,
}

/// The tools of the `send` Scope.
#[tool_router(router = send_tools, vis = "pub(super)")]
impl TgMcp {
    #[tool(
        title = "Send message",
        description = "Send a message as the user: `text` into one chat (`chat_id` as list_chats \
or find_chats gave it, passed verbatim), where everyone in it reads it as written by the user. \
It is sent at once, and can't be edited or deleted afterwards. Only the chats the user allowed \
when connecting this app work; any other is an error. `format` is `markdown` (default): \
**bold**, *italic* or _italic_, `inline code`, ``` code blocks (```lang on the first line names \
the language), [text](https://…) links (http, https, tg and mailto); a backslash before a \
punctuation character writes it as is; anything else, and a marker without its partner, stays \
as typed. `format: \"plain\"` sends the text exactly as given. 1 to 4096 characters once \
formatted: longer is an error, never split. `reply_to`: the id of a message in the same chat to \
reply to (no partial quotes). In a forum (get_chat_info lists its `topics`) give `topic` \
(General is 1), or `reply_to` a message, whose Topic it then goes to; neither is an error, and \
so is `topic` on a chat that is no forum. Text only: no media, no forwarding. Returns `status: \
\"sent\"` with the `message_id`, its `date`, the `chat_id`, and the `topic` and `reply_to` it \
went to. The same text to the same chat within 60 s of sending it is not sent again: an error \
says \"already sent: message N\". Each message counts against the account's writes budget and \
the chat's limit of 5 messages a minute. An error that says to retry in N s, or PEER_FLOOD, \
means nothing was sent: never retry it on your own; tell the user.",
        annotations(
            title = "Send message",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn send_message(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<SendMessageParams>,
    ) -> Result<Json<SendResult>, ErrorData> {
        let invalid = |msg: String| ErrorData::invalid_params(msg, None);
        let chat: ChatRef = params
            .chat_id
            .parse()
            .map_err(|e: TgError| invalid(e.to_string()))?;
        for (name, id) in [("reply_to", params.reply_to), ("topic", params.topic)] {
            if id.is_some_and(|id| id < 1) {
                return Err(invalid(format!("{name} must be a positive id")));
            }
        }
        let text = compose::prepare_message(&params.text, params.format.unwrap_or_default())
            .map_err(tool_error)?;
        let caller = writer(&parts, chat)?;
        let message = NewMessage::new(chat, text, params.reply_to, params.topic);
        let key = self
            .recent_sends
            .key(caller.account_id, chat.chat(), &message.text);
        // Claimed before it goes to Telegram, so two identical calls at once
        // send one; a send Telegram refuses releases the claim.
        let claim = self
            .recent_sends
            .claim(key)
            .map_err(|duplicate| refuse_duplicate(caller, &key, duplicate))?;
        let sent = caller
            .telegram
            .send_message(message)
            .await
            .map_err(tool_error)?;
        claim.sent(sent.id);
        // Ids only: never the text.
        tracing::info!(
            grant_id = %caller.grant_id,
            chat = %sent.chat,
            message_id = sent.id,
            "message sent"
        );
        Ok(Json(SendResult {
            status: SendStatus::Sent,
            chat_id: chat.to_string(),
            message_id: sent.id,
            date: rfc3339(sent.date),
            topic: sent.topic,
            reply_to: sent.reply_to,
        }))
    }

    #[tool(
        title = "React to a message",
        description = "React to a message as the user: set the account's reaction on message \
`message_id` (as get_messages gave it) of one chat (`chat_id` as list_chats or find_chats gave \
it, passed verbatim) to `emoji`, one standard Telegram reaction such as 👍 ❤ 🔥 🎉 😁 🤔 👌 🙏, \
replacing the account's previous reaction on that message; `emoji: null` removes it. Everyone \
in the chat sees it as the user's. Custom emoji and paid (Telegram Stars) reactions are not \
supported, and an emoji that is no standard reaction is an error that lists them; a chat whose \
admins allow only some reactions, or none, refuses the others with an error. Only the chats the \
user allowed when connecting this app work; any other is an error. Setting the reaction that \
is already there changes nothing and is no error. Counts against the account's writes budget \
(not the chat's limit of messages). get_messages shows a message's reactions as counts. Returns `status` (`set` or `removed`), the `chat_id`, \
`message_id` and the `emoji` as Telegram names it. An error that says to retry in N s, or \
PEER_FLOOD, means nothing was written: never retry it on your own; tell the user.",
        annotations(
            title = "React to a message",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn react(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<ReactParams>,
    ) -> Result<Json<ReactResult>, ErrorData> {
        let invalid = |msg: String| ErrorData::invalid_params(msg, None);
        let chat: ChatRef = params
            .chat_id
            .parse()
            .map_err(|e: TgError| invalid(e.to_string()))?;
        if params.message_id < 1 {
            return Err(invalid("message_id must be a positive id".into()));
        }
        let emoji = params
            .emoji
            .as_deref()
            .map(reactions::requested)
            .transpose()
            .map_err(tool_error)?;
        let caller = writer(&parts, chat)?;
        let reacted = telegram(&parts)?
            .react(NewReaction {
                chat,
                message_id: params.message_id,
                emoji,
            })
            .await
            .map_err(tool_error)?;
        // Ids only: neither the message nor the emoji is logged.
        tracing::info!(
            grant_id = %caller.grant_id,
            chat = %reacted.chat,
            message_id = reacted.message_id,
            set = reacted.emoji.is_some(),
            "reaction written"
        );
        Ok(Json(ReactResult {
            status: if reacted.emoji.is_some() {
                ReactStatus::Set
            } else {
                ReactStatus::Removed
            },
            chat_id: chat.to_string(),
            message_id: reacted.message_id,
            emoji: reacted.emoji,
        }))
    }

    #[tool(
        title = "Mark as read",
        description = "Mark a chat as read as the user (`chat_id` as list_chats or find_chats \
gave it, passed verbatim): up to its latest message, or up to and including message `up_to` (as \
get_messages gave it). The unread counter clears on all the account's devices, and in a \
private chat the other person sees their messages as read. In a forum (get_chat_info lists its \
`topics`) `topic` marks only that Topic read (General is 1), up to its latest message or \
`up_to`; without `topic` the whole chat. `topic` on a chat that is no forum, or a Topic it \
doesn't have, is an error. Only the chats the user allowed when connecting this app work; any \
other is an error. Marking read again changes nothing. Counts \
against the account's writes budget. Returns the `chat_id`, and the `topic` and `up_to` it \
marked read (no `up_to`: up to the latest message). An error that says to retry in N s means \
nothing was marked: never retry it on your own; tell the user.",
        annotations(
            title = "Mark as read",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn mark_read(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<MarkReadParams>,
    ) -> Result<Json<MarkReadResult>, ErrorData> {
        let invalid = |msg: String| ErrorData::invalid_params(msg, None);
        let chat: ChatRef = params
            .chat_id
            .parse()
            .map_err(|e: TgError| invalid(e.to_string()))?;
        for (name, id) in [("up_to", params.up_to), ("topic", params.topic)] {
            if id.is_some_and(|id| id < 1) {
                return Err(invalid(format!("{name} must be a positive id")));
            }
        }
        let caller = writer(&parts, chat)?;
        let marked = telegram(&parts)?
            .mark_read(MarkRead {
                chat,
                up_to: params.up_to,
                topic: params.topic,
            })
            .await
            .map_err(tool_error)?;
        tracing::info!(
            grant_id = %caller.grant_id,
            chat = %marked.chat,
            topic = marked.topic,
            up_to = marked.up_to,
            "chat marked read"
        );
        Ok(Json(MarkReadResult {
            chat_id: chat.to_string(),
            topic: marked.topic,
            up_to: marked.up_to,
        }))
    }
}

/// What a send of a message already sent says. Ids only in the log.
fn refuse_duplicate(caller: &Caller, key: &SendKey, duplicate: Duplicate) -> ErrorData {
    let message_id = match duplicate {
        Duplicate::Sent { id, .. } => Some(id),
        Duplicate::Sending { .. } => None,
    };
    tracing::info!(
        grant_id = %caller.grant_id,
        chat = %key.chat(),
        message_id,
        "same text to the same chat within a minute: not sent again"
    );
    ErrorData::invalid_params(duplicate.to_string(), None)
}

/// The caller, if it may write to `chat`: only into its Send allow-list.
fn writer(parts: &http::request::Parts, chat: ChatRef) -> Result<&Caller, ErrorData> {
    let caller = caller(parts)?;
    caller
        .may_write_to(chat.chat())
        .map_err(|refused| ErrorData::invalid_params(refused.to_string(), None))?;
    Ok(caller)
}

#[cfg(test)]
mod tests;
