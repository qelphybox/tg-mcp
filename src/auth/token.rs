//! `/token`: authorization_code (with PKCE) and refresh_token grants.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Form, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;

use super::{AuthState, pkce};
use crate::sessions::{GrantId, IssuedTokens, VaultError};

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

pub async fn token(State(auth): State<Arc<AuthState>>, Form(form): Form<TokenForm>) -> Response {
    if let Some(resource) = form.resource.as_deref()
        && !auth.is_our_resource(resource)
    {
        return oauth_error(
            "invalid_target",
            "this server only issues tokens for its /mcp resource",
        );
    }
    match form.grant_type.as_deref() {
        Some("authorization_code") => authorization_code(&auth, form).await,
        Some("refresh_token") => refresh_token(&auth, form).await,
        _ => oauth_error(
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}

async fn authorization_code(auth: &AuthState, form: TokenForm) -> Response {
    let (Some(code), Some(client_id), Some(redirect_uri), Some(verifier)) = (
        form.code.as_deref(),
        form.client_id.as_deref(),
        form.redirect_uri.as_deref(),
        form.code_verifier.as_deref(),
    ) else {
        return oauth_error(
            "invalid_request",
            "code, client_id, redirect_uri and code_verifier are required",
        );
    };
    // Taken before any check: a code is burnt by its first use, valid or not.
    let Some(issued) = auth.flows.take_code(code) else {
        return oauth_error("invalid_grant", "unknown, used or expired code");
    };
    let request = &issued.request;
    let client_ok: bool = request
        .client_id
        .as_bytes()
        .ct_eq(client_id.as_bytes())
        .into();
    if !client_ok
        || request.redirect_uri != redirect_uri
        || !pkce::verify(verifier, &request.code_challenge)
    {
        return oauth_error(
            "invalid_grant",
            "code does not match client, redirect_uri or code_verifier",
        );
    }
    let account_id = issued.logged_in.account.id;
    match auth
        .vault
        .create_grant_with_send(
            &request.client_id,
            account_id,
            &issued.logged_in.telegram_session,
            issued.scopes,
            issued.send_allow_list.as_ref(),
        )
        .await
    {
        Ok(issued) => token_response(&issued),
        Err(err) => server_error(err),
    }
}

/// A `scope` parameter is ignored: a refresh never changes a Grant's Scopes
/// (ADR-0005), and the response's `scope` says what the Grant has.
async fn refresh_token(auth: &AuthState, form: TokenForm) -> Response {
    let (Some(token), Some(client_id)) = (form.refresh_token.as_deref(), form.client_id.as_deref())
    else {
        return oauth_error(
            "invalid_request",
            "refresh_token and client_id are required",
        );
    };
    match auth.vault.refresh(token, client_id).await {
        Ok(issued) => token_response(&issued),
        Err(VaultError::InvalidToken) => invalid_refresh_token(),
        // Theft: the vault deleted the Grant and logged why. Drop what memory holds
        // of it, then answer exactly as for any bad refresh Token.
        Err(VaultError::GrantRevoked { grant_id, .. }) => {
            end_revoked_grant(auth, &grant_id).await;
            invalid_refresh_token()
        }
        Err(err) => server_error(err),
    }
}

fn invalid_refresh_token() -> Response {
    oauth_error("invalid_grant", "invalid, used or expired refresh_token")
}

/// A Grant revoked without a Token: its Media links go, and its Live client, if
/// pooled, logs its Telegram session out (best effort, ADR-0003).
async fn end_revoked_grant(auth: &AuthState, grant_id: &GrantId) {
    let links = auth.links.revoke_grant(grant_id);
    if links > 0 {
        tracing::info!(grant_id = %grant_id, links, "media links of a revoked grant dropped");
    }
    match auth.connector.evict_and_log_out(grant_id).await {
        Ok(true) => tracing::info!(
            grant_id = %grant_id,
            "revoked grant's live client logged its telegram session out"
        ),
        Ok(false) => tracing::info!(
            grant_id = %grant_id,
            "revoked grant had no live client; its telegram session stays in the account's active sessions"
        ),
        Err(err) => tracing::warn!(
            grant_id = %grant_id,
            error = %err,
            "logging a revoked grant's telegram session out failed; it stays in the account's active sessions"
        ),
    }
}

fn token_response(issued: &IssuedTokens) -> Response {
    no_store(
        StatusCode::OK,
        json!({
            "access_token": issued.access.expose(),
            "token_type": "Bearer",
            "expires_in": issued.access_expires_in.as_secs(),
            "refresh_token": issued.refresh.expose(),
            "scope": issued.scopes.to_string(),
        }),
    )
}

fn oauth_error(error: &str, description: &str) -> Response {
    no_store(
        StatusCode::BAD_REQUEST,
        json!({ "error": error, "error_description": description }),
    )
}

fn server_error(err: VaultError) -> Response {
    tracing::error!(error = %err, "token endpoint failed");
    no_store(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": "server_error" }),
    )
}

pub(super) fn no_store(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}
