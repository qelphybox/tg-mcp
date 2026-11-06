//! Disconnect: RFC 7009 revocation and Telegram-side session ends, through HTTP.

mod common;

use common::{TestServer, account, json, start};
use reqwest::{StatusCode, header};
use serde_json::Value;

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

async fn revoke(server: &TestServer, token: &str, client_id: &str) -> reqwest::Response {
    server
        .post_form(
            "/revoke",
            &[
                ("token", token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ],
            None,
        )
        .await
}

fn token<'a>(tokens: &'a Value, kind: &str) -> &'a str {
    tokens[kind].as_str().unwrap()
}

#[tokio::test]
async fn metadata_advertises_the_revocation_endpoint() {
    let server = start(vec![account(42, "Alice")]).await;
    let (_, meta) = json(
        server
            .get("/.well-known/oauth-authorization-server", None)
            .await,
    )
    .await;
    assert_eq!(meta["revocation_endpoint"], server.url("/revoke"));
    assert_eq!(
        meta["revocation_endpoint_auth_methods_supported"],
        serde_json::json!(["none"])
    );
}

#[tokio::test]
async fn revoking_either_token_ends_the_whole_grant() {
    for kind in ["refresh_token", "access_token"] {
        let server = start(vec![account(42, "Alice")]).await;
        let (client_id, tokens) = server.login().await;
        assert_eq!(
            mcp_status(&server, token(&tokens, "access_token")).await,
            StatusCode::OK
        );

        let res = revoke(&server, token(&tokens, kind), &client_id).await;
        assert_eq!(res.status(), StatusCode::OK, "revoking with {kind}");
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");

        assert_eq!(server.connector.logged_out(), vec![42], "{kind}");
        assert_eq!(
            mcp_status(&server, token(&tokens, "access_token")).await,
            StatusCode::UNAUTHORIZED,
            "old access Token after revoking with {kind}"
        );
        let (status, body) = refresh(&server, &client_id, token(&tokens, "refresh_token")).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "old refresh Token after revoking with {kind}"
        );
    }
}

#[tokio::test]
async fn revoking_one_grant_leaves_the_others_alone() {
    let server = start(vec![account(42, "Alice"), account(7, "Bob")]).await;
    let (alice_client, alice) = server.login().await;
    let (_, bob) = server.login().await;

    let res = revoke(&server, token(&alice, "refresh_token"), &alice_client).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(server.connector.logged_out(), vec![42]);
    assert_eq!(
        mcp_status(&server, token(&bob, "access_token")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn unknown_and_already_revoked_tokens_are_a_quiet_success() {
    let server = start(vec![account(42, "Alice")]).await;
    let (client_id, tokens) = server.login().await;

    for garbage in ["", "garbage", "tgm_AAAAAAAAAAAAAAAAAAAAAA.AAAA"] {
        let res = revoke(&server, garbage, &client_id).await;
        assert_eq!(res.status(), StatusCode::OK, "{garbage:?}");
    }
    assert!(server.connector.logged_out().is_empty());

    for _ in 0..2 {
        let res = revoke(&server, token(&tokens, "refresh_token"), &client_id).await;
        assert_eq!(res.status(), StatusCode::OK);
    }
    assert_eq!(
        server.connector.logged_out(),
        vec![42],
        "a second revocation finds nothing to log out"
    );
}

#[tokio::test]
async fn a_token_of_another_client_is_refused_and_keeps_working() {
    let server = start(vec![account(42, "Alice")]).await;
    let (_, tokens) = server.login().await;
    let other_client = server.register().await;

    let (status, body) =
        json(revoke(&server, token(&tokens, "refresh_token"), &other_client).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unauthorized_client");
    assert!(server.connector.logged_out().is_empty());
    assert_eq!(
        mcp_status(&server, token(&tokens, "access_token")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn token_and_client_id_are_required() {
    let server = start(vec![account(42, "Alice")]).await;
    let (client_id, tokens) = server.login().await;
    for pairs in [
        vec![("client_id", client_id.as_str())],
        vec![("token", token(&tokens, "refresh_token"))],
    ] {
        let (status, body) = json(server.post_form("/revoke", &pairs, None).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }
    assert!(server.connector.logged_out().is_empty());
}

#[tokio::test]
async fn grant_is_deleted_even_when_telegram_is_down() {
    let server = start(vec![account(42, "Alice")]).await;
    let (client_id, tokens) = server.login().await;

    server.connector.set_unreachable(true);
    let res = revoke(&server, token(&tokens, "refresh_token"), &client_id).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(server.connector.logged_out().is_empty());

    server.connector.set_unreachable(false);
    assert_eq!(
        mcp_status(&server, token(&tokens, "access_token")).await,
        StatusCode::UNAUTHORIZED
    );
    let (_, body) = refresh(&server, &client_id, token(&tokens, "refresh_token")).await;
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn session_ended_in_telegram_deletes_the_grant() {
    let server = start(vec![account(42, "Alice")]).await;
    let (client_id, tokens) = server.login().await;

    server.connector.end_session_in_telegram(42);
    let res = server
        .http
        .post(server.url("/mcp"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token(&tokens, "access_token")),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(
        res.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains(r#"error="invalid_token""#)
    );

    // The Grant is gone, not just unusable: its refresh Token no longer refreshes.
    let (_, body) = refresh(&server, &client_id, token(&tokens, "refresh_token")).await;
    assert_eq!(body["error"], "invalid_grant");
    assert!(
        server.connector.logged_out().is_empty(),
        "nothing to log out"
    );
}
