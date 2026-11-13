//! `/authorize` and the browser pages of the Login flow.
//!
//! Every `/login/{id}` request must carry the `tgm_login` cookie set by `/authorize`
//! in the same browser. SameSite=Lax keeps it off cross-site POSTs, which is the
//! CSRF protection of the Consent and Cloud password forms, and of the Consent
//! screen's chat pickers (which also are rate limited per client IP).

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use axum::{
    Extension, Json,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::json;
use url::Url;
use zeroize::Zeroizing;

use super::{
    AuthState, CONSENT_OFFERS, clients,
    flows::{AuthRequest, PendingLogin, Progress},
    pages, pkce,
    rate_limit::ClientIp,
    send_list::{self, Picked, Pickers, Refused},
};
use crate::{
    allow_list::{MAX_CHATS, SendAllowList},
    confirm::cookie,
    scope::{Scope, Scopes},
    tg::{LoggedIn, LoginState, TgError},
};

const COOKIE: &str = "tgm_login";
/// Tries per Login flow to load the Consent screen's chats.
const MAX_PICKER_LOADS: u32 = 5;
/// Wrong Cloud passwords per Login flow before it is dropped (Telegram has its own,
/// stricter-over-time limits on top, answered as `FLOOD_WAIT`).
const MAX_WRONG_PASSWORDS: u32 = 5;
/// Longer than any real Cloud password; such a value never goes to Telegram.
const MAX_PASSWORD_BYTES: usize = 1024;

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}

pub async fn authorize(
    State(auth): State<Arc<AuthState>>,
    Extension(client_ip): Extension<ClientIp>,
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
    let Ok(scopes) = Scopes::parse_request(q.scope.as_deref()) else {
        return fail(
            "invalid_scope",
            "the scopes of this server are read, draft, send and organize",
        );
    };

    // Each Login flow holds a Telegram connection: take a slot before opening one.
    let slot = match auth.flows.reserve(client_ip) {
        Ok(slot) => slot,
        Err(full) => {
            tracing::warn!(client_ip = %client_ip, reason = %full, "login flow refused");
            return fail("temporarily_unavailable", "too many logins in progress");
        }
    };
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
        scopes,
    };
    let login_id = auth.flows.start_login(slot, request, attempt);

    let mut response = Redirect::to(&format!("/login/{login_id}")).into_response();
    let secure = if auth.is_https() { "; Secure" } else { "" };
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
    match advance(&mut progress).await {
        Ok(Step::Waiting { qr_url }) => {
            Json(json!({ "state": "waiting", "qr_url": qr_url, "qr_svg": qr_svg_base64(&qr_url) }))
                .into_response()
        }
        Ok(Step::Password { .. }) => {
            Json(json!({ "state": "password", "next": format!("/login/{id}/password") }))
                .into_response()
        }
        Ok(Step::LoggedIn) => {
            Json(json!({ "state": "logged_in", "next": format!("/login/{id}/consent") }))
                .into_response()
        }
        Err(err) => {
            let (status, title, message) = telegram_failure(&err);
            (
                status,
                Json(json!({ "state": "error", "message": format!("{title}. {message}") })),
            )
                .into_response()
        }
    }
}

/// The Cloud password form, shown once the QR was scanned on a 2FA Account.
pub async fn password_page(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
) -> Response {
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let mut progress = login.progress.lock().await;
    match advance(&mut progress).await {
        Ok(Step::Password { hint }) => pages::password(StatusCode::OK, &id, hint.as_deref(), None),
        Ok(Step::Waiting { .. }) => Redirect::to(&format!("/login/{id}")).into_response(),
        Ok(Step::LoggedIn) => Redirect::to(&format!("/login/{id}/consent")).into_response(),
        Err(err) => telegram_failure_page(&err),
    }
}

/// No `Debug`: the password must never reach a log.
#[derive(Deserialize)]
pub struct PasswordForm {
    password: String,
}

