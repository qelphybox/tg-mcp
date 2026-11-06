//! The `mcp` module: rmcp tool server, routed to the caller's Telegram client.

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Json},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

use crate::{
    auth::Caller,
    tg::{Account, TgError},
};

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
    // The message is ours, never Telegram payload, so it is safe to return and log.
    tracing::warn!(error = %err, "tool call failed");
    ErrorData::internal_error(err.to_string(), None)
}
