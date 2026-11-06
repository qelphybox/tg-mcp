//! tg-mcp: a read-only remote MCP server for a personal Telegram account.

pub mod config;
pub mod mcp;
pub mod tg;

use std::sync::Arc;

use axum::{Router, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, mcp::TgMcp, tg::Telegram};

/// Build the whole HTTP surface. `shutdown` ends open MCP streams on graceful shutdown.
pub fn app(config: &Config, telegram: Arc<dyn Telegram>, shutdown: CancellationToken) -> Router {
    let mcp = StreamableHttpService::new(
        move || Ok(TgMcp::new(telegram.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts())
            .with_cancellation_token(shutdown),
    );

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/mcp", mcp)
}
