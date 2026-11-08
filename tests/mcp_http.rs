//! `/mcp` as an MCP client sees it: discovery, then authenticated tool calls.

mod common;

use common::{account, json, start};
use reqwest::{StatusCode, header};
use rmcp::model::CallToolRequestParams;
use serde_json::json;

#[tokio::test]
async fn healthz_is_ok() {
    let server = start(vec![account(42, "Alice")]).await;
    assert_eq!(server.get("/healthz", None).await.status(), StatusCode::OK);
}

/// The path `claude mcp add` takes before it ever shows a browser.
#[tokio::test]
async fn unauthenticated_mcp_points_to_oauth_discovery() {
    let server = start(vec![account(42, "Alice")]).await;
    let res = server
        .http
        .post(server.url("/mcp"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let challenge = res.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .to_string();
    let metadata_url = challenge
        .split("resource_metadata=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("resource_metadata in WWW-Authenticate");
    assert_eq!(
        metadata_url,
        server.url("/.well-known/oauth-protected-resource/mcp")
    );

    let (_, resource) = json(server.http.get(metadata_url).send().await.unwrap()).await;
    assert_eq!(resource["resource"], server.url("/mcp"));
    let issuer = resource["authorization_servers"][0].as_str().unwrap();
    assert_eq!(issuer, server.base);

    let (_, as_meta) = json(
        server
            .get("/.well-known/oauth-authorization-server", None)
            .await,
    )
    .await;
    assert_eq!(as_meta["issuer"], server.base);
    assert_eq!(as_meta["registration_endpoint"], server.url("/register"));
    assert_eq!(as_meta["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        as_meta["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
}

#[tokio::test]
async fn invalid_token_is_rejected_with_invalid_token() {
    let server = start(vec![account(42, "Alice")]).await;
    let res = server
        .http
        .post(server.url("/mcp"))
        .header(
            header::AUTHORIZATION,
            "Bearer tgm_AAAAAAAAAAAAAAAAAAAAAA.nope",
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let challenge = res.headers()[header::WWW_AUTHENTICATE].to_str().unwrap();
    assert!(
        challenge.contains(r#"error="invalid_token""#),
        "{challenge}"
    );
}

#[tokio::test]
async fn whoami_answers_for_the_callers_own_grant() {
    let server = start(vec![account(42, "Alice"), account(7, "Bob")]).await;
    let (_, alice_tokens) = server.login().await;
    let (_, bob_tokens) = server.login().await;

    for (tokens, expected) in [
        (
            &alice_tokens,
            json!({"id": 42, "first_name": "Alice", "username": "alice", "chat_folders": []}),
        ),
        (
            &bob_tokens,
            json!({"id": 7, "first_name": "Bob", "username": "bob", "chat_folders": []}),
        ),
    ] {
        let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "whoami"));
        // The spec: every tool a `read` Grant (the default) sees is read-only.
        for tool in &tools {
            assert_eq!(
                tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                Some(true),
                "{} is not marked read-only",
                tool.name
            );
        }

        let result = client
            .call_tool(CallToolRequestParams::new("whoami"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(expected));
        client.cancel().await.unwrap();
    }
}
