//! The `auth` module: this server is its own OAuth 2.1 authorization server
//! (ADR-0003), and the resource server check in front of `/mcp`.
//!
//! Discovery: `/mcp` → 401 + `WWW-Authenticate: Bearer resource_metadata=…` →
//! protected-resource metadata (RFC 9728) → authorization-server metadata
//! (RFC 8414) → `/register` (RFC 7591) → `/authorize` + Login flow → `/token`.
//! `/revoke` (RFC 7009) ends a Grant and logs its Telegram session out.

pub(crate) mod authorize;
pub mod clients;
mod flows;
pub(crate) mod pages;
mod pkce;
mod qr;
pub mod rate_limit;
mod revoke;
mod send_list;
mod token;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use self::{
    clients::ClientRegistry,
    flows::Flows,
    rate_limit::{Endpoint, OAuthLimits, RateLimiter},
};
use crate::{
    allow_list::{SendAllowList, WriteRefused, may_write},
    confirm::PendingSends,
    links::MediaLinks,
    scope::{Scope, Scopes},
    sessions::{GrantId, Vault, VaultError},
    tg::chats::ChatId,
    tg::{Connector, Login, Telegram, TgError},
};

/// The Scopes the Consent screen offers besides `read`, which is always granted.
/// `send` comes with the Send allow-list chosen there. `organize` has no tools
/// yet: it is advertised but not offered, and a request for it grants nothing.
pub const CONSENT_OFFERS: &[Scope] = &[Scope::Draft, Scope::Send];

/// `scopes_supported` of both metadata documents: every Scope there is.
fn scopes_supported() -> Vec<&'static str> {
    Scope::ALL.iter().map(|s| s.as_str()).collect()
}

pub struct AuthState {
    /// `scheme://host[:port]` of the public URL, no trailing slash. Also the issuer.
    pub base_url: String,
    pub vault: Arc<Vault>,
    pub clients: ClientRegistry,
    pub login: Arc<dyn Login>,
    pub connector: Arc<dyn Connector>,
    /// Media links: revoking a Grant ends them, and `/media/{ticket}` serves them.
    pub links: Arc<MediaLinks>,
    /// Pending sends: revoking a Grant ends them, and `/confirm/{id}` decides them.
    pub confirmations: Arc<PendingSends>,
    flows: Flows,
    rate_limiter: RateLimiter,
}

impl AuthState {
    pub fn new(
        base_url: String,
        vault: Arc<Vault>,
        clients: ClientRegistry,
        login: Arc<dyn Login>,
        connector: Arc<dyn Connector>,
        links: Arc<MediaLinks>,
        limits: OAuthLimits,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            confirmations: Arc::new(PendingSends::new(&base_url)),
            base_url,
            vault,
            clients,
            login,
            connector,
            links,
            flows: Flows::new(
                limits.max_pending_logins,
                limits.max_pending_logins_per_client,
            ),
            rate_limiter: RateLimiter::new(limits),
        }
    }

    /// Served over https: cookies are `Secure`.
    pub(crate) fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.base_url)
    }

    /// RFC 8707 `resource`: we only issue Tokens for our own MCP endpoint.
    fn is_our_resource(&self, resource: &str) -> bool {
        let resource = resource.trim_end_matches('/');
        resource == self.mcp_url() || resource == self.base_url
    }
}

/// Sweep expired Login flows, Media links and pending sends every minute, for
/// as long as the server lives.
pub fn spawn_flow_sweeper(auth: &Arc<AuthState>) {
    let auth = Arc::downgrade(auth);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            every.tick().await;
            let Some(auth) = auth.upgrade() else { return };
            let swept = auth.flows.sweep();
            if swept > 0 {
                tracing::info!(swept, "abandoned logins dropped");
            }
            let links = auth.links.sweep();
            if links > 0 {
                tracing::info!(links, "expired media links dropped");
            }
            let sends = auth.confirmations.sweep();
            if sends > 0 {
                tracing::info!(sends, "expired pending sends dropped");
            }
        }
    });
}

/// Who is calling `/mcp`: set by [`require_bearer`] on every request, read by tools.
#[derive(Clone)]
pub struct Caller {
    pub grant_id: GrantId,
    /// The Grant's Account: what the duplicate guard of sends counts by.
    pub account_id: i64,
    /// The Grant's Scopes: which tools this caller sees and may call.
    pub scopes: Scopes,
    /// With `send`: the Chats the Grant may write to, opened with its Token.
    pub send_allow_list: Option<Arc<SendAllowList>>,
    pub telegram: Arc<dyn Telegram>,
}

impl Caller {
    /// May this caller write to `chat` (send, react, mark as read)? Only with
    /// the `send` Scope and only into its Grant's Send allow-list. Every tool
    /// of the `send` Scope asks this before it writes.
    pub fn may_write_to(&self, chat: ChatId) -> Result<(), WriteRefused> {
        may_write(self.scopes, self.send_allow_list.as_deref(), chat)
    }
}

