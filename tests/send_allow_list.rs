//! The Send allow-list on the Consent screen (ADR-0005, #44): with `send`
//! ticked, the screen offers the logged-in Account's explicit-list Chat
//! folders, its 100 most recent Chats and its own Chats found by name; Allow
//! with nothing chosen is refused; the Grant gets exactly the Chats chosen,
//! sealed, and opens with them.

mod common;

use std::{
    collections::BTreeSet,
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use common::{
    REDIRECT, TestServer, account, json, location, pkce, query, start_with, start_with_env,
};
use reqwest::{StatusCode, header};
use serde_json::Value;
use tg_mcp::{
    allow_list::WriteRefused,
    tg::{
        FakeLogin,
        chats::{ChatId, ChatKind, ChatSummary, MessagePreview},
        fake::{FakeChatFolder, FakeData, fake_issued_id},
    },
};

const ME: i64 = 42;
/// Friends 1..=105, most recent first: the screen offers 1..=100.
const FRIENDS: i64 = 105;
/// In the "Work" folder, and too old to be among the recent ones.
const WORK_A: ChatId = ChatId::User(201);
const WORK_B: ChatId = ChatId::Chat(202);
/// In the "Work" folder too, but the Account has no dialog with it.
const LEFT: ChatId = ChatId::Channel(203);
/// An old chat, found only by name.
const CAROLYN: ChatId = ChatId::User(301);
/// A contact the Account never wrote to: found by Telegram, never offered.
const CAROL: ChatId = ChatId::User(300);
/// A public channel the Account isn't in: never offered.
const STRANGERS: ChatId = ChatId::Channel(909);

fn summary(id: ChatId, title: &str, kind: ChatKind, date: &str) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: Some(MessagePreview {
            date: date.into(),
            text: "hello".into(),
            media: None,
        }),
    }
}

fn friend(n: i64) -> ChatId {
    ChatId::User(n)
}

fn data() -> FakeData {
    let mut chats: Vec<ChatSummary> = (1..=FRIENDS)
        .map(|n| {
            summary(
                friend(n),
                &format!("Friend {n}"),
                ChatKind::Private,
                "2026-11-06T12:00:00Z",
            )
        })
        .collect();
    chats.extend([
        summary(
            WORK_A,
            "Zebra Desk",
            ChatKind::Private,
            "2026-01-01T00:00:00Z",
        ),
        summary(
            WORK_B,
            "Zebra Team",
            ChatKind::Group,
            "2026-01-01T00:00:00Z",
        ),
        summary(
            CAROLYN,
            "Carolyn Oldfriend",
            ChatKind::Private,
            "2025-01-01T00:00:00Z",
        ),
    ]);
    FakeData::new(account(ME, "Me"))
        .with_chats(chats)
        .with_contacts(vec![summary(
            CAROL,
            "Carol Contact",
            ChatKind::Private,
            "2025-01-01T00:00:00Z",
        )])
        .with_public(vec![summary(
            STRANGERS,
            "Carol Fans",
            ChatKind::Channel,
            "2025-01-01T00:00:00Z",
        )])
        .with_chat_folders(vec![
            FakeChatFolder {
                id: 3,
                title: "Work".into(),
                emoji: None,
                rules: false,
                pinned: vec![WORK_A],
                included: vec![WORK_B, LEFT],
                excluded: vec![],
            },
            FakeChatFolder {
                id: 4,
                title: "Unread".into(),
                emoji: None,
                rules: true,
                pinned: vec![],
                included: vec![],
                excluded: vec![],
            },
        ])
}

async fn server() -> TestServer {
    start_with(vec![data()]).await
}

/// A Login flow logged in and on the Consent screen: (consent path, cookie, client, pkce).
struct Flow {
    consent: String,
    chats: String,
    cookie: String,
    client_id: String,
    pkce: common::Pkce,
}

async fn on_consent(server: &TestServer) -> Flow {
    let client_id = server.register().await;
    let pkce = pkce();
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce, "xyz").await;
    let consent = server.wait_logged_in(&login_path, &cookie).await;
    Flow {
        chats: format!("{consent}/chats"),
        consent,
        cookie,
        client_id,
        pkce,
    }
}

