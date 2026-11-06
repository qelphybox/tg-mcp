//! The `mcp` module: rmcp tool server, routed to the caller's Telegram client.

use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Json},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

use crate::tg::{Account, Telegram, TgError};

const INSTRUCTIONS: &str = "Unofficial Telegram MCP server (uses the Telegram API). \
Read-only access to the connected Telegram account: nothing is ever sent, edited or deleted.";

#[derive(Clone)]
pub struct TgMcp {
    telegram: Arc<dyn Telegram>,
    tool_router: ToolRouter<Self>,
}

impl TgMcp {
    pub fn new(telegram: Arc<dyn Telegram>) -> Self {
        Self {
            telegram,
            tool_router: Self::tool_router(),
        }
    }
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
    async fn whoami(&self) -> Result<Json<Account>, ErrorData> {
        self.telegram.whoami().await.map(Json).map_err(tool_error)
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
