//! tg-mcp: a read-only remote MCP server for a personal Telegram account.

pub mod auth;
pub mod config;
pub mod db;
pub mod limits;
pub mod mcp;
pub mod sessions;
pub mod tg;

use std::sync::Arc;

use axum::{Router, http::header, middleware, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    auth::{AuthState, clients::ClientRegistry},
    config::Config,
    mcp::TgMcp,
    sessions::Vault,
    tg::{Connector, Login},
};

/// What the HTTP surface needs from the outside world.
pub struct Deps {
    pub vault: Arc<Vault>,
    pub clients: ClientRegistry,
    pub login: Arc<dyn Login>,
    pub connector: Arc<dyn Connector>,
}

/// Build the whole HTTP surface. `shutdown` ends open MCP streams on graceful shutdown.
pub fn app(config: &Config, deps: Deps, shutdown: CancellationToken) -> Router {
    let auth = Arc::new(AuthState::new(
        config.base_url(),
        deps.vault,
        deps.clients,
        deps.login,
        deps.connector,
    ));
    auth::spawn_flow_sweeper(&auth);

    let mcp = StreamableHttpService::new(
        || Ok(TgMcp::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts())
            .with_cancellation_token(shutdown),
    );
    let mcp = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            auth::require_bearer,
        ));

    // No credentials are ever sent cross-origin (Bearer only, cookies are SameSite),
    // so browser-based MCP clients may call us from anywhere.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            header::WWW_AUTHENTICATE,
            header::HeaderName::from_static("mcp-session-id"),
        ]);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(mcp)
        .merge(auth::router(auth))
        .layer(cors)
}
