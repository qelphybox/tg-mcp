//! `/authorize` and the browser pages of the Login flow.
//!
//! Every `/login/{id}` request must carry the `tgm_login` cookie set by `/authorize`
//! in the same browser. SameSite=Lax keeps it off cross-site POSTs, which is the
//! CSRF protection of the Consent form.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::{
    AuthState,
    flows::{AuthRequest, Progress},
    pages, pkce,
};
use crate::tg::{LoginState, TgError};

const COOKIE: &str = "tgm_login";

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
}

pub async fn authorize(
    State(auth): State<Arc<AuthState>>,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    // Until client and redirect_uri check out, errors are shown, never redirected.
    let Some(client_id) = q.client_id.as_deref() else {
        return pages::error(
            StatusCode::BAD_REQUEST,
            "Missing client",
            "The request has no client_id.",
        );
    };
    let client = match auth.clients.get(client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => {
            return pages::error(
                StatusCode::BAD_REQUEST,
                "Unknown client",
                "This app is not registered here. Remove the server from your app and add it again.",
            );
        }
        Err(err) => {
            tracing::error!(error = %err, "loading oauth client failed");
            return pages::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Server error",
                "Try again later.",
            );
        }
    };
    let Some(redirect_uri) = q
        .redirect_uri
        .clone()
        .filter(|uri| client.accepts_redirect(uri))
    else {
        return pages::error(
            StatusCode::BAD_REQUEST,
            "Invalid redirect",
            "The redirect_uri is missing or was not registered by this app.",
        );
    };

    let fail = |error: &str, description: &str| {
        redirect_with(
            &redirect_uri,
            &[("error", error), ("error_description", description)],
            q.state.as_deref(),
            &auth.base_url,
        )
    };
    if q.response_type.as_deref() != Some("code") {
        return fail(
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    if q.code_challenge_method.as_deref() != Some("S256") {
        return fail(
            "invalid_request",
            "PKCE with code_challenge_method=S256 is required",
        );
    }
    let Some(code_challenge) = q
        .code_challenge
        .clone()
        .filter(|c| pkce::challenge_is_well_formed(c))
    else {
        return fail("invalid_request", "code_challenge is missing or malformed");
    };
    if let Some(resource) = q.resource.as_deref()
        && !auth.is_our_resource(resource)
    {
        return fail(
            "invalid_target",
            "this server only issues tokens for its /mcp resource",
        );
    }

    let attempt = match auth.login.start().await {
        Ok(attempt) => attempt,
        Err(err) => {
            tracing::warn!(error = %err, "starting telegram login failed");
            return fail(
                "temporarily_unavailable",
                "Telegram login could not be started",
            );
        }
    };
    let request = AuthRequest {
        client_id: client.id,
        client_name: client.name,
        redirect_uri: redirect_uri.clone(),
        code_challenge,
        state: q.state.clone(),
    };
    let Ok(login_id) = auth.flows.start_login(request, attempt) else {
        return fail("temporarily_unavailable", "too many logins in progress");
    };

    let mut response = Redirect::to(&format!("/login/{login_id}")).into_response();
    let secure = if auth.base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie =
        format!("{COOKIE}={login_id}; Path=/login; HttpOnly; SameSite=Lax; Max-Age=600{secure}");
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("ascii cookie"),
    );
    response
}

pub async fn login_page(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
) -> Response {
    if let Err(response) = owned_login(&auth, &headers, &id) {
        return response;
    }
    pages::login(&id)
}

pub async fn login_status(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
) -> Response {
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let mut progress = login.progress.lock().await;
    if let Progress::InProgress(attempt) = &mut *progress {
        match attempt.poll().await {
            Ok(LoginState::WaitingForScan { qr_url }) => {
                return Json(json!({ "state": "waiting", "qr_url": qr_url, "qr_svg": qr_svg_base64(&qr_url) }))
                    .into_response();
            }
            Ok(LoginState::LoggedIn(logged_in)) => {
                tracing::info!(account_id = logged_in.account.id, "telegram login finished");
                *progress = Progress::LoggedIn(logged_in);
            }
            Err(TgError::TwoFactorRequired) => {
                let message = "This account has two-step verification enabled. \
                               Logging in with a cloud password isn't supported yet.";
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "state": "error", "message": message })),
                )
                    .into_response();
            }
            Err(err) => {
                tracing::warn!(error = %err, "telegram login poll failed");
                let message = "Telegram didn't answer. Start again from your app.";
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "state": "error", "message": message })),
                )
                    .into_response();
            }
        }
    }
    Json(json!({ "state": "logged_in", "next": format!("/login/{id}/consent") })).into_response()
}

