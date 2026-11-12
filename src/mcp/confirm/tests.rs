//! send_message (#46) and its delivery through `/mcp` with rmcp's own
//! client: Auto-send sends at once; a client with URL elicitation is asked
//! to open the confirmation page and the call returns the user's decision;
//! any other gets the link. Only into the Send allow-list, never the same
//! text twice within a minute, with save_draft's text and forum rules.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{extract::Request, middleware::Next};
use reqwest::header;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientConfig, ElicitRequestParams, ElicitResult,
        ElicitationCapability, FormElicitationCapability, ProtocolVersion,
        UrlElicitationCapability,
    },
    service::{RequestContext, RunningService},
    transport::{
        StreamableHttpClientTransport,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde_json::{Value, json};

use super::*;
use crate::{
    allow_list::SendAllowList,
    auth::{
        AuthState,
        clients::ClientRegistry,
        rate_limit::{OAuthLimits, Rate},
    },
    confirm::cookie,
    db,
    limits::WriteLimit,
    links::MediaLinks,
    scope::{Scope, Scopes},
    sessions::{TokenTtl, Vault},
    tg::{
        Account, Connector, FakeConnector, FakeLogin, TgError,
        chats::{ChatId, ChatKind, ChatSummary},
        fake::{FIRST_SENT_ID, FakeData, FakeTopic, fake_issued_id},
        messages::RawHistoryMessage,
    },
};

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
/// One of the Account's chats, not in the Send allow-list.
const BOB: ChatId = ChatId::User(102);
/// A forum in the Send allow-list: General (1) and Releases (10), where
/// message 12 is.
const FORUM: ChatId = ChatId::Channel(505);

fn raw(id: i32, topic: Option<i32>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: 1_793_923_200 + id,
        from: None,
        out: false,
        text: "earlier".into(),
        media: None,
        reply_to: None,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
        topic,
    }
}

fn data() -> FakeData {
    let chat = |id: ChatId, title: &str| ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind: if id == FORUM {
            ChatKind::Supergroup
        } else {
            ChatKind::Private
        },
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: None,
    };
    FakeData::new(Account {
        id: ME,
        first_name: "Me".into(),
        last_name: None,
        username: None,
    })
    .with_chats(vec![
        chat(ALICE, "Alice"),
        chat(BOB, "Bob"),
        chat(FORUM, "Dev forum"),
    ])
    .with_messages(
        FORUM,
        vec![raw(5, None), raw(10, Some(10)), raw(12, Some(10))],
    )
    .with_topics(
        FORUM,
        vec![FakeTopic::new(10, "Releases"), FakeTopic::new(1, "General")],
    )
}

/// The server: `/mcp` for one Grant with `send` to Alice, and `/confirm`.
struct Server {
    base: String,
    auth: Arc<AuthState>,
    connector: FakeConnector,
    /// The Consent browser's Confirmation cookie, `name=value`.
    cookie: String,
    _stop: tokio_util::sync::DropGuard,
    _dir: tempfile::TempDir,
}

