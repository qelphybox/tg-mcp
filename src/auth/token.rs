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

use super::{AuthState, SCOPE, pkce};
use crate::sessions::{IssuedTokens, VaultError};

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
        .create_grant(
            &request.client_id,
            account_id,
            &issued.logged_in.telegram_session,
        )
        .await
    {
        Ok(issued) => token_response(&issued),
        Err(err) => server_error(err),
    }
}

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
        Err(VaultError::InvalidToken) => {
            oauth_error("invalid_grant", "invalid, used or expired refresh_token")
        }
        Err(err) => server_error(err),
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
            "scope": SCOPE,
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

fn no_store(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}
