//! Scopes (ADR-0005): asked for at `/authorize`, chosen on Consent, carried by
//! the Grant through its Tokens.

mod common;

use common::{REDIRECT, account, form, json, location, pkce, query, start};
use reqwest::StatusCode;
use serde_json::{Value, json};

const ALLOW: (&str, &str) = ("decision", "allow");
const DRAFT: (&str, &str) = ("scope", "draft");

async fn refresh(
    server: &common::TestServer,
    client_id: &str,
    tokens: &Value,
    scope: &str,
) -> Value {
    let (status, body) = json(
        server
            .post_form(
                "/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                    ("client_id", client_id),
                    ("scope", scope),
                ],
                None,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn the_token_response_says_what_consent_granted() {
    let server = start(vec![account(42, "Alice")]).await;
    for (asked, consent, granted) in [
        // Claude Code and others that ask for nothing, or for read.
        (None, &[ALLOW][..], "read"),
        (Some("read"), &[ALLOW], "read"),
        // Drafts asked for and left checked, or unchecked by the user.
        (Some("read draft"), &[DRAFT, ALLOW], "read draft"),
        (Some("draft"), &[ALLOW], "read"),
        // Checked by the user though the app didn't ask.
        (None, &[DRAFT, ALLOW], "read draft"),
        // Every Scope advertised: send and organize are not offered yet.
        (
            Some("read draft send organize"),
            &[DRAFT, ALLOW],
            "read draft",
        ),
        // A hand-made form can't add what the screen doesn't offer.
        (
            Some("send organize"),
            &[
                ("scope", "send"),
                ("scope", "organize"),
                ("scope", "read"),
                ALLOW,
            ],
            "read",
        ),
    ] {
        let (_, tokens) = server.login_scoped(asked, consent).await;
        assert_eq!(
            tokens["scope"], granted,
            "asked {asked:?}, posted {consent:?}"
        );
    }
}

#[tokio::test]
async fn asked_scopes_are_checked_on_the_consent_screen() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let drafts_box = r#"name="scope" value="draft""#;

    let (html, _) = server
        .authorize_scoped(&client_id, &pkce(), Some("read draft"), &[ALLOW])
        .await;
    assert!(html.contains(&format!("{drafts_box} checked>")), "{html}");
    assert!(
        html.contains(r#"<span class="read-only" hidden>read-only </span>"#),
        "{html}"
    );

    for asked in [None, Some("read"), Some("send organize")] {
        let (html, _) = server
            .authorize_scoped(&client_id, &pkce(), asked, &[ALLOW])
            .await;
        assert!(
            html.contains(&format!("{drafts_box}>")),
            "{asked:?}: {html}"
        );
        assert!(
            html.contains(r#"<span class="read-only">read-only </span>"#),
            "{asked:?}: {html}"
        );
        assert!(!html.contains(r#"value="send""#), "{html}");
        assert!(!html.contains(r#"value="organize""#), "{html}");
    }
}

#[tokio::test]
async fn unknown_scopes_are_invalid_scope() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    for scope in [
        "write",
        "read write",
        "READ",
        "read,draft",
        &"read ".repeat(60),
    ] {
        let path = format!(
            "{}&{}",
            server.authorize_path(&client_id, &pkce(), "s1"),
            form(&[("scope", scope)])
        );
        let res = server.get(&path, None).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{scope}");
        let to = location(&res);
        assert!(to.as_str().starts_with(REDIRECT), "{to}");
        assert_eq!(
            query(&to, "error").as_deref(),
            Some("invalid_scope"),
            "{scope}"
        );
        assert_eq!(query(&to, "state").as_deref(), Some("s1"));
    }
}

/// Scopes live on the Grant: a refresh keeps them, whatever `scope` it asks for.
/// Other Scopes mean a new Grant.
#[tokio::test]
async fn refresh_keeps_the_grants_scopes_and_never_upgrades() {
    let server = start(vec![account(42, "Alice")]).await;

    let (client_id, tokens) = server.login_scoped(Some("draft"), &[DRAFT, ALLOW]).await;
    let narrower = refresh(&server, &client_id, &tokens, "read").await;
    assert_eq!(narrower["scope"], "read draft");

    let (client_id, tokens) = server.login_scoped(None, &[ALLOW]).await;
    let wider = refresh(&server, &client_id, &tokens, "read draft send organize").await;
    assert_eq!(wider["scope"], "read");
    let again = refresh(&server, &client_id, &wider, "draft").await;
    assert_eq!(again["scope"], "read");
}

#[tokio::test]
async fn both_metadata_documents_advertise_every_scope() {
    let server = start(vec![account(42, "Alice")]).await;
    let every = json!(["read", "draft", "send", "organize"]);
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
    ] {
        let (status, body) = json(server.get(path, None).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scopes_supported"], every, "{path}");
    }
}