async fn server(auto_send: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("t.db")).await.unwrap();
    let vault = Arc::new(Vault::new(pool.clone(), TokenTtl::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let connector = FakeConnector::default();
    let auth = Arc::new(AuthState::new(
        base.clone(),
        vault.clone(),
        ClientRegistry::new(pool),
        Arc::new(FakeLogin::default()),
        Arc::new(connector.clone()),
        Arc::new(MediaLinks::new(&base)),
        OAuthLimits {
            confirm: Rate {
                per_minute: 10_000,
                burst: 10_000,
            },
            ..OAuthLimits::default()
        },
    ));
    let client = auth
        .clients
        .register(
            Some("Claude Code"),
            &["http://localhost:4567/cb".to_string()],
        )
        .await
        .unwrap();
    let issued_cookie = cookie::issue(vault.ttl().refresh, false);
    let cookie = issued_cookie
        .set_cookie
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let list = SendAllowList::new([ALICE, FORUM], auto_send)
        .unwrap()
        .with_confirm_cookie(issued_cookie.verifier);
    let scopes: Scopes = [Scope::Send].into_iter().collect();
    let issued = vault
        .create_grant_with_send(
            &client.id,
            ME,
            &serde_json::to_vec(&data()).unwrap(),
            scopes,
            Some(&list),
        )
        .await
        .unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();
    let caller = Caller {
        grant_id: grant.grant_id.clone(),
        account_id: grant.account_id,
        scopes: grant.scopes,
        send_allow_list: grant.send_allow_list.clone().map(Arc::new),
        telegram: connector.connect(grant).await.unwrap(),
    };

    let stop = tokio_util::sync::CancellationToken::new();
    let (links, confirmations) = (auth.links.clone(), auth.confirmations.clone());
    let mcp = StreamableHttpService::new(
        move || {
            Ok(
                TgMcp::with_tools(links.clone(), TgMcp::scoped_tools(), false)
                    .with_confirmations(confirmations.clone()),
            )
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(["127.0.0.1"])
            .with_cancellation_token(stop.clone()),
    );
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn(
            move |mut request: Request, next: Next| {
                request.extensions_mut().insert(caller.clone());
                next.run(request)
            },
        ))
        .merge(crate::confirm::router(auth.clone()));
    let serving = stop.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(serving.cancelled_owned())
        .await
        .unwrap();
    });
    Server {
        base,
        auth,
        connector,
        cookie,
        _stop: stop.drop_guard(),
        _dir: dir,
    }
}

/// An MCP client in the user's app: what it declares, how the user answers
/// an elicitation, and what they then press on the page (in a browser with
/// `cookie`, or without).
#[derive(Clone)]
struct App {
    config: ClientConfig,
    answer: ElicitationAction,
    press: Option<(&'static str, Option<String>)>,
    opened: Arc<Mutex<Vec<String>>>,
}

impl App {
    fn new(elicitation: Option<ElicitationCapability>) -> Self {
        let mut config =
            ClientConfig::default().with_protocol_version(ProtocolVersion::V_2025_11_25);
        config.capabilities.elicitation = elicitation;
        Self {
            config,
            answer: ElicitationAction::Accept,
            press: None,
            opened: Arc::default(),
        }
    }

    fn with_url_elicitation() -> Self {
        Self::new(Some(
            ElicitationCapability::new().with_url(UrlElicitationCapability::new()),
        ))
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

impl ClientHandler for App {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let ElicitRequestParams::UrlElicitationParams { url, message, .. } = request else {
            panic!("only URL elicitation is asked for");
        };
        assert_eq!(message, ELICITATION_MESSAGE);
        self.opened.lock().unwrap().push(url.clone());
        if let Some((decision, cookie)) = self.press.clone() {
            // The user, in the browser the app opened.
            tokio::spawn(press(url, decision, cookie));
        }
        Ok(ElicitResult::new(self.answer.clone()))
    }
}

/// GET the page, then POST `decision` with its CSRF token, as a browser
/// with `cookie` (or none). Returns the answer's HTML.
async fn press(url: String, decision: &'static str, cookie: Option<String>) -> String {
    let http = reqwest::Client::new();
    let mut get = http.get(&url);
    if let Some(cookie) = &cookie {
        get = get.header(header::COOKIE, cookie);
    }
    let html = get.send().await.unwrap().text().await.unwrap();
    let start = html.find(r#"name="csrf" value=""#).expect("a form") + 19;
    let csrf = html[start..].split('"').next().unwrap().to_string();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([("csrf", csrf.as_str()), ("decision", decision)])
        .finish();
    let mut post = http
        .post(&url)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body);
    if let Some(cookie) = &cookie {
        post = post.header(header::COOKIE, cookie);
    }
    post.send().await.unwrap().text().await.unwrap()
}

async fn connect(server: &Server, app: App) -> RunningService<RoleClient, App> {
    let transport = StreamableHttpClientTransport::from_uri(format!("{}/mcp", server.base));
    app.serve(transport).await.expect("initialize")
}

/// send_message with `arguments`: its structured result, or the error.
async fn call(
    client: &RunningService<RoleClient, App>,
    arguments: Value,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("send_message")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| {
            assert_eq!(r.is_error, Some(false), "{r:?}");
            r.structured_content.expect("structured output")
        })
}

/// A text no other send of this test binary has: the duplicate guard lets
/// each through.
fn fresh_text() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    format!("**hi** Alice, number {}", N.fetch_add(1, Ordering::Relaxed))
}

