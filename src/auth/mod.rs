//! The `auth` module: this server is its own OAuth 2.1 authorization server
//! (ADR-0003), and the resource server check in front of `/mcp`.
//!
//! Discovery: `/mcp` → 401 + `WWW-Authenticate: Bearer resource_metadata=…` →
//! protected-resource metadata (RFC 9728) → authorization-server metadata
//! (RFC 8414) → `/register` (RFC 7591) → `/authorize` + Login flow → `/token`.

mod authorize;
pub mod clients;
mod flows;
mod pages;
mod pkce;
mod token;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use self::{clients::ClientRegistry, flows::Flows};
use crate::{
    sessions::{GrantId, Vault, VaultError},
    tg::{Connector, Login, Telegram, TgError},
};

/// The only scope: read-only access to all chats.
pub const SCOPE: &str = "read";

pub struct AuthState {
    /// `scheme://host[:port]` of the public URL, no trailing slash. Also the issuer.
    pub base_url: String,
    pub vault: Arc<Vault>,
    pub clients: ClientRegistry,
    pub login: Arc<dyn Login>,
    pub connector: Arc<dyn Connector>,
    flows: Flows,
}

impl AuthState {
    pub fn new(
        base_url: String,
        vault: Arc<Vault>,
        clients: ClientRegistry,
        login: Arc<dyn Login>,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            vault,
            clients,
            login,
            connector,
            flows: Flows::default(),
        }
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

/// Sweep expired Login flows every minute, for as long as the server lives.
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
        }
    });
}

/// Who is calling `/mcp`: set by [`require_bearer`] on every request, read by tools.
#[derive(Clone)]
pub struct Caller {
    pub grant_id: GrantId,
    pub telegram: Arc<dyn Telegram>,
}

/// All OAuth endpoints and Login flow pages.
pub fn router(auth: Arc<AuthState>) -> Router {
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
        .route("/register", post(register))
        .route("/token", post(token::token))
        .route("/authorize", get(authorize::authorize))
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
        .route("/login/assets/login.js", get(login_js))
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
    let telegram = match auth.connector.connect(grant).await {
        Ok(telegram) => telegram,
        Err(err @ TgError::BadSession(_)) => {
            // Logged out in Telegram or unusable: the client has to log in again.
            tracing::warn!(grant_id = %grant_id, error = %err, "grant has an unusable telegram session");
            return unauthorized(&auth, Some("invalid_token"));
        }
        Err(err) => {
            tracing::warn!(grant_id = %grant_id, error = %err, "connecting to telegram failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    request
        .extensions_mut()
        .insert(Caller { grant_id, telegram });
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
        "scopes_supported": [SCOPE],
        "resource_name": "Telegram (unofficial, read-only)",
    }))
    .into_response()
}

async fn authorization_server(State(auth): State<Arc<AuthState>>) -> Response {
    let base = &auth.base_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
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