/// Check the Cloud password with Telegram (SRP). The password lives only for the
/// duration of this request: it is wiped on drop and never stored or logged.
pub async fn password_submit(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
    Form(form): Form<PasswordForm>,
) -> Response {
    let password = Zeroizing::new(form.password);
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let mut progress = login.progress.lock().await;
    let hint = match advance(&mut progress).await {
        Ok(Step::Password { hint }) => hint,
        Ok(Step::Waiting { .. }) => return Redirect::to(&format!("/login/{id}")).into_response(),
        Ok(Step::LoggedIn) => {
            return Redirect::to(&format!("/login/{id}/consent")).into_response();
        }
        Err(err) => return telegram_failure_page(&err),
    };
    let retry = |status: StatusCode, error: &str| {
        pages::password(status, &id, hint.as_deref(), Some(error))
    };
    if let Some(left) = flood_wait_left(&login) {
        return retry(
            StatusCode::TOO_MANY_REQUESTS,
            &wait_message("password attempts", left),
        );
    }
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return retry(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enter your cloud password.",
        );
    }
    let Progress::InProgress(attempt) = &mut *progress else {
        return Redirect::to(&format!("/login/{id}/consent")).into_response();
    };
    match attempt.submit_password(password).await {
        Ok(logged_in) => {
            tracing::info!(
                account_id = logged_in.account.id,
                "telegram login finished with cloud password"
            );
            *progress = Progress::LoggedIn(logged_in);
            Redirect::to(&format!("/login/{id}/consent")).into_response()
        }
        Err(TgError::WrongPassword) => {
            let wrong = {
                let mut gate = login.password_gate.lock().expect("password gate lock");
                gate.wrong += 1;
                gate.wrong
            };
            tracing::info!(wrong, "wrong cloud password");
            if wrong >= MAX_WRONG_PASSWORDS {
                // The connection (and the half-logged-in auth key) goes with the login.
                drop(progress);
                auth.flows.take_login(&id);
                return pages::error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many attempts",
                    "The cloud password was wrong too many times. Start again from your app.",
                );
            }
            let left = MAX_WRONG_PASSWORDS - wrong;
            let attempts = if left == 1 { "attempt" } else { "attempts" };
            retry(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("Wrong password. {left} {attempts} left."),
            )
        }
        Err(TgError::FloodWait { seconds, .. }) => {
            tracing::warn!(seconds, "telegram flood wait on cloud password");
            login
                .password_gate
                .lock()
                .expect("password gate lock")
                .retry_at = Some(Instant::now() + Duration::from_secs(seconds.into()));
            retry(
                StatusCode::TOO_MANY_REQUESTS,
                &wait_message("password attempts", seconds.into()),
            )
        }
        Err(err) => telegram_failure_page(&err),
    }
}

/// Where a login stands after advancing it as far as possible without the user.
enum Step {
    Waiting { qr_url: String },
    Password { hint: Option<String> },
    LoggedIn,
}

/// Poll an unfinished login; a login that just finished is stored as such.
async fn advance(progress: &mut Progress) -> Result<Step, TgError> {
    let Progress::InProgress(attempt) = progress else {
        return Ok(Step::LoggedIn);
    };
    match attempt.poll().await? {
        LoginState::WaitingForScan { qr_url } => Ok(Step::Waiting { qr_url }),
        LoginState::PasswordNeeded { hint } => Ok(Step::Password { hint }),
        LoginState::LoggedIn(logged_in) => {
            tracing::info!(account_id = logged_in.account.id, "telegram login finished");
            *progress = Progress::LoggedIn(logged_in);
            Ok(Step::LoggedIn)
        }
    }
}

/// Seconds left of a `FLOOD_WAIT` Telegram gave this login, if any.
fn flood_wait_left(login: &PendingLogin) -> Option<u64> {
    let mut gate = login.password_gate.lock().expect("password gate lock");
    let left = gate.retry_at?.checked_duration_since(Instant::now());
    if left.is_none() {
        gate.retry_at = None;
    }
    // Round up: "0 seconds" would invite an immediate retry.
    left.map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0))
}

fn wait_message(what: &str, seconds: u64) -> String {
    let wait = match seconds {
        0..=90 => format!("{seconds} seconds"),
        91..=5400 => format!("{} minutes", seconds.div_ceil(60)),
        _ => format!("{} hours", seconds.div_ceil(3600)),
    };
    format!("Telegram limits {what}. Try again in {wait}.")
}

/// Status, title and message for a Telegram error during the Login flow.
fn telegram_failure(err: &TgError) -> (StatusCode, &'static str, String) {
    match err {
        TgError::FloodWait { seconds, .. } => {
            tracing::warn!(seconds, "telegram flood wait during login");
            (
                StatusCode::TOO_MANY_REQUESTS,
                "Telegram asks to slow down",
                wait_message("logins", u64::from(*seconds)),
            )
        }
        err => {
            tracing::warn!(error = %err, "telegram login step failed");
            (
                StatusCode::BAD_GATEWAY,
                "Telegram didn't answer",
                "Start again from your app.".into(),
            )
        }
    }
}