/// send_message of a fresh text to `chat`.
async fn send(
    client: &RunningService<RoleClient, App>,
    chat: ChatId,
) -> Result<Value, rmcp::ServiceError> {
    send_text(client, chat, &fresh_text()).await
}

async fn send_text(
    client: &RunningService<RoleClient, App>,
    chat: ChatId,
    text: &str,
) -> Result<Value, rmcp::ServiceError> {
    call(
        client,
        json!({ "chat_id": fake_issued_id(chat), "text": text }),
    )
    .await
}

#[tokio::test]
async fn with_url_elicitation_the_call_waits_and_returns_sent() {
    let server = server(false).await;
    let app = App {
        press: Some(("send", Some(server.cookie.clone()))),
        ..App::with_url_elicitation()
    };
    let client = connect(&server, app.clone()).await;
    let result = send(&client, ALICE).await.unwrap();
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(result["message_id"], FIRST_SENT_ID);
    assert_eq!(result["chat_id"], fake_issued_id(ALICE));
    assert!(result["date"].as_str().unwrap().ends_with('Z'), "{result}");
    assert!(
        result.get("confirm_url").is_none(),
        "the model never sees the link"
    );

    let opened = app.opened();
    assert_eq!(opened.len(), 1);
    assert!(
        opened[0].starts_with(&format!("{}/confirm/", server.base)),
        "{opened:?}"
    );
    let sent = server.connector.sent().of(ME);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].text.text.starts_with("hi Alice"), "{:?}", sent[0]);
    assert_eq!(sent[0].text.entities.len(), 1, "the bold");
    assert!(server.auth.confirmations.is_empty());
}

#[tokio::test]
async fn with_url_elicitation_a_phone_without_the_cookie_makes_it_a_draft() {
    let server = server(false).await;
    let app = App {
        press: Some(("draft", None)),
        ..App::with_url_elicitation()
    };
    let client = connect(&server, app).await;
    let result = send(&client, ALICE).await.unwrap();
    assert_eq!(result["status"], "drafted", "{result}");
    assert!(server.connector.sent().of(ME).is_empty());
    assert_eq!(server.connector.drafts().of(ME).len(), 1);
}

#[tokio::test]
async fn with_url_elicitation_cancel_or_declining_sends_nothing() {
    let server = server(false).await;
    let cancel = App {
        press: Some(("cancel", Some(server.cookie.clone()))),
        ..App::with_url_elicitation()
    };
    let client = connect(&server, cancel).await;
    assert_eq!(send(&client, ALICE).await.unwrap()["status"], "cancelled");

    for answer in [ElicitationAction::Decline, ElicitationAction::Cancel] {
        let declined = App {
            answer: answer.clone(),
            ..App::with_url_elicitation()
        };
        let client = connect(&server, declined.clone()).await;
        let result = send(&client, ALICE).await.unwrap();
        assert_eq!(result["status"], "cancelled", "{answer:?}: {result}");
        // The page it would have opened is no longer valid.
        let page = reqwest::get(&declined.opened()[0]).await.unwrap();
        assert_eq!(page.status(), 410);
    }
    assert!(server.connector.sent().of(ME).is_empty());
    assert!(server.auth.confirmations.is_empty());
}

