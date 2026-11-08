//! Refresh Token reuse detection (ADR-0003, #16), through HTTP: a consumed
//! refresh Token works for its grace window; after it, or from another client,
//! it revokes the Grant.

mod common;

use std::time::Duration;

use common::{TestServer, account, json, start_with_clock};
use reqwest::{StatusCode, header};
use serde_json::Value;
use tg_mcp::sessions::{PausedClock, REFRESH_GRACE};

/// A raw MCP `initialize` with this bearer; only the status matters. A success
/// leaves the Grant's Live client in the pool.
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

async fn refresh(server: &TestServer, client_id: &str, refresh_token: &str) -> (StatusCode, Value) {
    json(
        server
            .post_form(
                "/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("client_id", client_id),
                ],
                None,
            )
            .await,
    )
    .await
}

/// A refresh that must succeed; returns the new Token pair.
async fn rotate(server: &TestServer, client_id: &str, refresh_token: &str) -> Value {
    let (status, tokens) = refresh(server, client_id, refresh_token).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    tokens
}

/// Exactly what any invalid refresh Token gets: nothing tells a revocation apart.
fn assert_plain_invalid_grant((status, body): (StatusCode, Value)) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        serde_json::json!({
            "error": "invalid_grant",
            "error_description": "invalid, used or expired refresh_token",
        })
    );
}

fn token<'a>(tokens: &'a Value, kind: &str) -> &'a str {
    tokens[kind].as_str().unwrap()
}

/// The Token with one character of its secret changed, still well-formed.
fn forged(token: &str) -> String {
    let (id, secret) = token.split_once('.').unwrap();
    let mut chars: Vec<char> = secret.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    format!("{id}.{}", chars.into_iter().collect::<String>())
}

#[tokio::test]
async fn refreshing_twice_within_the_grace_window_gives_two_working_pairs() {
    let clock = PausedClock::new();
    let server = start_with_clock(vec![account(42, "Alice")], clock.clone()).await;
    let (client_id, first) = server.login().await;

    let second = rotate(&server, &client_id, token(&first, "refresh_token")).await;
    clock.advance(REFRESH_GRACE - Duration::from_secs(1));
    let third = rotate(&server, &client_id, token(&first, "refresh_token")).await;

    for pair in [&second, &third] {
        assert_eq!(
            mcp_status(&server, token(pair, "access_token")).await,
            StatusCode::OK
        );
        rotate(&server, &client_id, token(pair, "refresh_token")).await;
    }
    assert!(server.connector.logged_out().is_empty());
}

#[tokio::test]
async fn reuse_after_the_grace_window_revokes_the_grant_and_logs_its_live_client_out() {
    let clock = PausedClock::new();
    let server = start_with_clock(vec![account(42, "Alice")], clock.clone()).await;
    let (client_id, first) = server.login().await;
    let second = rotate(&server, &client_id, token(&first, "refresh_token")).await;
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::OK
    );
    assert_eq!(server.connector.live_clients(), 1);

    clock.advance(REFRESH_GRACE);
    assert_plain_invalid_grant(refresh(&server, &client_id, token(&first, "refresh_token")).await);

    assert_eq!(server.connector.logged_out(), vec![42]);
    assert_eq!(
        server.connector.live_clients(),
        0,
        "the Live client is evicted"
    );
    for access in [&first, &second] {
        assert_eq!(
            mcp_status(&server, token(access, "access_token")).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_plain_invalid_grant(refresh(&server, &client_id, token(&second, "refresh_token")).await);
}

#[tokio::test]
async fn reuse_without_a_live_client_revokes_the_grant_and_logs_nothing_out() {
    let clock = PausedClock::new();
    let server = start_with_clock(vec![account(42, "Alice")], clock.clone()).await;
    let (client_id, first) = server.login().await;
    let second = rotate(&server, &client_id, token(&first, "refresh_token")).await;

    clock.advance(REFRESH_GRACE);
    assert_plain_invalid_grant(refresh(&server, &client_id, token(&first, "refresh_token")).await);

    assert!(server.connector.logged_out().is_empty());
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn reuse_revokes_the_grant_even_when_telegram_is_down() {
    let clock = PausedClock::new();
    let server = start_with_clock(vec![account(42, "Alice")], clock.clone()).await;
    let (client_id, first) = server.login().await;
    let second = rotate(&server, &client_id, token(&first, "refresh_token")).await;
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::OK
    );

    server.connector.set_unreachable(true);
    clock.advance(REFRESH_GRACE);
    assert_plain_invalid_grant(refresh(&server, &client_id, token(&first, "refresh_token")).await);
    assert!(server.connector.logged_out().is_empty());
    assert_eq!(server.connector.live_clients(), 0, "evicted all the same");

    server.connector.set_unreachable(false);
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_forged_secret_for_a_consumed_token_revokes_nothing() {
    let clock = PausedClock::new();
    let server = start_with_clock(vec![account(42, "Alice")], clock.clone()).await;
    let (client_id, first) = server.login().await;
    let second = rotate(&server, &client_id, token(&first, "refresh_token")).await;
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::OK
    );

    clock.advance(REFRESH_GRACE);
    let forged = forged(token(&first, "refresh_token"));
    assert_plain_invalid_grant(refresh(&server, &client_id, &forged).await);

    assert!(server.connector.logged_out().is_empty());
    assert_eq!(
        mcp_status(&server, token(&second, "access_token")).await,
        StatusCode::OK
    );
    rotate(&server, &client_id, token(&second, "refresh_token")).await;
}

#[tokio::test]
async fn a_genuine_refresh_token_from_another_client_revokes_the_grant() {
    let server = start_with_clock(vec![account(42, "Alice")], PausedClock::new()).await;
    let (client_id, tokens) = server.login().await;
    let other_client = server.register().await;
    assert_eq!(
        mcp_status(&server, token(&tokens, "access_token")).await,
        StatusCode::OK
    );

    assert_plain_invalid_grant(
        refresh(&server, &other_client, token(&tokens, "refresh_token")).await,
    );

    assert_eq!(server.connector.logged_out(), vec![42]);
    assert_eq!(
        mcp_status(&server, token(&tokens, "access_token")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_plain_invalid_grant(refresh(&server, &client_id, token(&tokens, "refresh_token")).await);
}

#[tokio::test]
async fn reuse_revokes_only_its_own_grant() {
    let clock = PausedClock::new();
    let server =
        start_with_clock(vec![account(42, "Alice"), account(7, "Bob")], clock.clone()).await;
    let (alice_client, alice) = server.login().await;
    let (_, bob) = server.login().await;
    rotate(&server, &alice_client, token(&alice, "refresh_token")).await;
    for tokens in [&alice, &bob] {
        // Alice's first access Token is still live: only refresh Tokens are consumed.
        assert_eq!(
            mcp_status(&server, token(tokens, "access_token")).await,
            StatusCode::OK
        );
    }

    clock.advance(REFRESH_GRACE);
    assert_plain_invalid_grant(
        refresh(&server, &alice_client, token(&alice, "refresh_token")).await,
    );

    assert_eq!(server.connector.logged_out(), vec![42]);
    assert_eq!(server.connector.live_clients(), 1);
    assert_eq!(
        mcp_status(&server, token(&bob, "access_token")).await,
        StatusCode::OK
    );
}