fn telegram_failure_page(err: &TgError) -> Response {
    let (status, title, message) = telegram_failure(err);
    pages::error(status, title, &message)
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
    let pickers = login.send_pickers.lock().await;
    // `send` never starts ticked: it means choosing chats, and clients that ask
    // for every advertised Scope would otherwise open on a list to fill in.
    let checked = offered(login.request.scopes.iter().filter(|s| *s != Scope::Send));
    let send = pages::SendView {
        choices: pickers.as_ref().map(Pickers::choices),
        ..pages::SendView::default()
    };
    consent(StatusCode::OK, &id, &login, logged_in, checked, send)
}

/// The Consent screen for `login`, with `checked` boxes and its Send part.
fn consent(
    status: StatusCode,
    id: &str,
    login: &PendingLogin,
    logged_in: &LoggedIn,
    checked: Scopes,
    send: pages::SendView<'_>,
) -> Response {
    let target = redirect_target(&login.request.redirect_uri);
    let account_name = logged_in.account.display_name();
    pages::consent(
        status,
        &pages::ConsentView {
            login_id: id,
            client_name: &login.request.client_name,
            redirect_target: &target,
            redirect_is_loopback: redirect_is_loopback(&login.request.redirect_uri),
            account_name: &account_name,
            account_username: logged_in.account.username.as_deref(),
            checked,
            send,
        },
    )
}

/// `read`, plus those of `scopes` the Consent screen offers. Anything else (a
/// Scope not offered yet, a name we don't know) grants nothing: a hand-made form
/// can't get more than the screen shows.
fn offered(scopes: impl IntoIterator<Item = Scope>) -> Scopes {
    scopes
        .into_iter()
        .filter(|scope| CONSENT_OFFERS.contains(scope))
        .collect()
}

/// The Consent form: `decision` (`allow` or `deny`), one `scope` per checked
/// box, as the browser posts them (`scope=draft&decision=allow`), and the Send
/// allow-list part: `auto_send`, one `folder` and one `chat` per ticked box,
/// and `q`, the name to search for.
#[derive(Debug, Default)]
pub struct ConsentForm {
    decision: String,
    scopes: Vec<String>,
    folders: Vec<String>,
    chats: Vec<String>,
    auto_send: bool,
    query: String,
}

impl ConsentForm {
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        pairs
            .into_iter()
            .fold(ConsentForm::default(), |mut form, (key, value)| {
                match key.as_str() {
                    "decision" if form.decision.is_empty() => form.decision = value,
                    "scope" => form.scopes.push(value),
                    "folder" => form.folders.push(value),
                    "chat" => form.chats.push(value),
                    "auto_send" => form.auto_send = value == "on",
                    "q" if form.query.is_empty() => form.query = value,
                    _ => {}
                }
                form
            })
    }

    /// The Scopes the user checked, as a Grant gets them.
    fn scopes(&self) -> Scopes {
        offered(self.scopes.iter().filter_map(|name| Scope::parse(name)))
    }

    /// The boxes of the Send allow-list ticked, as far as they parse; whether
    /// the screen offered them is [`Pickers::resolve`]'s business.
    fn picked(&self) -> Picked {
        Picked {
            folders: self
                .folders
                .iter()
                .filter_map(|id| id.parse().ok())
                .take(MAX_CHATS)
                .collect(),
            chats: self
                .chats
                .iter()
                .filter_map(|id| id.parse().ok())
                .take(MAX_CHATS)
                .collect(),
            auto_send: self.auto_send,
        }
    }
}

/// The Consent screen's chats: load them (once per Login flow) and, with a
/// name in `q`, search for more; then the screen again, as it was posted.
/// Reaches Telegram before any Token exists, so it needs the login's cookie,
/// a finished Telegram login, and is rate limited per client IP (and its
/// searches per Login flow).
pub async fn consent_chats(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let form = ConsentForm::from_pairs(pairs);
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let progress = login.progress.lock().await;
    let Progress::LoggedIn(logged_in) = &*progress else {
        return Redirect::to(&format!("/login/{id}")).into_response();
    };
    let mut pickers = login.send_pickers.lock().await;
    let picked = form.picked();
    let mut send = pages::SendView {
        query: form.query.trim(),
        ..pages::SendView::default()
    };
    let mut notice = None;
    let mut error = None;
    match load_pickers(&auth, &login, logged_in, &mut pickers).await {
        Err(refused) => error = Some(refused),
        Ok(loaded) if !send.query.is_empty() => match loaded.search(send.query, &picked).await {
            Ok(0) => notice = Some("None of your chats has that name.".to_string()),
            Ok(1) => notice = Some("Found 1 of your chats.".to_string()),
            Ok(n) => notice = Some(format!("Found {n} of your chats.")),
            Err(refused) => error = Some(refused),
        },
        Ok(_) => {}
    }
    send.choices = pickers.as_ref().map(Pickers::choices);
    send.picked = picked;
    send.notice = notice.as_deref();
    send.error = error.as_ref().map(Refused::message);
    consent(StatusCode::OK, &id, &login, logged_in, form.scopes(), send)
}

