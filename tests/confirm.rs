//! The Confirmation cookie and the confirmation page through the whole
//! server (#45): Consent that grants `send` sets the cookie in its browser,
//! bound to the new Grant by a verifier sealed with its Send allow-list; a
//! Grant without `send` gets none; `/confirm/{id}` is served, locked down,
//! and says "no longer valid" to ids it doesn't know.

mod common;

use common::{account, json, location, pkce, query, start_with};
use reqwest::{StatusCode, header};
use tg_mcp::{
    confirm::cookie,
    tg::{
        chats::{ChatId, ChatKind, ChatSummary},
        fake::{FakeData, fake_issued_id},
    },
};

const ALICE: ChatId = ChatId::User(101);

fn data() -> FakeData {
    FakeData::new(account(42, "Me")).with_chats(vec![ChatSummary {
        id: ALICE.to_string(),
        title: "Alice".into(),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: None,
    }])
}

/// Consent posted as `consent`: the redirect's `Set-Cookie` headers, the
/// token response and the client id.
async fn consent(
    server: &common::TestServer,
    consent: &[(&str, &str)],
) -> (Vec<String>, serde_json::Value, String) {
    let client_id = server.register().await;
    let pkce = pkce();
    let (login_path, login_cookie) = server.start_authorize(&client_id, &pkce, "xyz").await;
    let consent_path = server.wait_logged_in(&login_path, &login_cookie).await;
    // Load the chats the way the screen's button does.
    let loaded = server
        .post_form(
            &format!("{consent_path}/chats"),
            &[("scope", "send")],
            Some(&login_cookie),
        )
        .await;
    assert_eq!(loaded.status(), StatusCode::OK);
    let res = server
        .post_form(&consent_path, consent, Some(&login_cookie))
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookies = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let code = query(&location(&res), "code").expect("code in redirect");
    let (status, tokens) = json(server.exchange(&client_id, &code, &pkce.verifier).await).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    (cookies, tokens, client_id)
}

#[tokio::test]
async fn consent_with_send_sets_the_confirmation_cookie_bound_to_the_grant() {
    let server = start_with(vec![data()]).await;
    let alice = fake_issued_id(ALICE);
    let (cookies, tokens, client_id) = consent(
        &server,
        &[("scope", "send"), ("chat", &alice), ("decision", "allow")],
    )
    .await;
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    let set = &cookies[0];
    assert!(set.starts_with(cookie::PREFIX), "{set}");
    for part in [
        "; Path=/confirm",
        "; HttpOnly",
        "; SameSite=Strict",
        // The refresh Token's lifetime: 30 days.
        "; Max-Age=2592000",
    ] {
        assert!(set.contains(part), "{part}: {set}");
    }
    // A plain-http test server; production (https) adds Secure.
    assert!(!set.contains("Secure"), "{set}");

    let value = set.split(';').next().unwrap().split_once('=').unwrap().1;
    let grant = server
        .vault
        .open(tokens["access_token"].as_str().unwrap())
        .await
        .unwrap();
    let list = grant.send_allow_list.expect("a send Grant has a list");
    assert_eq!(
        list.confirm_cookie(),
        Some(&cookie::verifier(value)),
        "the Grant keeps the cookie's verifier, sealed with its list"
    );
    assert_ne!(
        list.confirm_cookie().unwrap().as_slice(),
        value.as_bytes(),
        "never the cookie itself"
    );

    // Refresh rotates Tokens; the binding is to the Grant, not a Token.
    let (status, rotated) = json(
        server
            .post_form(
                "/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                    ("client_id", &client_id),
                ],
                None,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let rotated = server
        .vault
        .open(rotated["access_token"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        rotated.send_allow_list.unwrap().confirm_cookie(),
        Some(&cookie::verifier(value))
    );
}

#[tokio::test]
async fn consent_without_send_sets_no_cookie() {
    let server = start_with(vec![data()]).await;
    let (cookies, tokens, _) = consent(&server, &[("scope", "draft"), ("decision", "allow")]).await;
    assert!(cookies.is_empty(), "{cookies:?}");
    assert_eq!(tokens["scope"], "read draft");
}

#[tokio::test]
async fn an_unknown_confirmation_is_no_longer_valid_on_a_locked_down_page() {
    let server = start_with(vec![data()]).await;
    let res = server
        .http
        .get(server.url("/confirm/garbage"))
        .header(header::ORIGIN, "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
    let csp = res.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.starts_with("default-src 'none';"), "{csp}");
    assert!(!csp.contains("script-src"), "no script at all: {csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    // Not readable from another origin: no CORS headers here.
    assert!(
        !res.headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
    let html = res.text().await.unwrap();
    assert!(html.contains("no longer valid"), "{html}");

    let res = server
        .post_form(
            "/confirm/garbage",
            &[("csrf", "x"), ("decision", "send")],
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::GONE);
}
