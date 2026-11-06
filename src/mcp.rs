//! The `mcp` module: rmcp tool server, routed to the caller's Telegram client.

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        tool::Extension,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::{
    auth::Caller,
    tg::{Account, TgError, chats::ChatPage},
};

/// Output bounds, so one call can't flood the model's context.
const MAX_CHATS_PER_PAGE: usize = 100;
const DEFAULT_CHATS_PER_PAGE: usize = 30;
const MAX_TITLE_CHARS: usize = 128;
const MAX_PREVIEW_CHARS: usize = 100;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListChatsParams {
    /// How many chats to return (1-100, default 30).
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page. Omit for the first page.
    #[schemars(length(max = 512))]
    pub cursor: Option<String>,
    /// List the Archived folder instead of the main chat list.
    pub archived: Option<bool>,
}

const INSTRUCTIONS: &str = "Unofficial Telegram MCP server (uses the Telegram API). \
Read-only access to the connected Telegram account: nothing is ever sent, edited or deleted.";

/// One per MCP session. Holds no identity: every request carries its own [`Caller`],
/// put there by the Bearer check, so a session can never act as someone else.
#[derive(Clone)]
pub struct TgMcp {
    tool_router: ToolRouter<Self>,
}

impl TgMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for TgMcp {
    fn default() -> Self {
        Self::new()
    }
}

fn caller(parts: &http::request::Parts) -> Result<&Caller, ErrorData> {
    parts.extensions.get::<Caller>().ok_or_else(|| {
        ErrorData::internal_error("request reached a tool without authentication", None)
    })
}

#[tool_router]
impl TgMcp {
    #[tool(
        title = "Who am I",
        description = "Return the Telegram account this server is connected to: user id, name and @username.",
        annotations(
            title = "Who am I",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<Json<Account>, ErrorData> {
        caller(&parts)?
            .telegram
            .whoami()
            .await
            .map(Json)
            .map_err(tool_error)
    }

    #[tool(
        title = "List chats",
        description = "List the account's Telegram chats, most recent first: private chats, bots, \
groups, supergroups and channels. Each has an `id` (user:N, chat:N or channel:N) to pass to the \
other tools, a title, its type, the unread count and a short preview of the last message (media \
shown as [photo], [voice], ...). Paginated: pass `next_cursor` back as `cursor` for the next page; \
no `next_cursor` means this was the last page. Previews are cut to 100 characters.",
        annotations(
            title = "List chats",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_chats(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<ListChatsParams>,
    ) -> Result<Json<ChatPage>, ErrorData> {
        let limit = params.limit.map_or(DEFAULT_CHATS_PER_PAGE, |l| l as usize);
        if !(1..=MAX_CHATS_PER_PAGE).contains(&limit) {
            return Err(ErrorData::invalid_params(
                format!("limit must be between 1 and {MAX_CHATS_PER_PAGE}"),
                None,
            ));
        }
        let mut page = caller(&parts)?
            .telegram
            .list_chats(
                limit,
                params.cursor.as_deref(),
                params.archived.unwrap_or(false),
            )
            .await
            .map_err(tool_error)?;
        page.chats.truncate(limit);
        for chat in &mut page.chats {
            chat.title = shorten(&chat.title, MAX_TITLE_CHARS);
            if let Some(last) = &mut chat.last_message {
                last.text = shorten(&last.text, MAX_PREVIEW_CHARS);
            }
        }
        Ok(Json(page))
    }
}

/// Cut to `max` characters (not bytes), single line, marking the cut with `…`.
fn shorten(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let mut cut: String = flat.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TgMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tg-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

fn tool_error(err: TgError) -> ErrorData {
    // Messages are ours, never Telegram payload, so they are safe to return and log.
    match err {
        TgError::InvalidInput(msg) => ErrorData::invalid_params(msg, None),
        err => {
            tracing::warn!(error = %err, "tool call failed");
            ErrorData::internal_error(err.to_string(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shorten;

    #[test]
    fn shorten_counts_characters_and_flattens_lines() {
        assert_eq!(shorten("hi", 5), "hi");
        assert_eq!(shorten("привет мир", 7), "привет…");
        assert_eq!(shorten("line one\nline two", 100), "line one line two");
        assert_eq!(shorten("  padded  ", 100), "padded");
    }
}