#[tokio::test]
async fn a_send_cap_refusal_after_confirming_is_an_error_saying_when_to_retry() {
    let server = server(false).await;
    server.connector.sent().refuse_next(TgError::WritesLimited {
        seconds: 42,
        limit: WriteLimit::Chat,
    });
    let app = App {
        press: Some(("send", Some(server.cookie.clone()))),
        ..App::with_url_elicitation()
    };
    let client = connect(&server, app).await;
    let err = send(&client, ALICE).await.unwrap_err().to_string();
    assert!(err.contains("try again in 42 s"), "{err}");
    assert!(err.contains("not sent"), "{err}");
    assert!(server.connector.sent().of(ME).is_empty());
}

#[tokio::test]
async fn without_url_elicitation_the_call_returns_the_link_and_the_page_sends_later() {
    let server = server(false).await;
    let form_only =
        Some(ElicitationCapability::new().with_form(FormElicitationCapability::default()));
    for app in [App::new(None), App::new(form_only)] {
        let client = connect(&server, app.clone()).await;
        let result = send(&client, ALICE).await.unwrap();
        assert_eq!(result["status"], "pending_confirmation", "{result}");
        let url = result["confirm_url"].as_str().unwrap().to_string();
        assert!(
            url.starts_with(&format!("{}/confirm/", server.base)),
            "{url}"
        );
        assert!(result["expires_at"].as_str().unwrap().ends_with('Z'));
        assert!(result.get("message_id").is_none());
        assert!(app.opened().is_empty(), "no elicitation was sent");
        let before = server.connector.sent().of(ME).len();

        // The user opens the link in the Consent browser and sends.
        let html = press(url, "send", Some(server.cookie.clone())).await;
        assert!(html.contains("The message was sent"), "{html}");
        assert_eq!(server.connector.sent().of(ME).len(), before + 1);
    }
    assert_eq!(server.connector.sent().of(ME).len(), 2);
}

#[tokio::test]
async fn auto_send_sends_at_once_and_only_into_the_allow_list() {
    let server = server(true).await;
    let app = App::with_url_elicitation();
    let client = connect(&server, app.clone()).await;
    let result = send(&client, ALICE).await.unwrap();
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(result["message_id"], FIRST_SENT_ID);
    assert!(app.opened().is_empty(), "nobody was asked");
    assert!(server.auth.confirmations.is_empty());

    let err = send(&client, BOB).await.unwrap_err().to_string();
    assert!(
        err.contains("not one of the chats this connection may send to"),
        "{err}"
    );
    assert_eq!(server.connector.sent().of(ME).len(), 1);
}

#[tokio::test]
async fn a_chat_outside_the_allow_list_is_refused_before_anything_waits() {
    let server = server(false).await;
    let app = App::with_url_elicitation();
    let client = connect(&server, app.clone()).await;
    let err = send(&client, BOB).await.unwrap_err().to_string();
    assert!(err.contains("Send allow-list"), "{err}");
    assert!(app.opened().is_empty());
    assert!(server.auth.confirmations.is_empty());
}

#[tokio::test]
async fn send_message_is_listed_with_send_as_a_bounded_write_that_isnt_read_only() {
    let server = server(false).await;
    let client = connect(&server, App::new(None)).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "send_message")
        .expect("listed for a send Grant");
    let hints = tool.annotations.as_ref().unwrap();
    assert_eq!(hints.read_only_hint, Some(false));
    assert_eq!(hints.idempotent_hint, Some(false));
    assert_eq!(hints.open_world_hint, Some(false));
    assert_eq!(
        hints.destructive_hint,
        Some(false),
        "it only adds a message"
    );
    let description = tool.description.as_deref().unwrap();
    for words in [
        "Send a message as the user",
        "confirm_url",
        "the browser where they connected this app",
        "get_messages",
        "already sent: message N",
        "never retry it on your own",
    ] {
        assert!(description.contains(words), "{words}: {description}");
    }
    let input = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
    assert_eq!(input["properties"]["text"]["maxLength"], 16_384);
    assert_eq!(input["properties"]["text"]["minLength"], 1);
    assert!(input["properties"]["chat_id"]["maxLength"].is_u64());
    assert_eq!(input["required"], json!(["chat_id", "text"]));
    let output = serde_json::to_value(tool.output_schema.as_ref().unwrap().as_ref()).unwrap();
    assert!(
        output["properties"]["confirm_url"]["maxLength"].is_u64(),
        "{output}"
    );
    // A send Grant is told what it can do now.
    let said = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap();
    assert!(
        said.contains("send messages, react to messages and mark chats as read in the chats"),
        "{said}"
    );
}

