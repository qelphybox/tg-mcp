//! `/revoke`: OAuth 2.0 Token Revocation (RFC 7009) for our public clients.
//!
//! Revoking any Token revokes its whole Grant: the Grant (with every Token) is
//! deleted, then its Telegram session is logged out so it leaves the Account's
//! Active sessions. Only the presented Token can decrypt that session (ADR-0003),
//! which is why this happens here and nowhere without a Token.

use std::sync::Arc;

use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use super::{AuthState, token::no_store};
use crate::sessions::VaultError;

/// No `Debug`: the Token must never reach a log. `token_type_hint` is accepted and
/// ignored (RFC 7009 §2.1 allows that): we look the Token up by its id anyway.
#[derive(Deserialize)]
pub struct RevokeForm {
    token: Option<String>,
    client_id: Option<String>,
}

pub async fn revoke(State(auth): State<Arc<AuthState>>, Form(form): Form<RevokeForm>) -> Response {
    let token = form.token.map(Zeroizing::new);
    let (Some(token), Some(client_id)) = (token, form.client_id) else {
        return no_store(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid_request", "error_description": "token and client_id are required" }),
        );
    };
    let grant = match auth.vault.open_for_client(&token, &client_id).await {
        Ok(grant) => grant,
        // RFC 7009 §2.2: an invalid, expired or already revoked Token is a success,
        // so the answer doesn't tell which Tokens exist.
        Err(VaultError::InvalidToken) => return revoked(),
        // §2.1: a Token issued to another client is refused.
        Err(VaultError::WrongClient) => {
            return no_store(
                StatusCode::BAD_REQUEST,
                json!({ "error": "unauthorized_client", "error_description": "the token was not issued to this client" }),
            );
        }
        Err(err) => {
            tracing::error!(error = %err, "opening grant for revocation failed");
            return no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "server_error" }),
            );
        }
    };
    drop(token);

    // Delete first: from here on no Token of this Grant opens anything, whatever
    // Telegram does next.
    let grant_id = grant.grant_id.clone();
    // Its Media links go first: they download without a Token.
    let links = auth.links.revoke_grant(&grant_id);
    if links > 0 {
        tracing::info!(grant_id = %grant_id, links, "media links of a revoked grant dropped");
    }
    // So do its pending sends: they are decided without a Token.
    let sends = auth.confirmations.revoke_grant(&grant_id);
    if sends > 0 {
        tracing::info!(grant_id = %grant_id, sends, "pending sends of a revoked grant dropped");
    }
    if let Err(err) = auth.vault.delete_grant(&grant_id).await {
        tracing::error!(grant_id = %grant_id, error = %err, "deleting revoked grant failed");
        return no_store(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "server_error" }),
        );
    }
    crate::stats::global().revocation();
    match auth.connector.log_out(grant).await {
        Ok(()) => {
            tracing::info!(grant_id = %grant_id, "grant revoked, telegram session logged out")
        }
        Err(err) => tracing::warn!(
            grant_id = %grant_id,
            error = %err,
            "grant revoked, but logging the telegram session out failed; it stays in the account's active sessions"
        ),
    }
    revoked()
}

fn revoked() -> Response {
    no_store(StatusCode::OK, json!({}))
}
