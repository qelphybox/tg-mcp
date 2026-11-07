//! Limits on what anonymous callers can make the server do: per-client rate limits
//! on the OAuth endpoints and caps on pending Login flows. Bucket arithmetic and
//! `X-Forwarded-For` parsing are unit-tested in src/auth/rate_limit.rs.

mod common;

use std::sync::Arc;

use common::{REDIRECT, SequenceLogin, TestServer, account, json, location, pkce, query};
use reqwest::{Response, StatusCode, header};

const TRUST_PROXY: (&str, &str) = ("TG_MCP_TRUST_FORWARDED_FOR", "1");

async fn server(env: &[(&str, &str)]) -> (TestServer, Arc<SequenceLogin>) {
    let login = Arc::new(SequenceLogin::new(vec![account(42, "Alice")]));
    (common::start_with_env(login.clone(), env).await, login)
}

/// `GET /authorize` as relayed by a proxy that sent `X-Forwarded-For: xff`.
async fn authorize(server: &TestServer, client_id: &str, xff: &str) -> Response {
    server
        .http
        .get(server.url(&server.authorize_path(client_id, &pkce(), "s")))
        .header("x-forwarded-for", xff)
        .send()
        .await
        .unwrap()
}

fn retry_after(res: &Response) -> u64 {
    res.headers()
        .get(header::RETRY_AFTER)
        .expect("Retry-After on a 429")
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn hammering_authorize_from_one_ip_gets_429_and_other_ips_are_unaffected() {
    let (server, _) = server(&[
        TRUST_PROXY,
        ("TG_MCP_AUTHORIZE_PER_MINUTE", "1"),
        ("TG_MCP_AUTHORIZE_BURST", "2"),
    ])
    .await;
    let client_id = server.register().await;

    // The client sent its own XFF; the proxy appended the address it saw.
    for _ in 0..2 {
        let res = authorize(&server, &client_id, "198.51.100.1, 203.0.113.7").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }
    let res = authorize(&server, &client_id, "198.51.100.2, 203.0.113.7").await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait = retry_after(&res);
    assert!((1..=60).contains(&wait), "Retry-After: {wait}");

    // A forged left-hand entry doesn't make it another client.
    let res = authorize(&server, &client_id, "203.0.113.8, 203.0.113.7").await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);

    let res = authorize(&server, &client_id, "203.0.113.8").await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn without_trust_forwarded_for_is_ignored() {
    let (server, _) = server(&[
        ("TG_MCP_AUTHORIZE_PER_MINUTE", "1"),
        ("TG_MCP_AUTHORIZE_BURST", "2"),
    ])
    .await;
    let client_id = server.register().await;
    let statuses = [
        authorize(&server, &client_id, "203.0.113.1").await.status(),
        authorize(&server, &client_id, "203.0.113.2").await.status(),
        authorize(&server, &client_id, "203.0.113.3").await.status(),
    ];
    assert_eq!(
        statuses,
        [
            StatusCode::SEE_OTHER,
            StatusCode::SEE_OTHER,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[tokio::test]
async fn register_token_and_revoke_are_limited() {
    let (server, _) = server(&[
        ("TG_MCP_REGISTER_PER_MINUTE", "1"),
        ("TG_MCP_REGISTER_BURST", "1"),
        ("TG_MCP_TOKEN_PER_MINUTE", "1"),
        ("TG_MCP_TOKEN_BURST", "1"),
    ])
    .await;
    let client_id = server.register().await;
    let res = server
        .post_json(
            "/register",
            serde_json::json!({ "redirect_uris": [REDIRECT] }),
        )
        .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(retry_after(&res) >= 1);
    let (_, body) = json(res).await;
    assert_eq!(body["error"], "too_many_requests");

    let bad_code = [
        ("grant_type", "authorization_code"),
        ("code", "nope"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", REDIRECT),
        ("code_verifier", "x"),
    ];
    let res = server.post_form("/token", &bad_code, None).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = server.post_form("/token", &bad_code, None).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(retry_after(&res) >= 1);

    // /revoke has its own bucket.
    let revoke = [("token", "garbage"), ("client_id", client_id.as_str())];
    let res = server.post_form("/revoke", &revoke, None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = server.post_form("/revoke", &revoke, None).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_server_wide_login_cap_refuses_without_starting_a_telegram_login() {
    let (server, login) = server(&[
        ("TG_MCP_MAX_PENDING_LOGINS", "2"),
        ("TG_MCP_MAX_PENDING_LOGINS_PER_IP", "10"),
    ])
    .await;
    let client_id = server.register().await;
    server.start_authorize(&client_id, &pkce(), "a").await;
    server.start_authorize(&client_id, &pkce(), "b").await;

    let res = authorize(&server, &client_id, "203.0.113.7").await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let redirect = location(&res);
    assert!(redirect.as_str().starts_with(REDIRECT));
    assert_eq!(
        query(&redirect, "error").as_deref(),
        Some("temporarily_unavailable")
    );
    assert_eq!(login.started(), 2, "Telegram must not be asked for a third");
}

#[tokio::test]
async fn the_per_ip_login_cap_leaves_room_for_other_ips() {
    let (server, login) = server(&[TRUST_PROXY, ("TG_MCP_MAX_PENDING_LOGINS_PER_IP", "2")]).await;
    let client_id = server.register().await;
    for _ in 0..2 {
        let res = authorize(&server, &client_id, "203.0.113.7").await;
        assert!(
            res.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .starts_with("/login/")
        );
    }
    let res = authorize(&server, &client_id, "203.0.113.7").await;
    assert_eq!(
        query(&location(&res), "error").as_deref(),
        Some("temporarily_unavailable")
    );

    let res = authorize(&server, &client_id, "203.0.113.8").await;
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .starts_with("/login/")
    );
    assert_eq!(login.started(), 3);
}

#[tokio::test]
async fn health_mcp_metadata_and_login_pages_are_not_limited() {
    let one_per_minute = [
        ("TG_MCP_AUTHORIZE_PER_MINUTE", "1"),
        ("TG_MCP_AUTHORIZE_BURST", "1"),
        ("TG_MCP_REGISTER_PER_MINUTE", "1"),
        ("TG_MCP_REGISTER_BURST", "1"),
        ("TG_MCP_TOKEN_PER_MINUTE", "1"),
        ("TG_MCP_TOKEN_BURST", "1"),
    ];
    let (server, _) = server(&one_per_minute).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "s").await;

    for _ in 0..5 {
        for path in [
            "/healthz",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
            "/login/assets/login.js",
        ] {
            assert_eq!(
                server.get(path, None).await.status(),
                StatusCode::OK,
                "{path}"
            );
        }
        let res = server.get(&login_path, Some(&cookie)).await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = server
            .get(&format!("{login_path}/status"), Some(&cookie))
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = server
            .post_json("/mcp", serde_json::json!({ "jsonrpc": "2.0" }))
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