/// The login's pickers, loaded through a Live client for its session if they
/// aren't yet. A failure loads nothing, so the next try asks Telegram again,
/// up to [`MAX_PICKER_LOADS`] times per Login flow (each opens a connection).
async fn load_pickers<'p>(
    auth: &AuthState,
    login: &PendingLogin,
    logged_in: &LoggedIn,
    pickers: &'p mut Option<Pickers>,
) -> Result<&'p mut Pickers, Refused> {
    if pickers.is_none() {
        let tries = login.picker_loads.fetch_add(1, Ordering::SeqCst);
        if tries >= MAX_PICKER_LOADS {
            return Err(Refused::Telegram(
                "Your chats couldn't be loaded. Start again from your app.".into(),
            ));
        }
        let telegram = auth
            .connector
            .connect_for_consent(logged_in)
            .await
            .map_err(send_list::refused)?;
        *pickers = Some(Pickers::load(telegram).await?);
    }
    Ok(pickers.as_mut().expect("loaded above"))
}

pub async fn consent_submit(
    headers: HeaderMap,
    Path(id): Path<String>,
    State(auth): State<Arc<AuthState>>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let form = ConsentForm::from_pairs(pairs);
    let login = match owned_login(&auth, &headers, &id) {
        Ok(login) => login,
        Err(response) => return response,
    };
    let scopes = form.scopes();
    // The Send allow-list is settled before the login is taken: a refusal
    // shows the screen again, as it was posted.
    let send_allow_list = if form.decision == "allow" && scopes.contains(Scope::Send) {
        match send_allow_list(&auth, &id, &login, &form).await {
            Ok(list) => Some(list),
            Err(response) => return response,
        }
    } else {
        None
    };
    drop(login);
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
            // Counts only: which Chats is as private as their names.
            let send_chats = send_allow_list.as_ref().map_or(0, SendAllowList::len);
            let auto_send = send_allow_list
                .as_ref()
                .is_some_and(SendAllowList::auto_send);
            // With `send`, this browser gets the Confirmation cookie: the
            // only place its pending sends can be sent from (#45).
            let cookie = send_allow_list
                .is_some()
                .then(|| cookie::issue(auth.vault.ttl().refresh, auth.is_https()));
            let send_allow_list = send_allow_list.map(|list| match &cookie {
                Some(cookie) => list.with_confirm_cookie(cookie.verifier),
                None => list,
            });
            let code = auth
                .flows
                .issue_code(request, logged_in, scopes, send_allow_list);
            tracing::info!(client_id = %client_id, scopes = %scopes, send_chats, auto_send, "authorization code issued");
            let mut response = redirect_with(
                &redirect_uri,
                &[("code", &code)],
                state.as_deref(),
                &auth.base_url,
            );
            if let Some(cookie) = cookie {
                response
                    .headers_mut()
                    .append(header::SET_COOKIE, cookie.set_cookie);
            }
            response
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

/// The Send allow-list Allow with `send` gives: the chats ticked that the
/// screen offered and the chats of the folders ticked, read now. Nothing
/// chosen (or too much, or Telegram failing) is the screen again with why.
// The Err is the ready-to-send page; it is built once, on the failure path only.
#[allow(clippy::result_large_err)]
async fn send_allow_list(
    auth: &AuthState,
    id: &str,
    login: &PendingLogin,
    form: &ConsentForm,
) -> Result<SendAllowList, Response> {
    let progress = login.progress.lock().await;
    let Progress::LoggedIn(logged_in) = &*progress else {
        return Err(pages::error(
            StatusCode::BAD_REQUEST,
            "Not logged in",
            "Finish the Telegram login first.",
        ));
    };
    let mut pickers = login.send_pickers.lock().await;
    let picked = form.picked();
    let refused = match load_pickers(auth, login, logged_in, &mut pickers).await {
        Ok(loaded) => match loaded.resolve(&picked).await {
            Ok(list) => return Ok(list),
            Err(refused) => refused,
        },
        Err(refused) => refused,
    };
    tracing::info!("consent with send refused: no usable send allow-list");
    let send = pages::SendView {
        choices: pickers.as_ref().map(Pickers::choices),
        picked,
        error: Some(refused.message()),
        ..pages::SendView::default()
    };
    Err(consent(
        StatusCode::UNPROCESSABLE_ENTITY,
        id,
        login,
        logged_in,
        form.scopes(),
        send,
    ))
}

/// The login exists and this browser is the one that started it.
// The Err is the ready-to-send page; it is built once, on the failure path only.
#[allow(clippy::result_large_err)]
fn owned_login(
    auth: &AuthState,
    headers: &HeaderMap,
    id: &str,
) -> Result<Arc<PendingLogin>, Response> {
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
pub(crate) fn redirect_target(redirect_uri: &str) -> String {
    match Url::parse(redirect_uri) {
        Ok(url) => match url.host_str() {
            Some(host) if url.scheme() == "https" || url.scheme() == "http" => host.to_string(),
            Some(host) => format!("{}://{host}", url.scheme()),
            None => format!("{}:", url.scheme()),
        },
        Err(_) => redirect_uri.to_string(),
    }
}

/// An `http(s)` redirect to this computer. A private-use scheme
/// (`myapp://localhost/…`) keeps its name, which says more than "this computer".
pub(crate) fn redirect_is_loopback(redirect_uri: &str) -> bool {
    Url::parse(redirect_uri).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https") && clients::is_loopback_host(&url)
    })
}