#[tokio::test]
async fn the_same_text_to_the_same_chat_within_a_minute_is_already_sent() {
    let server = server(true).await;
    let client = connect(&server, App::new(None)).await;
    let text = fresh_text();
    let first = send_text(&client, ALICE, &text).await.unwrap();
    assert_eq!(first["status"], "sent");
    let err = send_text(&client, ALICE, &text)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("already sent: message {FIRST_SENT_ID}")),
        "{err}"
    );
    // The same as the chat shows it: Markdown that formats nothing is plain.
    let plain = json!({ "chat_id": fake_issued_id(ALICE), "text": "see you", "format": "plain" });
    assert_eq!(call(&client, plain).await.unwrap()["status"], "sent");
    let err = send_text(&client, ALICE, "see you")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already sent"), "{err}");

    // Another chat, or another text, is another message.
    let in_general = json!({ "chat_id": fake_issued_id(FORUM), "text": text, "topic": 1 });
    assert_eq!(call(&client, in_general).await.unwrap()["status"], "sent");
    let more = format!("{text}!");
    assert_eq!(
        send_text(&client, ALICE, &more).await.unwrap()["status"],
        "sent"
    );
    assert_eq!(server.connector.sent().of(ME).len(), 4);
    // Only a hash, and only of what was sent.
    assert_eq!(server.auth.confirmations.recent_sends().len(), 4);
}

#[tokio::test]
async fn a_confirmed_send_is_already_sent_too_however_it_was_confirmed() {
    let server = server(false).await;
    // Through the link the call returned.
    let client = connect(&server, App::new(None)).await;
    let text = fresh_text();
    let pending = send_text(&client, ALICE, &text).await.unwrap();
    assert_eq!(pending["status"], "pending_confirmation");
    // Not sent yet: asking again is another pending send, not a duplicate.
    let again = send_text(&client, ALICE, &text).await.unwrap();
    assert_eq!(again["status"], "pending_confirmation");
    let url = pending["confirm_url"].as_str().unwrap().to_string();
    let html = press(url, "send", Some(server.cookie.clone())).await;
    assert!(html.contains("The message was sent"), "{html}");
    let err = send_text(&client, ALICE, &text)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("already sent: message {FIRST_SENT_ID}")),
        "{err}"
    );

    // In the page the client opened, while the call waited.
    let app = App {
        press: Some(("send", Some(server.cookie.clone()))),
        ..App::with_url_elicitation()
    };
    let client = connect(&server, app.clone()).await;
    let text = fresh_text();
    assert_eq!(
        send_text(&client, ALICE, &text).await.unwrap()["status"],
        "sent"
    );
    let err = send_text(&client, ALICE, &text)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already sent"), "{err}");
    assert_eq!(app.opened().len(), 1, "the duplicate asked nobody");
    assert_eq!(server.connector.sent().of(ME).len(), 2);
}

