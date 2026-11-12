//! react and mark_read (#47) through `/mcp` with rmcp's own client, on the
//! fake Telegram: only into the Send allow-list, never confirmed, a standard
//! emoji only, a Topic only in a forum.

use std::{net::SocketAddr, sync::Arc};

use axum::{extract::Request, middleware::Next};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientConfig, ErrorCode},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde_json::{Value, json};

use super::TgMcp;
use crate::{
    allow_list::SendAllowList,
    auth::Caller,
    links::MediaLinks,
    scope::{Scope, Scopes},
    sessions::{TokenTtl, Vault},
    tg::{
        Account, Connector, FakeConnector,
        chats::{ChatId, ChatKind, ChatSummary},
        fake::{FakeData, FakeTopic, fake_issued_id},
        mark_read::MarkedRead,
        messages::RawHistoryMessage,
    },
};

const ME: i64 = 42;
/// In the Send allow-list, with messages 5 and 6.
const ALICE: ChatId = ChatId::User(101);
/// One of the Account's chats, not in the Send allow-list.
const BOB: ChatId = ChatId::User(102);
/// A forum in the Send allow-list: General (1) with message 5, Releases (10)
/// with messages 10 and 12.
const FORUM: ChatId = ChatId::Channel(505);
/// A channel in the Send allow-list whose admins allow only 👍.
const STRICT: ChatId = ChatId::Channel(506);
/// A group in the Send allow-list with reactions off.
const QUIET: ChatId = ChatId::Chat(507);

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
    let chat = |id: ChatId, kind| ChatSummary {
        id: id.to_string(),
        title: "a chat".into(),
        kind,
        username: None,
        is_self: false,
        unread_count: 3,
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
        chat(ALICE, ChatKind::Private),
        chat(BOB, ChatKind::Private),
        chat(FORUM, ChatKind::Supergroup),
        chat(STRICT, ChatKind::Channel),
        chat(QUIET, ChatKind::Group),
    ])
    .with_messages(ALICE, vec![raw(5, None), raw(6, None)])
    .with_messages(BOB, vec![raw(7, None)])
    .with_messages(
        FORUM,
        vec![raw(5, None), raw(10, Some(10)), raw(12, Some(10))],
    )
    .with_topics(
        FORUM,
        vec![FakeTopic::new(10, "Releases"), FakeTopic::new(1, "General")],
    )
    .with_messages(STRICT, vec![raw(3, None)])
    .with_messages(QUIET, vec![raw(4, None)])
    .with_allowed_reactions(STRICT, &["👍"])
    .with_allowed_reactions(QUIET, &[])
}

type Client = RunningService<RoleClient, ClientConfig>;

/// `/mcp` for one Grant with `scopes`, its Send allow-list Alice and the
/// forum, the strict channel and the quiet group (when it has `send`).
struct Server {
    client: Client,
    connector: FakeConnector,
    _stop: tokio_util::sync::DropGuard,
    _dir: tempfile::TempDir,
}

async fn server(scopes: Scopes) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_path(&dir.path().join("t.db"), TokenTtl::default())
        .await
        .unwrap();
    let issued = vault
        .create_grant_with_scopes("client", ME, &serde_json::to_vec(&data()).unwrap(), scopes)
        .await
        .unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();
    let connector = FakeConnector::default();
    let send_allow_list = scopes
        .contains(Scope::Send)
        .then(|| Arc::new(SendAllowList::new([ALICE, FORUM, STRICT, QUIET], false).unwrap()));
    let caller = Caller {
        grant_id: grant.grant_id.clone(),
        account_id: grant.account_id,
        scopes: grant.scopes,
        send_allow_list,
        telegram: connector.connect(grant).await.unwrap(),
    };

    let links = Arc::new(MediaLinks::new("http://127.0.0.1"));
    let stop = tokio_util::sync::CancellationToken::new();
    let mcp = StreamableHttpService::new(
        move || {
            Ok(TgMcp::with_tools(
                links.clone(),
                TgMcp::scoped_tools(),
                false,
            ))
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
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let serving = stop.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(serving.cancelled_owned())
            .await
            .unwrap();
    });
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client = ClientConfig::default()
        .serve(transport)
        .await
        .expect("initialize");
    Server {
        client,
        connector,
        _stop: stop.drop_guard(),
        _dir: dir,
    }
}

async fn sending() -> Server {
    server([Scope::Send].into_iter().collect()).await
}

/// `tool` with `arguments`: its structured result, or the error's code and
/// message.
async fn call(
    server: &Server,
    tool: &'static str,
    arguments: Value,
) -> Result<Value, (i32, String)> {
    match server
        .client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
    {
        // What rmcp says of arguments it can't read.
        Ok(result) if result.is_error == Some(true) => Err((0, format!("{:?}", result.content))),
        Ok(result) => Ok(result.structured_content.expect("structured output")),
        Err(rmcp::ServiceError::McpError(err)) => Err((err.code.0, err.message.to_string())),
        Err(other) => panic!("{other}"),
    }
}

