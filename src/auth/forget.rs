//! `/forget` (#63): delete every Grant of an Account without a Token, for a
//! user who removed the app or lost its Tokens.
//!
//! `GET /forget` explains; its button posts to `/forget`, which starts a QR
//! login (Cloud password step included) on the Login flow's own pages, with
//! its `tgm_login` cookie and the per-IP limit of `/authorize` (its own
//! bucket). Once logged in, the login page sends it on to `/login/{id}/forget`:
//! how many connections the Account has and which apps they are for, and
//! Delete / Cancel. Nothing is deleted before the click. Either way the
//! session made for this check is logged out at the end, and so is one left
//! logged in when its login expires (the minute's sweep).
//!
//! ADR-0003 holds: deleting needs no Token because it destroys, never
//! decrypts. A Grant's Telegram session is logged out only where the Client
//! pool still holds it in memory; otherwise it stays in the Account's Active
//! sessions, which nobody can use any more, and the page says where to end it.
//! Logs carry the Account id, Grant ids and counts, as the Login flow does.

use std::sync::Arc;

use axum::{
    Extension,
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use super::{
    AuthState,
    authorize::{expired, not_an_authorization, owned_login, to_login_page},
    flows::{PendingLogin, Progress, Purpose},
    pages,
    rate_limit::ClientIp,
};
use crate::tg::LoggedIn;

/// `GET /forget`: what happens, and the button that starts.
pub async fn page() -> Response {
    pages::forget_intro()
}

/// `POST /forget`: start a QR login whose only use is `/forget`.
pub async fn start(
    State(auth): State<Arc<AuthState>>,
    Extension(client_ip): Extension<ClientIp>,
) -> Response {
    let unavailable = || {
        pages::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again later",
            "Too many logins are in progress. Wait a minute and try again.",
        )
    };
    let slot = match auth.flows.reserve(client_ip) {
        Ok(slot) => slot,
        Err(full) => {
            tracing::warn!(client_ip = %client_ip, reason = %full, "forget login refused");
            return unavailable();
        }
    };
    let attempt = match auth.login.start().await {
        Ok(attempt) => attempt,
        Err(err) => {
            tracing::warn!(error = %err, "starting telegram login for forget failed");
            return unavailable();
        }
    };
    let login_id = auth.flows.start_login(slot, Purpose::Forget, attempt);
    to_login_page(&auth, &login_id)
}

/// `GET /login/{id}/forget`: the Account's connections, and Delete / Cancel.
pub async fn review(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
) -> Response {
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    if !matches!(login.purpose, Purpose::Forget) {
        return not_an_authorization();
    }
    let progress = login.progress.lock().await;
    let Progress::LoggedIn(logged_in) = &*progress else {
        return Redirect::to(&format!("/login/{id}")).into_response();
    };
    let grants = match auth.vault.grants_of_account(logged_in.account.id).await {
        Ok(grants) => grants,
        Err(err) => {
            tracing::error!(error = %err, "listing grants for forget failed");
            return unavailable_now();
        }
    };
    let mut apps: Vec<String> = Vec::new();
    for (_, client_id) in &grants {
        let name = match auth.clients.get(client_id).await {
            Ok(Some(client)) => client.name,
            Ok(None) => "an app no longer registered".into(),
            Err(err) => {
                tracing::error!(error = %err, "loading oauth client for forget failed");
                return unavailable_now();
            }
        };
        if !apps.contains(&name) {
            apps.push(name);
        }
    }
    pages::forget_review(&id, &logged_in.account.display_name(), grants.len(), &apps)
}

#[derive(Deserialize)]
pub struct Decision {
    #[serde(default)]
    decision: String,
}

/// `POST /login/{id}/forget`: Delete or Cancel, once.
pub async fn decide(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
    Form(form): Form<Decision>,
) -> Response {
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    // Checked before the login is taken: an OAuth login stays usable.
    if !matches!(login.purpose, Purpose::Forget) {
        return not_an_authorization();
    }
    let delete = match form.decision.as_str() {
        "delete" => true,
        "cancel" => false,
        _ => {
            return pages::error(
                StatusCode::BAD_REQUEST,
                "Nothing chosen",
                "Go back and press Delete or Cancel.",
            );
        }
    };
    {
        let progress = login.progress.lock().await;
        if !matches!(&*progress, Progress::LoggedIn(_)) {
            return Redirect::to(&format!("/login/{id}")).into_response();
        }
    }
    drop(login);
    // Once: a second submit finds the login gone, or its session taken.
    let Some(login) = auth.flows.take_login(&id) else {
        return expired();
    };
    let Some(logged_in) = login.progress.lock().await.take_logged_in() else {
        return expired();
    };
    let account_id = logged_in.account.id;
    let response = if delete {
        match delete_grants(&auth, account_id).await {
            Ok(deleted) => pages::forget_deleted(deleted),
            Err(response) => response,
        }
    } else {
        tracing::info!(account_id, "forget: cancelled, nothing deleted");
        pages::forget_cancelled()
    };
    log_out_check(&auth, logged_in).await;
    response
}

/// Delete every Grant of `account_id`, as revocation does: Media links first,
/// then the Grant with its Tokens and sealed session, then its Live client,
/// logged out where the Client pool holds one. Returns how many went.
// The Err is the ready-to-send page; it is built once, on the failure path only.
#[allow(clippy::result_large_err)]
async fn delete_grants(auth: &AuthState, account_id: i64) -> Result<usize, Response> {
    let grants = auth
        .vault
        .grants_of_account(account_id)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "listing grants for forget failed");
            server_error()
        })?;
    let mut logged_out = 0;
    for (grant_id, _) in &grants {
        auth.links.revoke_grant(grant_id);
        if let Err(err) = auth.vault.delete_grant(grant_id).await {
            tracing::error!(grant_id = %grant_id, error = %err, "forget: deleting a grant failed");
            return Err(server_error());
        }
        match auth.connector.evict_and_log_out(grant_id).await {
            Ok(true) => logged_out += 1,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(grant_id = %grant_id, error = %err, "forget: logging a live client out failed");
            }
        }
    }
    tracing::info!(
        account_id,
        grants = grants.len(),
        logged_out,
        "forget: grants deleted"
    );
    Ok(grants.len())
}

/// A `/forget` login dropped by the sweep while logged in: end its session,
/// which nobody will use. Any other login is left as it is.
pub async fn log_out_abandoned(auth: &AuthState, login: &PendingLogin) {
    if !matches!(login.purpose, Purpose::Forget) {
        return;
    }
    let Some(logged_in) = login.progress.lock().await.take_logged_in() else {
        return;
    };
    tracing::info!(
        account_id = logged_in.account.id,
        "forget: abandoned check logged out"
    );
    log_out_check(auth, logged_in).await;
}

/// End the session made for this check; best effort, it is useless anyway.
async fn log_out_check(auth: &AuthState, logged_in: LoggedIn) {
    let account_id = logged_in.account.id;
    if let Err(err) = auth.connector.log_out_login(logged_in).await {
        tracing::warn!(account_id, error = %err, "forget: logging the check's session out failed");
    }
}

/// Reading failed before anything was deleted.
fn unavailable_now() -> Response {
    pages::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Server error",
        "Nothing was deleted. Try again later.",
    )
}

fn server_error() -> Response {
    pages::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Server error",
        "Nothing more was deleted. Try again later.",
    )
}
