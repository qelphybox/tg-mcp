//! `/forget` (#63): the user proves they own the Account with a QR login and
//! deletes every Grant of it, without a Token (ADR-0003: deleting destroys,
//! never decrypts). Exactly that Account's Grants go, another's survive;
//! Cancel deletes nothing; the session made for the check is logged out
//! either way; the login cookie binds every step to its browser.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use common::{SequenceLogin, TestServer, account, start, start_with_env};
use reqwest::{StatusCode, header};

const ME: i64 = 42;
const OTHER: i64 = 77;

/// A raw MCP `initialize` with this bearer; only the status matters.
async fn mcp_status(server: &TestServer, access_token: &str) -> StatusCode {
    server
        .http
        .post(server.url("/mcp"))
        .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        )
        .send()
        .await
        .unwrap()
        .status()
}

/// The access Token of a fresh Grant for the next Account of the server's
/// login sequence.
async fn connect(server: &TestServer) -> String {
    let (_, tokens) = server.login().await;
    tokens["access_token"].as_str().unwrap().to_string()
}

/// POST /forget, then the QR login: the forget page's path and the login cookie.
async fn start_forget(server: &TestServer) -> (String, String) {
    let page = server.get("/forget", None).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(
        html.contains(r#"<form method="post" action="/forget">"#),
        "{html}"
    );
    assert!(html.contains("Log in with Telegram</button>"), "{html}");

    let res = server.post_form("/forget", &[], None).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookie = res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("tgm_login="), "{cookie}");
    let login_path = res.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    assert!(login_path.starts_with("/login/"), "{login_path}");
    // The QR page as for /authorize; once scanned, on to the forget page.
    let next = server.wait_logged_in(&login_path, &cookie).await;
    let res = server.get(&next, Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "{next}");
    let forget_path = res.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(forget_path, format!("{login_path}/forget"));
    (forget_path, cookie)
}

async fn decide(
    server: &TestServer,
    path: &str,
    cookie: &str,
    decision: &str,
) -> (StatusCode, String) {
    let res = server
        .post_form(path, &[("decision", decision)], Some(cookie))
        .await;
    (res.status(), res.text().await.unwrap())
}

