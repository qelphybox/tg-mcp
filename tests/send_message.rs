//! send_message through the whole server (#46): a Grant whose Consent ticked
//! `send` sends into its Send allow-list, at once with Auto-send, else once
//! the user confirms on the page in the Consent browser; the same text isn't
//! sent twice within a minute; and the logs carry ids, never the text, the
//! page's id, the cookie or a Token.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use common::{TestServer, account, json, location, pkce, query, start_with};
use reqwest::{StatusCode, header};
use rmcp::{RoleClient, model::CallToolRequestParams, service::RunningService};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FIRST_SENT_ID, FakeData, fake_issued_id},
};

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
/// One of the Account's chats, never in the Send allow-list.
const BOB: ChatId = ChatId::User(102);

fn data() -> FakeData {
    let chat = |id: ChatId, title: &str| ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: None,
    };
    FakeData::new(account(ME, "Me")).with_chats(vec![chat(ALICE, "Alice"), chat(BOB, "Bob")])
}

/// A Grant with `send` to Alice (and Auto-send if `auto_send`): its access
/// Token, and the Confirmation cookie the Consent browser got (`name=value`).
async fn grant(server: &TestServer, auto_send: bool) -> (String, String) {
    let client_id = server.register().await;
    let pkce = pkce();
    let (login_path, login_cookie) = server.start_authorize(&client_id, &pkce, "xyz").await;
    let consent_path = server.wait_logged_in(&login_path, &login_cookie).await;
    let loaded = server
        .post_form(
            &format!("{consent_path}/chats"),
            &[("scope", "send")],
            Some(&login_cookie),
        )
        .await;
    assert_eq!(loaded.status(), StatusCode::OK);
    let alice = fake_issued_id(ALICE);
    let mut form = vec![("scope", "send"), ("chat", alice.as_str())];
    if auto_send {
        form.push(("auto_send", "on"));
    }
    form.push(("decision", "allow"));
    let res = server
        .post_form(&consent_path, &form, Some(&login_cookie))
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookie = res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let code = query(&location(&res), "code").expect("code in redirect");
    let (status, tokens) = json(server.exchange(&client_id, &code, &pkce.verifier).await).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["scope"], "read send");
    (tokens["access_token"].as_str().unwrap().to_string(), cookie)
}

type Client = RunningService<RoleClient, rmcp::model::ClientConfig>;

async fn send(client: &Client, chat: ChatId, text: &str) -> Result<Value, String> {
    let arguments = json!({ "chat_id": fake_issued_id(chat), "text": text });
    client
        .call_tool(
            CallToolRequestParams::new("send_message")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
        .map_err(|err| err.to_string())
}

#[tokio::test]
async fn with_auto_send_it_sends_once_only_into_the_allow_list_and_logs_ids_only() {
    let logs = captured_logs();
    let server = start_with(vec![data()]).await;
    let (token, _) = grant(&server, true).await;
    let client = server.mcp(&token).await;
    // Something no log line can contain by chance.
    let text = "**Auto** send e2e needle-7c1f9a";

    let sent = send(&client, ALICE, text).await.unwrap();
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(sent["message_id"], FIRST_SENT_ID);
    let err = send(&client, ALICE, text).await.unwrap_err();
    assert!(
        err.contains(&format!("already sent: message {FIRST_SENT_ID}")),
        "{err}"
    );
    let err = send(&client, BOB, "to Bob needle-7c1f9a")
        .await
        .unwrap_err();
    assert!(err.contains("Send allow-list"), "{err}");
    let messages = server.connector.sent().of(ME);
    assert_eq!(messages.len(), 1, "sent once, and only to Alice");
    assert_eq!(messages[0].chat, ALICE);
    assert_eq!(messages[0].text.text, "Auto send e2e needle-7c1f9a");

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        logs.contains("message sent without confirmation (auto-send)"),
        "{logs}"
    );
    assert!(
        logs.contains(&format!("message_id={FIRST_SENT_ID}")),
        "{logs}"
    );
    assert!(
        logs.contains("same text to the same chat within a minute: not sent again"),
        "{logs}"
    );
    assert!(!logs.contains("needle-7c1f9a"), "no text in the logs");
    assert!(!logs.contains(&token), "no Token in the logs");
    assert!(!logs.contains("tgm_"), "no Token or cookie in the logs");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn without_auto_send_the_consent_browser_confirms_and_then_it_is_already_sent() {
    let logs = captured_logs();
    let server = start_with(vec![data()]).await;
    let (token, cookie) = grant(&server, false).await;
    // rmcp's default client declares no elicitation: it gets the link.
    let client = server.mcp(&token).await;
    let text = "Confirm e2e needle-d40b2e";

    let pending = send(&client, ALICE, text).await.unwrap();
    assert_eq!(pending["status"], "pending_confirmation", "{pending}");
    let url = pending["confirm_url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&server.url("/confirm/")), "{url}");
    assert!(
        server.connector.sent().of(ME).is_empty(),
        "not before the user confirms"
    );

    let http = reqwest::Client::new();
    let page = http
        .get(&url)
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("needle-d40b2e"), "the page shows the text");
    let start = page.find(r#"name="csrf" value=""#).expect("a form") + 19;
    let csrf = page[start..].split('"').next().unwrap();
    let decided = http
        .post(&url)
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(common::form(&[("csrf", csrf), ("decision", "send")]))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(decided.contains("The message was sent"), "{decided}");
    assert_eq!(server.connector.sent().of(ME).len(), 1);

    let err = send(&client, ALICE, text).await.unwrap_err();
    assert!(
        err.contains(&format!("already sent: message {FIRST_SENT_ID}")),
        "{err}"
    );
    assert_eq!(server.connector.sent().of(ME).len(), 1);

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(logs.contains("pending send made"), "{logs}");
    assert!(
        logs.contains("pending send confirmed: message sent"),
        "{logs}"
    );
    let id = url.rsplit_once('/').unwrap().1;
    let cookie_value = cookie.split_once('=').unwrap().1;
    for secret in ["needle-d40b2e", id, cookie_value, csrf, &token, "tgm_"] {
        assert!(!logs.contains(secret), "{secret} in the logs");
    }
    client.cancel().await.unwrap();
}

/// Everything the server logs while this test binary runs.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        tracing_subscriber::fmt()
            // As in production (main.rs), and everything of ours.
            .with_env_filter("info,tg_mcp=trace,rmcp=info")
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