#[tokio::test]
async fn a_refused_send_says_when_to_retry_and_holds_nothing_back() {
    let server = server(true).await;
    let client = connect(&server, App::new(None)).await;
    let text = fresh_text();
    for (refusal, says) in [
        (
            TgError::WritesLimited {
                seconds: 42,
                limit: WriteLimit::Chat,
            },
            "Too many messages to this chat right now; retry in 42 s",
        ),
        (
            TgError::SlowMode { seconds: 17 },
            "slow mode: each member may send one message per set interval; retry in 17 s",
        ),
        (TgError::PeerFlood, "PEER_FLOOD"),
    ] {
        server.connector.sent().refuse_next(refusal);
        let err = send_text(&client, ALICE, &text)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(says), "{says}: {err}");
        assert!(server.connector.sent().of(ME).is_empty());
    }
    // Nothing was sent, so nothing is a duplicate.
    assert!(server.auth.confirmations.recent_sends().is_empty());
    assert_eq!(
        send_text(&client, ALICE, &text).await.unwrap()["status"],
        "sent"
    );
}

#[tokio::test]
async fn text_and_forum_rules_are_save_drafts() {
    let server = server(true).await;
    let client = connect(&server, App::new(None)).await;
    let forum = fake_issued_id(FORUM);
    let alice = fake_issued_id(ALICE);
    let refused = |arguments: Value| {
        let client = &client;
        async move { call(client, arguments).await.unwrap_err().to_string() }
    };

    let msg = refused(json!({ "chat_id": forum, "text": fresh_text() })).await;
    assert!(
        msg.contains("is a forum") && msg.contains("General is 1"),
        "{msg}"
    );
    let msg = refused(json!({ "chat_id": alice, "text": fresh_text(), "topic": 1 })).await;
    assert!(msg.contains("not a forum"), "{msg}");
    let msg =
        refused(json!({ "chat_id": forum, "text": fresh_text(), "reply_to": 12, "topic": 1 }))
            .await;
    assert!(msg.contains("in Topic 10, not in topic 1"), "{msg}");
    let msg = refused(json!({ "chat_id": alice, "text": fresh_text(), "reply_to": 0 })).await;
    assert!(msg.contains("reply_to must be a positive id"), "{msg}");
    let msg = refused(json!({ "chat_id": alice, "text": "x".repeat(4097) })).await;
    assert!(msg.contains("4097") && msg.contains("not split"), "{msg}");
    for blank in ["", "  \n"] {
        let msg = refused(json!({ "chat_id": alice, "text": blank })).await;
        assert!(msg.contains("a message needs some text"), "{msg}");
    }
    assert!(server.connector.sent().of(ME).is_empty());

    // In a Topic, a reply that finds its Topic, and plain text as given.
    let in_topic = call(
        &client,
        json!({ "chat_id": forum, "text": fresh_text(), "topic": 10 }),
    )
    .await
    .unwrap();
    assert_eq!(
        (in_topic["status"].as_str(), in_topic["topic"].as_i64()),
        (Some("sent"), Some(10))
    );
    let reply = call(
        &client,
        json!({ "chat_id": forum, "text": fresh_text(), "reply_to": 12 }),
    )
    .await
    .unwrap();
    assert_eq!(reply["topic"], 10, "{reply}");
    assert_eq!(reply["reply_to"], 12, "{reply}");
    let plain = call(
        &client,
        json!({ "chat_id": alice, "text": "**as is**", "format": "plain" }),
    )
    .await
    .unwrap();
    assert_eq!(plain["status"], "sent");
    let sent = server.connector.sent().of(ME);
    assert_eq!(sent.len(), 3);
    assert_eq!((sent[0].topic, sent[0].reply_to), (Some(10), None));
    assert_eq!((sent[1].topic, sent[1].reply_to), (Some(10), Some(12)));
    assert_eq!(sent[2].text.text, "**as is**");
    assert!(sent[2].text.entities.is_empty());
}

#[tokio::test]
async fn without_auto_send_a_message_that_cant_go_there_never_waits() {
    let server = server(false).await;
    let app = App::with_url_elicitation();
    let client = connect(&server, app.clone()).await;
    let msg = call(
        &client,
        json!({ "chat_id": fake_issued_id(FORUM), "text": fresh_text() }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(msg.contains("is a forum"), "{msg}");
    assert!(app.opened().is_empty());
    assert!(server.auth.confirmations.is_empty());
}
