//! The OAuth 2.1 authorization server, driven like a browser and a client would.

mod common;

use common::{REDIRECT, account, json, location, pkce, query, start};
use reqwest::{StatusCode, header};
use serde_json::json;

#[tokio::test]
async fn registration_enforces_the_redirect_uri_policy() {
    let server = start(vec![account(42, "Alice")]).await;
    for ok in [
        json!(["http://localhost:33418/callback"]),
        json!(["https://claude.ai/api/mcp/auth_callback"]),
        json!(["cursor://anysphere.cursor-retrieval/oauth/callback"]),
    ] {
        let (status, body) = json(
            server
                .post_json("/register", json!({ "redirect_uris": ok }))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(body["client_id"].as_str().unwrap().starts_with("tgmc_"));
        assert_eq!(body["token_endpoint_auth_method"], "none");
    }
    for bad in [
        json!([]),
        json!(["http://evil.example/cb"]),
        json!(["javascript:alert(1)"]),
        json!(["https://ok.example/cb#frag"]),
    ] {
        let (status, body) = json(
            server
                .post_json("/register", json!({ "redirect_uris": bad }))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {bad}");
        assert_eq!(body["error"], "invalid_redirect_uri");
    }
    let (status, body) = json(
        server
            .post_json(
                "/register",
                json!({ "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic" }),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

#[tokio::test]
async fn authorize_never_redirects_to_an_unverified_uri() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let p = pkce();
    for query_string in [
        format!("client_id=nope&redirect_uri={REDIRECT}&response_type=code"),
        format!("client_id={client_id}&redirect_uri=https://evil.example/cb&response_type=code"),
        format!("client_id={client_id}&response_type=code"),
    ] {
        let res = server
            .get(
                &format!(
                    "/authorize?{query_string}&code_challenge={}&code_challenge_method=S256",
                    p.challenge
                ),
                None,
            )
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{query_string}");
        assert!(res.headers().get(header::LOCATION).is_none());
    }
}

#[tokio::test]
async fn authorize_requires_pkce_s256_and_our_resource() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let p = pkce();
    let base = format!(
        "/authorize?client_id={client_id}&redirect_uri={REDIRECT}&response_type=code&state=s1"
    );
    for (extra, error) in [
        (String::new(), "invalid_request"),
        (
            format!(
                "&code_challenge={}&code_challenge_method=plain",
                p.challenge
            ),
            "invalid_request",
        ),
        (
            format!(
                "&code_challenge={}&code_challenge_method=S256&resource=https://other.example/mcp",
                p.challenge
            ),
            "invalid_target",
        ),
    ] {
        let res = server.get(&format!("{base}{extra}"), None).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{extra}");
        let to = location(&res);
        assert!(to.as_str().starts_with(REDIRECT));
        assert_eq!(query(&to, "error").as_deref(), Some(error), "{extra}");
        assert_eq!(query(&to, "state").as_deref(), Some("s1"));
    }
}

#[tokio::test]
async fn loopback_redirect_may_use_another_port() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let p = pkce();
    let other_port = REDIRECT.replace("43123", "50999");
    let res = server
        .get(
            &format!(
                "/authorize?client_id={client_id}&redirect_uri={other_port}&response_type=code&code_challenge={}&code_challenge_method=S256",
                p.challenge
            ),
            None,
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .starts_with("/login/")
    );
}

#[tokio::test]
async fn login_pages_belong_to_the_browser_that_started_them() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "s").await;

    assert_eq!(
        server.get(&login_path, Some(&cookie)).await.status(),
        StatusCode::OK
    );
    for wrong in [None, Some("tgm_login=someone-else")] {
        assert_eq!(
            server.get(&login_path, wrong).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            server
                .get(&format!("{login_path}/status"), wrong)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let res = server
            .post_form(
                &format!("{login_path}/consent"),
                &[("decision", "allow")],
                wrong,
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn consent_page_shows_who_gets_what_and_cannot_be_framed() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "s").await;
    let consent_path = server.wait_logged_in(&login_path, &cookie).await;

    let res = server.get(&consent_path, Some(&cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let csp = res.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    let html = res.text().await.unwrap();
    for needle in [
        // The test client redirects to 127.0.0.1: a loopback host leads.
        r#"<h1 class="ask">Allow <b>an app on this computer (localhost)</b> <span class="read-only">read-only </span>access to all chats of <b>Alice</b> (@alice)?</h1>"#,
        r#"<p class="muted">The app calls itself “Test Client” (not verified).</p>"#,
        "Settings → Devices",
        "in the app alone does not end that session",
    ] {
        assert!(html.contains(needle), "consent page lacks {needle:?}");
    }
}

#[tokio::test]
async fn consent_page_shows_a_hostile_client_name_escaped_and_cleaned() {
    let server = start(vec![account(42, "Alice")]).await;
    let (status, body) = json(
        server
            .post_json(
                "/register",
                json!({
                    "client_name": "<b>Claude</b>\u{202E}\u{200B}evil",
                    "redirect_uris": [REDIRECT],
                }),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let client_id = body["client_id"].as_str().unwrap();
    let (login_path, cookie) = server.start_authorize(client_id, &pkce(), "s").await;
    let consent_path = server.wait_logged_in(&login_path, &cookie).await;

    let html = server
        .get(&consent_path, Some(&cookie))
        .await
        .text()
        .await
        .unwrap();
    assert!(
        html.contains(
            r#"<p class="muted">The app calls itself “&lt;b&gt;Claude&lt;/b&gt;evil” (not verified).</p>"#
        ),
        "{html}"
    );
    assert!(!html.contains("<b>Claude</b>"), "{html}");
}

#[tokio::test]
async fn denying_consent_returns_access_denied() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "keep-me").await;
    let consent_path = server.wait_logged_in(&login_path, &cookie).await;

    let res = server
        .post_form(&consent_path, &[("decision", "deny")], Some(&cookie))
        .await;
    let to = location(&res);
    assert_eq!(query(&to, "error").as_deref(), Some("access_denied"));
    assert_eq!(query(&to, "state").as_deref(), Some("keep-me"));
    assert!(query(&to, "code").is_none());
    // The login is gone: no second chance with the same page.
    let again = server
        .post_form(&consent_path, &[("decision", "allow")], Some(&cookie))
        .await;
    assert_eq!(again.status(), StatusCode::GONE);
}

#[tokio::test]
async fn codes_are_bound_single_use_and_need_the_verifier() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let other_client = server.register().await;

    // Wrong verifier, wrong client, wrong redirect_uri: each burns its own code.
    let p = pkce();
    let code = server.authorize(&client_id, &p).await;
    let (status, body) = json(server.exchange(&client_id, &code, &pkce().verifier).await).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (_, body) = json(server.exchange(&client_id, &code, &p.verifier).await).await;
    assert_eq!(
        body["error"], "invalid_grant",
        "a failed exchange must burn the code"
    );

    let p = pkce();
    let code = server.authorize(&client_id, &p).await;
    let (_, body) = json(server.exchange(&other_client, &code, &p.verifier).await).await;
    assert_eq!(body["error"], "invalid_grant");

    let p = pkce();
    let code = server.authorize(&client_id, &p).await;
    let res = server
        .post_form(
            "/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &client_id),
                ("redirect_uri", "http://127.0.0.1:43123/elsewhere"),
                ("code_verifier", &p.verifier),
            ],
            None,
        )
        .await;
    assert_eq!(json(res).await.1["error"], "invalid_grant");

    // The happy path, then a replay.
    let p = pkce();
    let code = server.authorize(&client_id, &p).await;
    let res = server.exchange(&client_id, &code, &p.verifier).await;
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    let (status, tokens) = json(res).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    assert!(tokens["access_token"].as_str().unwrap().starts_with("tgm_"));
    assert!(tokens["expires_in"].as_u64().unwrap() > 0);
    let (_, replay) = json(server.exchange(&client_id, &code, &p.verifier).await).await;
    assert_eq!(replay["error"], "invalid_grant");
}

#[tokio::test]
async fn refresh_rotates_and_is_bound_to_the_client() {
    let server = start(vec![account(42, "Alice")]).await;
    let (client_id, tokens) = server.login().await;
    let other_client = server.register().await;
    let refresh = tokens["refresh_token"].as_str().unwrap();

    let refresh_with = |client: String, token: String| {
        let server = &server;
        async move {
            json(
                server
                    .post_form(
                        "/token",
                        &[
                            ("grant_type", "refresh_token"),
                            ("refresh_token", &token),
                            ("client_id", &client),
                        ],
                        None,
                    )
                    .await,
            )
            .await
        }
    };

    let (status, rotated) = refresh_with(client_id.clone(), refresh.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert_ne!(rotated["refresh_token"], tokens["refresh_token"]);
    // A retry right away (a lost response) still works: the grace window.
    let (status, retried) = refresh_with(client_id.clone(), refresh.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_ne!(retried["refresh_token"], rotated["refresh_token"]);

    let client = server.mcp(rotated["access_token"].as_str().unwrap()).await;
    let whoami = client
        .call_tool(rmcp::model::CallToolRequestParams::new("whoami"))
        .await
        .unwrap();
    assert_eq!(whoami.structured_content.unwrap()["id"], 42);

    // Another client presenting it is theft: the Grant ends (tests/refresh_reuse.rs).
    let (_, body) = refresh_with(
        other_client,
        rotated["refresh_token"].as_str().unwrap().to_string(),
    )
    .await;
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn unknown_grant_type_is_rejected() {
    let server = start(vec![account(42, "Alice")]).await;
    let res = server
        .post_form("/token", &[("grant_type", "password")], None)
        .await;
    assert_eq!(json(res).await.1["error"], "unsupported_grant_type");
}