async fn react(
    server: &Server,
    chat: ChatId,
    message_id: i32,
    emoji: Option<&str>,
) -> Result<Value, (i32, String)> {
    call(
        server,
        "react",
        json!({ "chat_id": fake_issued_id(chat), "message_id": message_id, "emoji": emoji }),
    )
    .await
}

async fn mark_read(server: &Server, chat: ChatId, extra: Value) -> Result<Value, (i32, String)> {
    let mut arguments = json!({ "chat_id": fake_issued_id(chat) });
    arguments
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    call(server, "mark_read", arguments).await
}

fn invalid_params(result: Result<Value, (i32, String)>) -> String {
    match result {
        Err((code, msg)) => {
            assert_eq!(code, ErrorCode::INVALID_PARAMS.0, "{msg}");
            msg
        }
        Ok(value) => panic!("expected invalid params, got {value}"),
    }
}

#[tokio::test]
async fn a_reaction_is_set_replaced_and_removed_without_confirmation() {
    let server = sending().await;
    let reactions = server.connector.reactions();

    let set = react(&server, ALICE, 5, Some("👍")).await.unwrap();
    assert_eq!(
        set,
        json!({
            "status": "set",
            "chat_id": fake_issued_id(ALICE),
            "message_id": 5,
            "emoji": "👍"
        })
    );
    assert_eq!(reactions.on(ME, ALICE, 5).as_deref(), Some("👍"));

    // Replaced; Telegram's own spelling of ❤️ comes back.
    let replaced = react(&server, ALICE, 5, Some("❤️")).await.unwrap();
    assert_eq!(replaced["emoji"], "❤");
    assert_eq!(reactions.on(ME, ALICE, 5).as_deref(), Some("❤"));
    // The same again is no error.
    assert_eq!(react(&server, ALICE, 5, Some("❤")).await.unwrap(), replaced);
    // Another message keeps its own.
    assert_eq!(reactions.on(ME, ALICE, 6), None);

    let removed = react(&server, ALICE, 5, None).await.unwrap();
    assert_eq!(
        removed,
        json!({ "status": "removed", "chat_id": fake_issued_id(ALICE), "message_id": 5 })
    );
    assert_eq!(reactions.on(ME, ALICE, 5), None);
    assert_eq!(react(&server, ALICE, 5, None).await.unwrap(), removed);
    assert_eq!(reactions.calls(ME), 5);
}

#[tokio::test]
async fn custom_emoji_and_paid_reactions_are_invalid_params_and_never_reach_telegram() {
    let server = sending().await;
    for custom in ["custom", "5368324170671202286"] {
        let msg = invalid_params(react(&server, ALICE, 5, Some(custom)).await);
        assert!(
            msg.contains("custom emoji reactions are not supported"),
            "{msg}"
        );
    }
    let msg = invalid_params(react(&server, ALICE, 5, Some("paid")).await);
    assert!(msg.contains("paid reactions"), "{msg}");
    let msg = invalid_params(react(&server, ALICE, 5, Some("🦖")).await);
    assert!(
        msg.contains("not one of Telegram's standard reactions"),
        "{msg}"
    );
    assert!(msg.contains("👍 👎 ❤"), "{msg}");
    assert_eq!(server.connector.reactions().calls(ME), 0);
}

#[tokio::test]
async fn emoji_must_be_given_null_to_remove() {
    let server = sending().await;
    let missing = call(
        &server,
        "react",
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 5 }),
    )
    .await;
    let Err((_, msg)) = missing else {
        panic!("a missing emoji removes nothing: {missing:?}")
    };
    assert!(msg.contains("missing field `emoji`"), "{msg}");
    assert_eq!(server.connector.reactions().calls(ME), 0);
}

