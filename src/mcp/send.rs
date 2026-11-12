//! The tool of the `send` Scope (#46): `send_message`, in its own router, so
//! its Scope is the block it is in. It checks and formats its input as
//! `save_draft` does, and [`TgMcp::deliver`] sends it: only into the Grant's
//! Send allow-list, confirmed by the user unless the Grant has Auto-send.

use rmcp::{
    ErrorData,
    handler::server::{
        tool::Extension,
        wrapper::{Json, Parameters},
    },
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use serde::Deserialize;

use super::{TgMcp, caller, confirm::SendResult, tool_error};
use crate::tg::{
    TgError,
    chats::{CHAT_REF_MAX_LEN, CHAT_REF_PATTERN, ChatRef},
    compose::{self, MAX_INPUT_CHARS},
    markdown::TextFormat,
    send::NewMessage,
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

/// The tools of the `send` Scope.
#[tool_router(router = send_tools, vis = "pub(super)")]
impl TgMcp {
    #[tool(
        title = "Send message",
        description = "Send a message as the user: `text` into one chat (`chat_id` as list_chats \
or find_chats gave it, passed verbatim), where everyone in it reads it as written by the user. \
Only the chats the user allowed when connecting this app work; any other is an error. Unless the \
user chose to let this app send without asking, the user confirms each message on a page that \
shows it: a client that can open a URL for the user waits for their decision; otherwise the call \
returns `status: \"pending_confirmation\"` with `confirm_url` (valid until `expires_at`, 10 \
minutes): give the user that link, to open in the browser where they connected this app (on \
another device the page can only make it a Draft), and afterwards check get_messages (sent) or \
get_drafts (made a Draft); don't send it again meanwhile. `format` is `markdown` (default): \
**bold**, *italic* or _italic_, `inline code`, ``` code blocks (```lang on the first line names \
the language), [text](https://…) links (http, https, tg and mailto); a backslash before a \
punctuation character writes it as is; anything else, and a marker without its partner, stays \
as typed. `format: \"plain\"` sends the text exactly as given. 1 to 4096 characters once \
formatted: longer is an error, never split. `reply_to`: the id of a message in the same chat to \
reply to (no partial quotes). In a forum (get_chat_info lists its `topics`) give `topic` \
(General is 1), or `reply_to` a message, whose Topic it then goes to; neither is an error, and \
so is `topic` on a chat that is no forum. Text only: no media, no forwarding. Returns `status`: \
`sent` (with `message_id` and `date`), `cancelled` or `expired` (nothing was sent), `drafted` \
(the user made it a Draft in the chat's message field) or `pending_confirmation`, with the \
`chat_id`, and the `topic` and `reply_to` it went to. The same text to the same chat within 60 s \
of sending it is not sent again: an error says \"already sent: message N\". Each message counts \
against the account's writes budget and the chat's limit of 5 messages a minute. An error that \
says to retry in N s, or PEER_FLOOD, means nothing was sent: never retry it on your own; tell \
the user.",
        annotations(
            title = "Send message",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn send_message(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        context: RequestContext<RoleServer>,
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
        let message = NewMessage::new(chat, text, params.reply_to, params.topic);
        self.deliver(&context, caller(&parts)?, message)
            .await
            .map(Json)
    }
}