impl Flow {
    async fn post(
        &self,
        server: &TestServer,
        path: &str,
        form: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let res = server.post_form(path, form, Some(&self.cookie)).await;
        (res.status(), res.text().await.unwrap())
    }

    /// Allow with `form`; the code, then the token response.
    async fn allow(&self, server: &TestServer, form: &[(&str, &str)]) -> Value {
        let res = server
            .post_form(&self.consent, form, Some(&self.cookie))
            .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let redirect = location(&res);
        assert!(redirect.as_str().starts_with(REDIRECT));
        let code = query(&redirect, "code").expect("code in redirect");
        let (status, tokens) = json(
            server
                .exchange(&self.client_id, &code, &self.pkce.verifier)
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{tokens}");
        tokens
    }
}

fn id(chat: ChatId) -> String {
    fake_issued_id(chat)
}

/// The Chats and auto-send flag of the Grant behind `tokens`, opened with its Token.
async fn opened(server: &TestServer, tokens: &Value) -> (BTreeSet<String>, bool) {
    let grant = server
        .vault
        .open(tokens["access_token"].as_str().unwrap())
        .await
        .unwrap();
    let list = grant.send_allow_list.expect("a send Grant has a list");
    (
        list.chats().map(|c| c.to_string()).collect(),
        list.auto_send(),
    )
}

fn set(chats: &[ChatId]) -> BTreeSet<String> {
    chats.iter().map(ChatId::to_string).collect()
}

const SEND: (&str, &str) = ("scope", "send");
const ALLOW: (&str, &str) = ("decision", "allow");

#[tokio::test]
async fn two_chats_chosen_are_exactly_what_the_grant_opens_with() {
    let logs = captured_logs();
    let server = server().await;
    let flow = on_consent(&server).await;

    // The screen opens with Send unticked, and nothing asked of Telegram yet.
    let res = server.get(&flow.consent, Some(&flow.cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = res.text().await.unwrap();
    assert!(html.contains(r#"value="send">"#), "{html}");
    assert!(html.contains("Choose chats</button>"), "{html}");
    assert_eq!(server.connector.consent_clients(), 0);

    // "Choose chats": folders that are lists, the 100 most recent chats.
    let (status, html) = flow.post(&server, &flow.chats, &[SEND]).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(server.connector.consent_clients(), 1);
    assert!(html.contains(r#"name="folder" value="3""#), "{html}");
    assert!(
        !html.contains(r#"name="folder" value="4""#),
        "a folder of rules: {html}"
    );
    assert_eq!(html.matches(r#"name="chat""#).count(), 100, "{html}");
    for n in [1, 100] {
        assert!(
            html.contains(&format!(r#"value="{}""#, id(friend(n)))),
            "{n}"
        );
    }
    for chat in [friend(101), WORK_A, CAROLYN] {
        assert!(
            !html.contains(&id(chat)),
            "{chat} is not among the recent ones"
        );
    }
    assert!(
        html.contains(r#"class="write send" name="scope" value="send" checked>"#),
        "the box stays as posted: {html}"
    );
    assert!(
        html.contains(r#"name="auto_send" value="on">"#),
        "auto-send starts off"
    );

    let tokens = flow
        .allow(
            &server,
            &[
                SEND,
                ("chat", &id(friend(1))),
                ("chat", &id(friend(2))),
                ALLOW,
            ],
        )
        .await;
    assert_eq!(tokens["scope"], "read send");
    assert_eq!(
        opened(&server, &tokens).await,
        (set(&[friend(1), friend(2)]), false)
    );
    let grant = server
        .vault
        .open(tokens["access_token"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(grant.may_write_to(friend(2)), Ok(()));
    assert_eq!(
        grant.may_write_to(friend(3)),
        Err(WriteRefused::NotInAllowList(friend(3)))
    );

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(logs.contains("send_chats=2"), "counts are logged: {logs}");
    assert!(logs.contains("recent=100"), "{logs}");
    for secret in ["user:1", "user:2", "Friend 1", "tgm_"] {
        assert!(!logs.contains(secret), "{secret} in the logs");
    }
}

#[tokio::test]
async fn send_with_nothing_chosen_is_refused_and_the_login_goes_on() {
    let server = server().await;
    let flow = on_consent(&server).await;

    // Allow without ever loading the chats: refused, and they are loaded now.
    let (status, html) = flow.post(&server, &flow.consent, &[SEND, ALLOW]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        html.contains("Choose at least one chat it may send to, or untick “Send messages”."),
        "{html}"
    );
    assert!(html.contains(&id(friend(1))), "{html}");

    // Ticked chats the screen never offered don't count either.
    let (status, _) = flow
        .post(
            &server,
            &flow.consent,
            &[
                SEND,
                ("chat", &id(friend(101))),
                ("chat", &id(CAROL)),
                ALLOW,
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The same login still finishes, with a chat this time.
    let tokens = flow
        .allow(
            &server,
            &[SEND, ("chat", &id(friend(5))), ("auto_send", "on"), ALLOW],
        )
        .await;
    assert_eq!(opened(&server, &tokens).await, (set(&[friend(5)]), true));
}

#[tokio::test]
async fn a_folder_brings_its_chats_with_a_dialog_as_they_are_now() {
    let server = server().await;
    let flow = on_consent(&server).await;
    flow.post(&server, &flow.chats, &[SEND]).await;
    let tokens = flow
        .allow(&server, &[SEND, ("folder", "3"), ("folder", "4"), ALLOW])
        .await;
    // Not the chat it left, and nothing for the folder of rules.
    assert_eq!(
        opened(&server, &tokens).await,
        (set(&[WORK_A, WORK_B]), false)
    );
}

#[tokio::test]
async fn search_offers_only_the_accounts_own_chats_with_a_dialog() {
    let logs = captured_logs();
    let server = server().await;
    let flow = on_consent(&server).await;

    let (status, html) = flow
        .post(&server, &flow.chats, &[SEND, ("q", "Carol")])
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Found 1 of your chats."), "{html}");
    assert!(html.contains(&id(CAROLYN)), "{html}");
    assert!(html.contains("Carolyn Oldfriend"), "{html}");
    for stranger in [CAROL, STRANGERS] {
        assert!(!html.contains(&stranger.to_string()), "{stranger}: {html}");
    }
    assert!(!html.contains("Carol Fans") && !html.contains("Carol Contact"));
    assert!(
        html.contains(r#"value="Carol""#),
        "the name stays in the box"
    );

    let (_, html) = flow
        .post(&server, &flow.chats, &[SEND, ("q", "Nobody")])
        .await;
    assert!(html.contains("None of your chats has that name."), "{html}");

    // Found, ticked, searched again: still offered, and picked.
    let (_, html) = flow
        .post(&server, &flow.chats, &[SEND, ("q", "Carol")])
        .await;
    assert!(html.contains(&id(CAROLYN)));
    let (_, html) = flow
        .post(
            &server,
            &flow.chats,
            &[SEND, ("chat", &id(CAROLYN)), ("q", "Zebra")],
        )
        .await;
    assert!(
        html.contains(&format!(r#"value="{}" checked>"#, id(CAROLYN))),
        "{html}"
    );
    assert!(html.contains(&id(WORK_A)), "{html}");
    let tokens = flow
        .allow(
            &server,
            &[
                SEND,
                ("chat", &id(CAROLYN)),
                ("chat", &id(WORK_B)),
                ("chat", &id(CAROL)),
                ALLOW,
            ],
        )
        .await;
    assert_eq!(
        opened(&server, &tokens).await,
        (set(&[CAROLYN, WORK_B]), false)
    );

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(logs.contains("consent chat search"), "{logs}");
    for secret in ["Carol", "Zebra", "user:301"] {
        assert!(!logs.contains(secret), "{secret} in the logs");
    }
}

#[tokio::test]
async fn without_send_consent_asks_telegram_nothing() {
    let server = server().await;
    for consent in [&[ALLOW][..], &[("scope", "draft"), ALLOW]] {
        let flow = on_consent(&server).await;
        server.get(&flow.consent, Some(&flow.cookie)).await;
        // Chats ticked without `send` give nothing.
        let mut form = consent.to_vec();
        let chat = id(friend(1));
        form.insert(0, ("chat", chat.as_str()));
        let tokens = flow.allow(&server, &form).await;
        let grant = server
            .vault
            .open(tokens["access_token"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(grant.send_allow_list, None);
        assert_eq!(
            grant.may_write_to(friend(1)),
            Err(WriteRefused::NoSendScope)
        );
    }
    assert_eq!(server.connector.consent_clients(), 0);
}

/// A client asking for every Scope gets `send` offered, not ticked: Allow
/// right away gives a Grant that reads (and writes Drafts).
#[tokio::test]
async fn send_asked_for_starts_unticked() {
    let server = server().await;
    let client_id = server.register().await;
    let (html, code) = server
        .authorize_scoped(
            &client_id,
            &pkce(),
            Some("read draft send organize"),
            &[("scope", "draft"), ALLOW],
        )
        .await;
    assert!(html.contains(r#"value="draft" checked>"#), "{html}");
    assert!(html.contains(r#"value="send">"#), "{html}");
    assert!(html.contains(r#"<section class="send" hidden>"#), "{html}");
    assert!(!code.is_empty());
}

#[tokio::test]
async fn the_chat_pickers_need_the_login_cookie_and_a_finished_login() {
    let server = server().await;
    let flow = on_consent(&server).await;
    for cookie in [None, Some("tgm_login=someone-else")] {
        let res = server.post_form(&flow.chats, &[SEND], cookie).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(server.connector.consent_clients(), 0);

    // Before the QR is scanned: back to the login page.
    let client_id = server.register().await;
    let (login_path, cookie) = server.start_authorize(&client_id, &pkce(), "s").await;
    let res = server
        .post_form(
            &format!("{login_path}/consent/chats"),
            &[SEND],
            Some(&cookie),
        )
        .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], login_path.as_str());
    assert_eq!(server.connector.consent_clients(), 0);
}

#[tokio::test]
async fn the_chat_pickers_are_rate_limited_per_client() {
    let server = start_with_env(
        Arc::new(FakeLogin::new(data())),
        &[
            ("TG_MCP_CONSENT_CHATS_PER_MINUTE", "1"),
            ("TG_MCP_CONSENT_CHATS_BURST", "2"),
        ],
    )
    .await;
    let flow = on_consent(&server).await;
    for _ in 0..2 {
        let (status, _) = flow
            .post(&server, &flow.chats, &[SEND, ("q", "Friend")])
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let res = server
        .post_form(&flow.chats, &[SEND, ("q", "Friend")], Some(&flow.cookie))
        .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers().contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn telegram_failing_shows_why_and_grants_nothing() {
    let server = server().await;
    let flow = on_consent(&server).await;
    server.connector.set_unreachable(true);
    let (status, html) = flow.post(&server, &flow.chats, &[SEND]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Telegram didn"), "{html}");
    assert!(html.contains("Choose chats</button>"), "try again: {html}");
    let (status, _) = flow
        .post(
            &server,
            &flow.consent,
            &[SEND, ("chat", &id(friend(1))), ALLOW],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    server.connector.set_unreachable(false);
    flow.post(&server, &flow.chats, &[SEND]).await;
    let tokens = flow
        .allow(&server, &[SEND, ("chat", &id(friend(1))), ALLOW])
        .await;
    assert_eq!(opened(&server, &tokens).await, (set(&[friend(1)]), false));
}

#[tokio::test]
async fn a_refresh_keeps_the_allow_list() {
    let server = server().await;
    let flow = on_consent(&server).await;
    flow.post(&server, &flow.chats, &[SEND]).await;
    let tokens = flow
        .allow(&server, &[SEND, ("chat", &id(friend(9))), ALLOW])
        .await;
    let (status, rotated) = json(
        server
            .post_form(
                "/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                    ("client_id", &flow.client_id),
                ],
                None,
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert_eq!(rotated["scope"], "read send");
    assert_eq!(opened(&server, &rotated).await, (set(&[friend(9)]), false));
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