/// All OAuth endpoints and Login flow pages. Only the anonymous OAuth endpoints are
/// rate limited: the Login flow pages are polled by the page and need its cookie.
pub fn router(auth: Arc<AuthState>) -> Router {
    let limited =
        |endpoint| middleware::from_fn_with_state((auth.clone(), endpoint), rate_limit::limit);
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/register",
            post(register).route_layer(limited(Endpoint::Register)),
        )
        .route(
            "/token",
            post(token::token).route_layer(limited(Endpoint::Token)),
        )
        .route(
            "/revoke",
            post(revoke::revoke).route_layer(limited(Endpoint::Revoke)),
        )
        .route(
            "/authorize",
            get(authorize::authorize).route_layer(limited(Endpoint::Authorize)),
        )
        .route("/login/{id}", get(authorize::login_page))
        .route("/login/{id}/status", get(authorize::login_status))
        .route(
            "/login/{id}/password",
            get(authorize::password_page).post(authorize::password_submit),
        )
        .route(
            "/login/{id}/consent",
            get(authorize::consent_page).post(authorize::consent_submit),
        )
        .route(
            "/login/{id}/consent/chats",
            post(authorize::consent_chats).route_layer(limited(Endpoint::ConsentChats)),
        )
        .route("/login/assets/login.js", get(login_js))
        .route("/login/assets/pattern.svg", get(pattern_svg))
        .route("/login/assets/unbounded.woff2", get(unbounded_woff2))
        .route("/login/assets/golos-text.woff2", get(golos_woff2))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(auth)
}

/// Middleware for `/mcp`: a valid access Token or 401 with discovery pointers.
pub async fn require_bearer(
    State(auth): State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
    let Some(token) = token else {
        return unauthorized(&auth, None);
    };
    let grant = match auth.vault.open(token).await {
        Ok(grant) => grant,
        Err(VaultError::InvalidToken) => return unauthorized(&auth, Some("invalid_token")),
        Err(err) => {
            tracing::error!(error = %err, "opening grant failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let grant_id = grant.grant_id.clone();
    let account_id = grant.account_id;
    let scopes = grant.scopes;
    let send_allow_list = grant.send_allow_list.clone().map(Arc::new);
    let telegram = match auth.connector.connect(grant).await {
        Ok(telegram) => telegram,
        Err(err @ TgError::BadSession(_)) => {
            // Ended in Telegram (Active sessions) or unreadable: that is permanent, so
            // the Grant goes too, and the client has to log in again.
            tracing::warn!(grant_id = %grant_id, error = %err, "grant has a dead telegram session, deleting it");
            auth.links.revoke_grant(&grant_id);
            auth.confirmations.revoke_grant(&grant_id);
            if let Err(err) = auth.vault.delete_grant(&grant_id).await {
                tracing::error!(grant_id = %grant_id, error = %err, "deleting dead grant failed");
            }
            return unauthorized(&auth, Some("invalid_token"));
        }
        Err(err) => {
            tracing::warn!(grant_id = %grant_id, error = %err, "connecting to telegram failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    request.extensions_mut().insert(Caller {
        grant_id,
        account_id,
        scopes,
        send_allow_list,
        telegram,
    });
    next.run(request).await
}

fn unauthorized(auth: &AuthState, error: Option<&str>) -> Response {
    let mut challenge = format!(
        r#"Bearer resource_metadata="{}""#,
        auth.resource_metadata_url()
    );
    if let Some(error) = error {
        challenge.push_str(&format!(r#", error="{error}""#));
    }
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": error.unwrap_or("unauthorized") })),
    )
        .into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_str(&challenge).expect("ascii challenge"),
    );
    response
}

async fn protected_resource(State(auth): State<Arc<AuthState>>) -> Response {
    Json(json!({
        "resource": auth.mcp_url(),
        "authorization_servers": [auth.base_url],
        "bearer_methods_supported": ["header"],
        "scopes_supported": scopes_supported(),
        "resource_name": "Telegram (unofficial)",
    }))
    .into_response()
}

async fn authorization_server(State(auth): State<Arc<AuthState>>) -> Response {
    let base = &auth.base_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "revocation_endpoint": format!("{base}/revoke"),
        "revocation_endpoint_auth_methods_supported": ["none"],
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": scopes_supported(),
        "authorization_response_iss_parameter_supported": true,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

async fn register(State(auth): State<Arc<AuthState>>, body: Json<RegisterRequest>) -> Response {
    let Json(req) = body;
    let bad = |error: &str, description: &str| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error, "error_description": description })),
        )
            .into_response()
    };
    if req
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|m| m != "none")
    {
        return bad(
            "invalid_client_metadata",
            "only public clients (token_endpoint_auth_method=none) are supported",
        );
    }
    if req
        .grant_types
        .iter()
        .flatten()
        .any(|g| g != "authorization_code" && g != "refresh_token")
    {
        return bad(
            "invalid_client_metadata",
            "grant_types may only be authorization_code and refresh_token",
        );
    }
    if req.response_types.iter().flatten().any(|r| r != "code") {
        return bad("invalid_client_metadata", "response_types may only be code");
    }
    match auth
        .clients
        .register(req.client_name.as_deref(), &req.redirect_uris)
        .await
    {
        Ok(client) => (
            StatusCode::CREATED,
            Json(json!({
                "client_id": client.id,
                "client_name": client.name,
                "redirect_uris": client.redirect_uris,
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })),
        )
            .into_response(),
        Err(clients::RegisterError::InvalidRedirectUri(msg)) => bad("invalid_redirect_uri", &msg),
        Err(clients::RegisterError::InvalidMetadata(msg)) => bad("invalid_client_metadata", &msg),
        Err(clients::RegisterError::Database(err)) => {
            tracing::error!(error = %err, "client registration failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The pages' wallpaper pattern and fonts: never change within a release.
fn static_asset(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

async fn pattern_svg() -> Response {
    static_asset("image/svg+xml", include_bytes!("../../assets/pattern.svg"))
}

async fn unbounded_woff2() -> Response {
    static_asset(
        "font/woff2",
        include_bytes!("../../assets/fonts/unbounded.woff2"),
    )
}

async fn golos_woff2() -> Response {
    static_asset(
        "font/woff2",
        include_bytes!("../../assets/fonts/golos-text.woff2"),
    )
}

async fn login_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        pages::LOGIN_JS,
    )
        .into_response()
}