pub async fn consent_page(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
) -> Response {
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let progress = login.progress.lock().await;
    let Progress::LoggedIn(logged_in) = &*progress else {
        return Redirect::to(&format!("/login/{id}")).into_response();
    };
    let target = redirect_target(&login.request.redirect_uri);
    let account_name = logged_in.account.display_name();
    pages::consent(&pages::ConsentView {
        login_id: &id,
        client_name: &login.request.client_name,
        redirect_target: &target,
        account_name: &account_name,
        account_username: logged_in.account.username.as_deref(),
    })
}

#[derive(Debug, Deserialize)]
pub struct ConsentForm {
    decision: String,
}

pub async fn consent_submit(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
    Form(form): Form<ConsentForm>,
) -> Response {
    if let Err(response) = owned_login(&auth, &headers, &id) {
        return response;
    }
    let Some(login) = auth.flows.take_login(&id) else {
        return expired();
    };
    let Ok(login) = Arc::try_unwrap(login) else {
        // A status poll still holds it; that only happens on a double submit.
        return pages::error(
            StatusCode::CONFLICT,
            "Already submitted",
            "This login was already finished.",
        );
    };
    let request = login.request;
    let state = request.state.clone();
    match (form.decision.as_str(), login.progress.into_inner()) {
        ("allow", Progress::LoggedIn(logged_in)) => {
            let redirect_uri = request.redirect_uri.clone();
            let client_id = request.client_id.clone();
            let code = auth.flows.issue_code(request, logged_in);
            tracing::info!(client_id = %client_id, "authorization code issued");
            redirect_with(
                &redirect_uri,
                &[("code", &code)],
                state.as_deref(),
                &auth.base_url,
            )
        }
        ("allow", Progress::InProgress(_)) => pages::error(
            StatusCode::BAD_REQUEST,
            "Not logged in",
            "Finish the Telegram login first.",
        ),
        _ => redirect_with(
            &request.redirect_uri,
            &[
                ("error", "access_denied"),
                ("error_description", "the user denied access"),
            ],
            state.as_deref(),
            &auth.base_url,
        ),
    }
}

/// The login exists and this browser is the one that started it.
// The Err is the ready-to-send page; it is built once, on the failure path only.
#[allow(clippy::result_large_err)]
fn owned_login(
    auth: &AuthState,
    headers: &HeaderMap,
    id: &str,
) -> Result<Arc<super::flows::PendingLogin>, Response> {
    let cookie_matches = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(name, value)| name == COOKIE && value == id);
    if !cookie_matches {
        return Err(pages::error(
            StatusCode::FORBIDDEN,
            "Wrong browser",
            "This login was started in another browser or has expired. Start again from your app.",
        ));
    }
    auth.flows.login(id).ok_or_else(expired)
}

fn expired() -> Response {
    pages::error(
        StatusCode::GONE,
        "Login expired",
        "Start again from your app.",
    )
}

/// Redirect to the client with OAuth parameters (+ RFC 9207 `iss`).
fn redirect_with(
    redirect_uri: &str,
    params: &[(&str, &str)],
    state: Option<&str>,
    issuer: &str,
) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        return pages::error(
            StatusCode::BAD_REQUEST,
            "Invalid redirect",
            "The redirect_uri is not a URL.",
        );
    };
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
        if let Some(state) = state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer);
    }
    Redirect::to(url.as_str()).into_response()
}

/// What the Consent screen tells the user they'll be sent to.
fn redirect_target(redirect_uri: &str) -> String {
    match Url::parse(redirect_uri) {
        Ok(url) => match url.host_str() {
            Some(host) if url.scheme() == "https" || url.scheme() == "http" => host.to_string(),
            Some(host) => format!("{}://{host}", url.scheme()),
            None => format!("{}:", url.scheme()),
        },
        Err(_) => redirect_uri.to_string(),
    }
}

/// The `tg://login` URL as an SVG QR code, base64 for a `data:` URL. The fake
/// login's URL is rendered too; only an unencodable URL yields `None`.
fn qr_svg_base64(qr_url: &str) -> Option<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let code = qrcode::QrCode::with_error_correction_level(qr_url, qrcode::EcLevel::M).ok()?;
    let svg = code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build();
    Some(STANDARD.encode(svg))
}