/// The `tg://login` URL as the styled SVG QR code ([`super::qr`]), base64 for
/// a `data:` URL. The fake login's URL is rendered too; only an unencodable URL
/// yields `None`.
fn qr_svg_base64(qr_url: &str) -> Option<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    super::qr::styled_svg(qr_url).map(|svg| STANDARD.encode(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_redirects_are_told_apart() {
        for uri in [
            "http://localhost:33418/callback",
            "http://LOCALHOST/cb",
            "http://127.0.0.1:43123/callback",
            "http://127.1.2.3/cb",
            "http://[::1]:8080/cb",
            "https://localhost/cb",
        ] {
            assert!(redirect_is_loopback(uri), "{uri}");
        }
        for uri in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost.evil.example/cb",
            "cursor://anysphere.cursor-retrieval/oauth/callback",
            "myapp://localhost/cb",
            "not a uri",
        ] {
            assert!(!redirect_is_loopback(uri), "{uri}");
        }
    }

    #[test]
    fn redirect_targets_read_as_hosts() {
        assert_eq!(
            redirect_target("https://claude.ai/api/mcp/auth_callback"),
            "claude.ai"
        );
        assert_eq!(
            redirect_target("cursor://anysphere.cursor-retrieval/oauth/callback"),
            "cursor://anysphere.cursor-retrieval"
        );
    }

    fn posted(pairs: &[(&str, &str)]) -> ConsentForm {
        ConsentForm::from_pairs(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn consent_grants_read_plus_the_checked_offered_boxes() {
        let form = posted(&[("decision", "allow")]);
        assert_eq!(form.decision, "allow");
        assert_eq!(form.scopes(), Scopes::READ_ONLY);

        let form = posted(&[("scope", "draft"), ("decision", "allow")]);
        assert_eq!(form.scopes().to_string(), "read draft");
    }

    #[test]
    fn a_hand_made_consent_form_gets_no_more_than_the_screen_offers() {
        let form = posted(&[
            ("scope", "send"),
            ("scope", "organize"),
            ("scope", "teleport"),
            ("scope", "read"),
            ("decision", "allow"),
            ("decision", "deny"),
        ]);
        assert_eq!(form.scopes().to_string(), "read send");
        assert_eq!(form.decision, "allow", "the first decision counts");
    }

    #[test]
    fn the_send_allow_list_part_is_read_as_far_as_it_parses() {
        let form = posted(&[
            ("scope", "send"),
            ("auto_send", "on"),
            ("folder", "3"),
            ("folder", "three"),
            ("chat", "user:7~AAAAAAAAAAE"),
            ("chat", "chat:8"),
            ("chat", "user:-1"),
            ("chat", "nobody"),
            ("q", "Ann"),
            ("q", "Bob"),
            ("decision", "allow"),
        ]);
        let picked = form.picked();
        assert_eq!(picked.folders, [3]);
        assert_eq!(
            picked
                .chats
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>(),
            ["user:7~AAAAAAAAAAE", "chat:8"]
        );
        assert!(picked.auto_send);
        assert_eq!(form.query, "Ann");

        let form = posted(&[("auto_send", "yes"), ("decision", "allow")]);
        assert!(
            !form.picked().auto_send,
            "only the box's own value ticks it"
        );
        assert_eq!(form.picked(), Picked::default());
    }
}
