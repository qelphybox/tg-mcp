//! The Cloud password step of the Login flow, driven like a browser would.

mod common;

use std::sync::Arc;

use common::{Pkce, TestServer, account, json, location, pkce, query, start, start_with_login};
use reqwest::{Response, StatusCode, header};
use tg_mcp::tg::{FakeLogin, FakePassword};

const PASSWORD: &str = "correct horse battery staple";

async fn server_with_password(flood_after: Option<u32>) -> TestServer {
    start_with_login(Arc::new(FakeLogin::with_password(
        account(42, "Alice"),
        FakePassword {
            password: PASSWORD.into(),
            hint: Some("the <horse> one".into()),
            flood_after,
        },
    )))
    .await
}

/// A browser at the password step of one Login flow.
struct AtPassword {
    client_id: String,
    pkce: Pkce,
    login_path: String,
    cookie: String,
}

impl AtPassword {
    fn password_path(&self) -> String {
        format!("{}/password", self.login_path)
    }
}

/// authorize → QR "scanned" → the status poll asks for the password.
async fn until_password(server: &TestServer) -> AtPassword {
    let client_id = server.register().await;
    let pkce = pkce();
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce, "xyz").await;
    for _ in 0..10 {
        let (status, body) = json(
            server
                .get(&format!("{login_path}/status"), Some(&cookie))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if body["state"] == "password" {
            assert_eq!(body["next"], format!("{login_path}/password"));
            return AtPassword {
                client_id,
                pkce,
                login_path,
                cookie,
            };
        }
        assert_eq!(body["state"], "waiting", "{body}");
    }
    panic!("never asked for the password");
}

async fn submit(server: &TestServer, at: &AtPassword, password: &str) -> Response {
    server
        .post_form(
            &at.password_path(),
            &[("password", password)],
            Some(&at.cookie),
        )
        .await
}

#[tokio::test]
async fn two_factor_account_completes_the_login_flow() {
    let server = server_with_password(None).await;
    let at = until_password(&server).await;

    let page = server.get(&at.password_path(), Some(&at.cookie)).await;
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    let html = page.text().await.unwrap();
    assert!(html.contains(r#"type="password""#));
    assert!(html.contains(&format!(r#"action="{}""#, at.password_path())));
    assert!(html.contains("the &lt;horse&gt; one"), "hint, escaped");

    let empty = submit(&server, &at, "").await;
    assert_eq!(empty.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        empty
            .text()
            .await
            .unwrap()
            .contains("Enter your cloud password.")
    );

    let wrong = submit(&server, &at, "hunter2").await;
    assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = wrong.text().await.unwrap();
    assert!(
        html.contains("Wrong password. 4 attempts left."),
        "an empty form doesn't count, a wrong password does: {html}"
    );
    assert!(
        html.contains(r#"type="password""#),
        "the form is shown again"
    );
    assert!(!html.contains("hunter2"), "the password is never echoed");

    let ok = submit(&server, &at, PASSWORD).await;
    assert_eq!(ok.status(), StatusCode::SEE_OTHER);
    let consent_path = ok.headers()[header::LOCATION].to_str().unwrap().to_string();
    assert_eq!(consent_path, format!("{}/consent", at.login_path));

    // A status poll still running in the tab moves on to Consent too.
    let (_, status) = json(
        server
            .get(&format!("{}/status", at.login_path), Some(&at.cookie))
            .await,
    )
    .await;
    assert_eq!(status["state"], "logged_in");

    let consent = server.get(&consent_path, Some(&at.cookie)).await;
    assert_eq!(consent.status(), StatusCode::OK);
    assert!(consent.text().await.unwrap().contains("Alice"));
    let res = server
        .post_form(&consent_path, &[("decision", "allow")], Some(&at.cookie))
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let code = query(&location(&res), "code").expect("code in redirect");
    let (status, tokens) = json(
        server
            .exchange(&at.client_id, &code, &at.pkce.verifier)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");

    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    let whoami = client
        .call_tool(rmcp::model::CallToolRequestParams::new("whoami"))
        .await
        .unwrap();
    assert_eq!(whoami.structured_content.unwrap()["id"], 42);
}

#[tokio::test]
async fn consent_cannot_skip_the_password() {
    let server = server_with_password(None).await;
    let at = until_password(&server).await;
    let res = server
        .post_form(
            &format!("{}/consent", at.login_path),
            &[("decision", "allow")],
            Some(&at.cookie),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(res.headers().get(header::LOCATION).is_none());
    let consent = server
        .get(&format!("{}/consent", at.login_path), Some(&at.cookie))
        .await;
    assert_ne!(consent.status(), StatusCode::OK);
}

#[tokio::test]
async fn password_step_belongs_to_the_browser_that_started_it() {
    let server = server_with_password(None).await;
    let at = until_password(&server).await;
    for wrong in [None, Some("tgm_login=someone-else")] {
        assert_eq!(
            server.get(&at.password_path(), wrong).await.status(),
            StatusCode::FORBIDDEN
        );
        let res = server
            .post_form(&at.password_path(), &[("password", PASSWORD)], wrong)
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    // Those were never checked: the real browser still has every attempt.
    let wrong = submit(&server, &at, "hunter2").await;
    assert!(
        wrong
            .text()
            .await
            .unwrap()
            .contains("Wrong password. 4 attempts left.")
    );
}

#[tokio::test]
async fn flood_wait_is_honoured_not_retried_around() {
    let server = server_with_password(Some(1)).await;
    let at = until_password(&server).await;

    assert_eq!(
        submit(&server, &at, "hunter2").await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let flood = submit(&server, &at, "hunter3").await;
    assert_eq!(flood.status(), StatusCode::TOO_MANY_REQUESTS);
    let html = flood.text().await.unwrap();
    assert!(
        html.contains("Telegram limits password attempts. Try again in 30 seconds."),
        "{html}"
    );

    // The fake floods only once, so had this reached it, it would have been accepted.
    let during = submit(&server, &at, PASSWORD).await;
    assert_eq!(during.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        during
            .text()
            .await
            .unwrap()
            .contains("Telegram limits password attempts.")
    );
}

#[tokio::test]
async fn too_many_wrong_passwords_end_the_login() {
    let server = server_with_password(None).await;
    let at = until_password(&server).await;
    for left in (1..=4).rev() {
        let res = submit(&server, &at, "hunter2").await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let attempts = if left == 1 { "attempt" } else { "attempts" };
        assert!(
            res.text()
                .await
                .unwrap()
                .contains(&format!("Wrong password. {left} {attempts} left."))
        );
    }
    let last = submit(&server, &at, "hunter2").await;
    assert_eq!(last.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(last.text().await.unwrap().contains("Too many attempts"));

    assert_eq!(
        submit(&server, &at, PASSWORD).await.status(),
        StatusCode::GONE
    );
    assert_eq!(
        server
            .get(&format!("{}/status", at.login_path), Some(&at.cookie))
            .await
            .status(),
        StatusCode::GONE
    );
}

#[tokio::test]
async fn password_page_sends_a_qr_login_back_to_the_qr() {
    let server = start(vec![account(42, "Alice")]).await;
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "s").await;

    let res = server
        .post_form(
            &format!("{login_path}/password"),
            &[("password", PASSWORD)],
            Some(&cookie),
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], login_path.as_str());
}
