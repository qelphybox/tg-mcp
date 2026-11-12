//! Delivery through `/mcp` with rmcp's own client: Auto-send sends at once;
//! a client with URL elicitation is asked to open the confirmation page and
//! the call returns the user's decision; any other gets the link.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
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
        fake::{FIRST_SENT_ID, FakeData, fake_issued_id},
        markdown::markdown,
    },
};

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
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
    FakeData::new(Account {
        id: ME,
        first_name: "Me".into(),
        last_name: None,
        username: None,
    })
    .with_chats(vec![chat(ALICE, "Alice"), chat(BOB, "Bob")])
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
    let list = SendAllowList::new([ALICE], auto_send)
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
        scopes: grant.scopes,
        send_allow_list: grant.send_allow_list.clone().map(Arc::new),
        telegram: connector.connect(grant).await.unwrap(),
    };

    let stop = tokio_util::sync::CancellationToken::new();
    let (links, confirmations) = (auth.links.clone(), auth.confirmations.clone());
    let mcp = StreamableHttpService::new(
        move || {
            let tools = TgMcp::scoped_tools().with(Scope::Send, TgMcp::test_send_tools());
            Ok(TgMcp::with_tools(links.clone(), tools, false)
                .with_confirmations(confirmations.clone()))
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

async fn send(
    client: &RunningService<RoleClient, App>,
    chat: ChatId,
) -> Result<Value, rmcp::ServiceError> {
    let arguments = json!({ "chat_id": fake_issued_id(chat), "text": "**hi** Alice" });
    client
        .call_tool(
            CallToolRequestParams::new("test_send")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| {
            assert_eq!(r.is_error, Some(false), "{r:?}");
            r.structured_content.expect("structured output")
        })
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
    assert_eq!(sent[0].text, markdown("**hi** Alice"));
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