#[tokio::test]
async fn a_chat_that_restricts_reactions_refuses_the_others_clearly() {
    let server = sending().await;
    let msg = invalid_params(react(&server, STRICT, 3, Some("❤")).await);
    assert!(
        msg.contains("channel:506 doesn't allow the reaction ❤ (REACTION_INVALID)"),
        "{msg}"
    );
    react(&server, STRICT, 3, Some("👍")).await.unwrap();

    let msg = invalid_params(react(&server, QUIET, 4, Some("👍")).await);
    assert!(msg.contains("turned reactions off there"), "{msg}");

    let msg = invalid_params(react(&server, ALICE, 99, Some("👍")).await);
    assert!(
        msg.contains("message 99 is not a message of user:101"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_chat_is_marked_read_up_to_its_latest_message_or_up_to() {
    let server = sending().await;
    let latest = mark_read(&server, ALICE, json!({})).await.unwrap();
    assert_eq!(latest, json!({ "chat_id": fake_issued_id(ALICE) }));
    let up_to = mark_read(&server, ALICE, json!({ "up_to": 5 }))
        .await
        .unwrap();
    assert_eq!(
        up_to,
        json!({ "chat_id": fake_issued_id(ALICE), "up_to": 5 })
    );
    // Again: no error.
    mark_read(&server, ALICE, json!({ "up_to": 5 }))
        .await
        .unwrap();
    assert_eq!(
        server.connector.reads().of(ME),
        [
            MarkedRead {
                chat: ALICE,
                topic: None,
                up_to: None
            },
            MarkedRead {
                chat: ALICE,
                topic: None,
                up_to: Some(5)
            },
            MarkedRead {
                chat: ALICE,
                topic: None,
                up_to: Some(5)
            },
        ]
    );
}

#[tokio::test]
async fn in_a_forum_only_the_topic_is_marked_read() {
    let server = sending().await;
    let topic = mark_read(&server, FORUM, json!({ "topic": 10 }))
        .await
        .unwrap();
    assert_eq!(
        topic,
        json!({ "chat_id": fake_issued_id(FORUM), "topic": 10, "up_to": 12 }),
        "up to the Topic's latest message"
    );
    let general = mark_read(&server, FORUM, json!({ "topic": 1, "up_to": 5 }))
        .await
        .unwrap();
    assert_eq!(general["up_to"], 5);
    assert_eq!(general["topic"], 1);

    let msg = invalid_params(mark_read(&server, FORUM, json!({ "topic": 99 })).await);
    assert!(
        msg.contains("topic 99 is not a Topic of this forum"),
        "{msg}"
    );
    let msg = invalid_params(mark_read(&server, ALICE, json!({ "topic": 1 })).await);
    assert!(msg.contains("is not a forum"), "{msg}");
    assert_eq!(server.connector.reads().of(ME).len(), 2);
}

#[tokio::test]
async fn a_chat_outside_the_send_allow_list_is_refused_before_telegram() {
    let server = sending().await;
    let msg = invalid_params(react(&server, BOB, 7, Some("👍")).await);
    assert!(
        msg.contains("user:102 is not one of the chats this connection may send to"),
        "{msg}"
    );
    let msg = invalid_params(mark_read(&server, BOB, json!({})).await);
    assert!(msg.contains("Send allow-list"), "{msg}");
    assert_eq!(server.connector.reactions().calls(ME), 0);
    assert!(server.connector.reads().of(ME).is_empty());
}

#[tokio::test]
async fn without_send_neither_is_listed_nor_callable() {
    let server = server([Scope::Draft].into_iter().collect()).await;
    let tools = server.client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "save_draft"));
    assert!(
        !tools
            .iter()
            .any(|t| t.name == "react" || t.name == "mark_read")
    );
    let msg = invalid_params(react(&server, ALICE, 5, Some("👍")).await);
    assert!(msg.contains("react needs the `send` Scope"), "{msg}");
    let msg = invalid_params(mark_read(&server, ALICE, json!({})).await);
    assert!(msg.contains("mark_read needs the `send` Scope"), "{msg}");
    assert_eq!(server.connector.reactions().calls(ME), 0);
    assert!(server.connector.reads().of(ME).is_empty());
}

#[tokio::test]
async fn both_are_listed_as_bounded_idempotent_writes() {
    let server = sending().await;
    let tools = server.client.list_all_tools().await.unwrap();
    for name in ["react", "mark_read"] {
        let tool = tools.iter().find(|t| t.name == name).expect(name);
        let hints = tool.annotations.as_ref().unwrap();
        assert_eq!(hints.read_only_hint, Some(false), "{name}");
        assert_eq!(hints.destructive_hint, Some(false), "{name}");
        assert_eq!(hints.idempotent_hint, Some(true), "{name}");
        assert_eq!(hints.open_world_hint, Some(false), "{name}");
        assert!(tool.output_schema.is_some(), "{name}");
    }
    let react = tools.iter().find(|t| t.name == "react").unwrap();
    let input = serde_json::to_value(&*react.input_schema).unwrap();
    let required: Vec<&str> = input["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(required.contains(&"emoji"), "{input}");
    let emoji = &input["properties"]["emoji"];
    assert_eq!(emoji["type"], json!(["string", "null"]), "{input}");
    assert_eq!(emoji["maxLength"], 16, "{input}");
    assert!(
        emoji["description"]
            .as_str()
            .unwrap()
            .contains("null removes")
    );
    assert_eq!(
        input["properties"]["chat_id"]["maxLength"],
        crate::tg::chats::CHAT_REF_MAX_LEN,
        "{input}"
    );
}