#[tokio::test]
async fn delete_removes_exactly_that_accounts_grants_and_logs_the_check_out() {
    let logs = captured_logs();
    // Logins in turn: Me, Other, Me, then Me for /forget.
    let server = common::start_with_login(Arc::new(SequenceLogin::new(vec![
        account(ME, "Me"),
        account(OTHER, "Other"),
        account(ME, "Me"),
        account(ME, "Me"),
    ])))
    .await;
    let first = connect(&server).await;
    let other = connect(&server).await;
    let mine = [first, connect(&server).await];
    for token in mine.iter().chain([&other]) {
        assert_eq!(mcp_status(&server, token).await, StatusCode::OK);
    }
    assert_eq!(server.connector.live_clients(), 3);

    let (path, cookie) = start_forget(&server).await;
    let page = server.get(&path, Some(&cookie)).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(html.contains("2 connections"), "{html}");
    assert!(html.contains("Test Client"), "the apps, by name: {html}");
    assert!(html.contains("not verified"), "{html}");
    assert!(html.contains(r#"value="delete""#), "{html}");
    assert!(html.contains(r#"value="cancel""#), "{html}");
    // Nothing is deleted before the click.
    for token in &mine {
        assert_eq!(mcp_status(&server, token).await, StatusCode::OK);
    }

    let (status, html) = decide(&server, &path, &cookie, "delete").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Deleted 2 connections"), "{html}");
    assert!(html.contains("Settings → Devices"), "{html}");
    for token in &mine {
        assert_eq!(mcp_status(&server, token).await, StatusCode::UNAUTHORIZED);
        assert!(server.vault.open(token).await.is_err());
    }
    assert_eq!(mcp_status(&server, &other).await, StatusCode::OK);
    // The pooled Live clients of Me's Grants were logged out (best effort),
    // and so was the session made for this check.
    assert_eq!(server.connector.logged_out(), vec![ME, ME]);
    assert_eq!(server.connector.logins_logged_out(), vec![ME]);
    assert_eq!(server.connector.live_clients(), 1);

    // The login is used up.
    let (status, _) = decide(&server, &path, &cookie, "delete").await;
    assert_eq!(status, StatusCode::GONE);

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(logs.contains("forget: grants deleted"), "{logs}");
    assert!(logs.contains("grants=2"), "{logs}");
    for token in mine.iter().chain([&other]) {
        assert!(!logs.contains(token.as_str()), "no Token in the logs");
    }
}

#[tokio::test]
async fn cancel_deletes_nothing_and_logs_the_check_out() {
    let server = start(vec![account(ME, "Me")]).await;
    let token = connect(&server).await;
    let (path, cookie) = start_forget(&server).await;
    let html = server.get(&path, Some(&cookie)).await.text().await.unwrap();
    assert!(html.contains("1 connection"), "{html}");

    let (status, html) = decide(&server, &path, &cookie, "cancel").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Nothing was deleted"), "{html}");
    assert_eq!(mcp_status(&server, &token).await, StatusCode::OK);
    assert!(server.connector.logged_out().is_empty());
    assert_eq!(server.connector.logins_logged_out(), vec![ME]);
}

#[tokio::test]
async fn an_account_with_nothing_to_delete_is_told_so() {
    let server = start(vec![account(ME, "Me")]).await;
    let (path, cookie) = start_forget(&server).await;
    let html = server.get(&path, Some(&cookie)).await.text().await.unwrap();
    assert!(html.contains("no connections"), "{html}");
    assert!(!html.contains(r#"value="delete""#), "{html}");
    let (status, _) = decide(&server, &path, &cookie, "cancel").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(server.connector.logins_logged_out(), vec![ME]);
}

#[tokio::test]
async fn every_step_is_bound_to_the_browser_and_to_its_purpose() {
    let server = start(vec![account(ME, "Me")]).await;
    let token = connect(&server).await;
    let (path, cookie) = start_forget(&server).await;

    // Another browser (no cookie, or another login's) can't see or decide.
    let res = server.get(&path, None).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    for wrong in [None, Some("tgm_login=someone-else")] {
        let res = server
            .post_form(&path, &[("decision", "delete")], wrong)
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    // A forget login can't be turned into an authorization code.
    let consent = path.replace("/forget", "/consent");
    let res = server
        .post_form(&consent, &[("decision", "allow")], Some(&cookie))
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mcp_status(&server, &token).await, StatusCode::OK);

    // Nor an OAuth login into a forget page.
    let client_id = server.register().await;
    let (login_path, oauth_cookie) = server
        .start_authorize(&client_id, &common::pkce(), "xyz")
        .await;
    server.wait_logged_in(&login_path, &oauth_cookie).await;
    let res = server
        .get(&format!("{login_path}/forget"), Some(&oauth_cookie))
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = server
        .post_form(
            &format!("{login_path}/forget"),
            &[("decision", "delete")],
            Some(&oauth_cookie),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mcp_status(&server, &token).await, StatusCode::OK);

    // An unknown decision deletes nothing.
    let (status, _) = decide(&server, &path, &cookie, "maybe").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(mcp_status(&server, &token).await, StatusCode::OK);
}

#[tokio::test]
async fn starting_is_rate_limited_per_client_ip_like_authorize() {
    let server = start_with_env(
        Arc::new(SequenceLogin::new(vec![account(ME, "Me")])),
        &[
            ("TG_MCP_AUTHORIZE_PER_MINUTE", "1"),
            ("TG_MCP_AUTHORIZE_BURST", "1"),
        ],
    )
    .await;
    let first = server.post_form("/forget", &[], None).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);
    let second = server.post_form("/forget", &[], None).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().contains_key(header::RETRY_AFTER));
    // Its own bucket: /authorize is not used up by /forget.
    let client_id = server.register().await;
    let res = server
        .get(
            &server.authorize_path(&client_id, &common::pkce(), "xyz"),
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_consent_screen_links_to_it() {
    let server = start(vec![account(ME, "Me")]).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server
        .start_authorize(&client_id, &common::pkce(), "xyz")
        .await;
    let consent = server.wait_logged_in(&login_path, &cookie).await;
    let html = server
        .get(&consent, Some(&cookie))
        .await
        .text()
        .await
        .unwrap();
    assert!(html.contains(r#"<a href="/forget">"#), "{html}");
}

/// Everything the server logs while this test binary runs.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        tracing_subscriber::fmt()
            .with_env_filter("info,tg_mcp=trace")
            .with_ansi(false)
            .with_writer(move || Sink(sink.clone()))
            .init();
        logs
    })
    .clone()
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
