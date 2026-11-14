//! tg-mcp: a remote MCP server for a personal Telegram account: reading, and
//! the writes a Grant's Scopes allow (Drafts, and sends the user confirms).

pub mod allow_list;
pub mod auth;
pub mod config;
pub mod confirm;
pub mod db;
pub mod limits;
pub mod links;
pub mod mcp;
pub mod recent_sends;
pub mod scope;
pub mod sessions;
pub mod stats;
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
    links::MediaLinks,
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
///
/// Serve it with `into_make_service_with_connect_info::<SocketAddr>()`: the rate
/// limits of the OAuth endpoints need the peer address.
pub fn app(config: &Config, deps: Deps, shutdown: CancellationToken) -> Router {
    let links = Arc::new(MediaLinks::new(&config.base_url()));
    let auth = Arc::new(AuthState::new(
        config.base_url(),
        deps.vault,
        deps.clients,
        deps.login,
        deps.connector,
        links.clone(),
        config.oauth_limits,
    ));
    let confirmations = auth.confirmations.clone();
    auth::spawn_flow_sweeper(&auth);
    spawn_stats(&auth, shutdown.clone());

    let experimental_documents = config.experimental_documents;
    let mcp = StreamableHttpService::new(
        move || {
            Ok(TgMcp::new(links.clone(), experimental_documents)
                .with_confirmations(confirmations.clone()))
        },
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
        .merge(auth::router(auth.clone()))
        .layer(cors)
        // Media links are for curl in an agent's shell, never for a page:
        // merged after the CORS layer, so no browser may read them cross-origin.
        .merge(links::router(auth.clone()))
        // Nor may a page read or post the confirmation page cross-origin.
        .merge(confirm::router(auth))
}

/// The hourly `stats` line ([`stats`]), with what exists right now read from `auth`.
fn spawn_stats(auth: &Arc<AuthState>, shutdown: CancellationToken) {
    let auth = auth.clone();
    stats::spawn(
        move || {
            let auth = auth.clone();
            async move {
                let grants = match auth.vault.count_grants().await {
                    Ok(n) => Some(n),
                    Err(err) => {
                        tracing::warn!(error = %err, "counting grants for stats failed");
                        None
                    }
                };
                stats::Gauges {
                    grants,
                    live_clients: auth.connector.live_clients(),
                    login_flows: auth.pending_logins(),
                    media_links: auth.links.len(),
                }
            }
        },
        shutdown,
    );
}
