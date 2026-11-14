//! The `send` Scope's tools through `/mcp` with rmcp's own client, on the
//! fake Telegram: send_message (#46) sends at once (ADR-0008), never the same
//! text twice within a minute, with save_draft's text and forum rules; react
//! and mark_read (#47) a standard emoji only, a Topic only in a forum. All
//! only into the Send allow-list.

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
    limits::WriteLimit,
    links::MediaLinks,
    scope::{Scope, Scopes},
    sessions::{TokenTtl, Vault},
    tg::{
        Account, Connector, FakeConnector, TgError,
        chats::{ChatId, ChatKind, ChatSummary},
        fake::{FIRST_SENT_ID, FakeData, FakeTopic, fake_issued_id},
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
        .then(|| Arc::new(SendAllowList::new([ALICE, FORUM, STRICT, QUIET]).unwrap()));
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

async fn send_message(server: &Server, arguments: Value) -> Result<Value, (i32, String)> {
    call(server, "send_message", arguments).await
}

async fn send_text(server: &Server, chat: ChatId, text: &str) -> Result<Value, (i32, String)> {
    send_message(
        server,
        json!({ "chat_id": fake_issued_id(chat), "text": text }),
    )
    .await
}

/// The error's message, whatever kind of error it is.
fn refused(result: Result<Value, (i32, String)>) -> String {
    match result {
        Err((_, msg)) => msg,
        Ok(value) => panic!("expected an error, got {value}"),
    }
}

#[tokio::test]
async fn a_message_is_sent_at_once_and_only_into_the_allow_list() {
    let server = sending().await;
    let sent = send_text(&server, ALICE, "**hi** Alice").await.unwrap();
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(sent["message_id"], FIRST_SENT_ID);
    assert_eq!(sent["chat_id"], fake_issued_id(ALICE));
    assert!(sent["date"].as_str().unwrap().ends_with('Z'), "{sent}");
    for gone in ["confirm_url", "expires_at"] {
        assert!(sent.get(gone).is_none(), "{gone}: {sent}");
    }
    let messages = server.connector.sent().of(ME);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].chat, ALICE);
    assert_eq!(messages[0].text.text, "hi Alice");
    assert_eq!(messages[0].text.entities.len(), 1, "the bold");

    let msg = invalid_params(send_text(&server, BOB, "hi Bob").await);
    assert!(
        msg.contains("user:102 is not one of the chats this connection may send to"),
        "{msg}"
    );
    assert_eq!(server.connector.sent().of(ME).len(), 1);
}

#[tokio::test]
async fn the_same_text_to_the_same_chat_within_a_minute_is_already_sent() {
    let server = sending().await;
    let first = send_text(&server, ALICE, "see you").await.unwrap();
    assert_eq!(first["status"], "sent");
    let msg = refused(send_text(&server, ALICE, "see you").await);
    assert!(
        msg.contains(&format!("already sent: message {FIRST_SENT_ID}")),
        "{msg}"
    );
    // The same as the chat shows it: Markdown that formats nothing is plain.
    let plain = json!({ "chat_id": fake_issued_id(ALICE), "text": "see you", "format": "plain" });
    assert!(refused(send_message(&server, plain).await).contains("already sent"));

    // Another chat, or another text, is another message.
    let in_general = json!({ "chat_id": fake_issued_id(FORUM), "text": "see you", "topic": 1 });
    assert_eq!(
        send_message(&server, in_general).await.unwrap()["status"],
        "sent"
    );
    assert_eq!(
        send_text(&server, ALICE, "see you!").await.unwrap()["status"],
        "sent"
    );
    assert_eq!(server.connector.sent().of(ME).len(), 3);
}

#[tokio::test]
async fn a_refused_send_says_when_to_retry_and_holds_nothing_back() {
    let server = sending().await;
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
        let msg = refused(send_text(&server, ALICE, "once more").await);
        assert!(msg.contains(says), "{says}: {msg}");
        assert!(server.connector.sent().of(ME).is_empty());
    }
    // Nothing was sent, so nothing is a duplicate.
    assert_eq!(
        send_text(&server, ALICE, "once more").await.unwrap()["status"],
        "sent"
    );
}

#[tokio::test]
async fn text_and_forum_rules_are_save_drafts() {
    let server = sending().await;
    let forum = fake_issued_id(FORUM);
    let alice = fake_issued_id(ALICE);

    let msg = refused(send_message(&server, json!({ "chat_id": forum, "text": "a" })).await);
    assert!(
        msg.contains("is a forum") && msg.contains("General is 1"),
        "{msg}"
    );
    let msg = refused(
        send_message(
            &server,
            json!({ "chat_id": alice, "text": "b", "topic": 1 }),
        )
        .await,
    );
    assert!(msg.contains("not a forum"), "{msg}");
    let msg = refused(
        send_message(
            &server,
            json!({ "chat_id": forum, "text": "c", "reply_to": 12, "topic": 1 }),
        )
        .await,
    );
    assert!(msg.contains("in Topic 10, not in topic 1"), "{msg}");
    let msg = refused(
        send_message(
            &server,
            json!({ "chat_id": alice, "text": "d", "reply_to": 0 }),
        )
        .await,
    );
    assert!(msg.contains("reply_to must be a positive id"), "{msg}");
    let msg = refused(
        send_message(
            &server,
            json!({ "chat_id": alice, "text": "x".repeat(4097) }),
        )
        .await,
    );
    assert!(msg.contains("4097") && msg.contains("not split"), "{msg}");
    for blank in ["", "  \n"] {
        let msg = refused(send_message(&server, json!({ "chat_id": alice, "text": blank })).await);
        assert!(msg.contains("a message needs some text"), "{msg}");
    }
    assert!(server.connector.sent().of(ME).is_empty());

    // In a Topic, a reply that finds its Topic, and plain text as given.
    let in_topic = send_message(
        &server,
        json!({ "chat_id": forum, "text": "e", "topic": 10 }),
    )
    .await
    .unwrap();
    assert_eq!(
        (in_topic["status"].as_str(), in_topic["topic"].as_i64()),
        (Some("sent"), Some(10))
    );
    let reply = send_message(
        &server,
        json!({ "chat_id": forum, "text": "f", "reply_to": 12 }),
    )
    .await
    .unwrap();
    assert_eq!(reply["topic"], 10, "{reply}");
    assert_eq!(reply["reply_to"], 12, "{reply}");
    let plain = send_message(
        &server,
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
async fn send_message_is_listed_as_a_bounded_destructive_open_world_write() {
    let server = sending().await;
    let tools = server.client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "send_message")
        .expect("listed for a send Grant");
    let hints = tool.annotations.as_ref().unwrap();
    assert_eq!(hints.read_only_hint, Some(false));
    assert_eq!(hints.idempotent_hint, Some(false));
    // So that clients ask the user by default (ADR-0008); nothing here
    // relies on it.
    assert_eq!(hints.destructive_hint, Some(true), "it can't be unsent");
    assert_eq!(hints.open_world_hint, Some(true), "others read it");
    let description = tool.description.as_deref().unwrap();
    for words in [
        "Send a message as the user",
        "sent at once",
        "already sent: message N",
        "never retry it on your own",
    ] {
        assert!(description.contains(words), "{words}: {description}");
    }
    for gone in ["confirm", "pending", "Draft"] {
        assert!(!description.contains(gone), "{gone}: {description}");
    }
    let input = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
    assert_eq!(input["properties"]["text"]["maxLength"], 16_384);
    assert_eq!(input["properties"]["text"]["minLength"], 1);
    assert!(input["properties"]["chat_id"]["maxLength"].is_u64());
    assert_eq!(input["required"], json!(["chat_id", "text"]));
    let output = serde_json::to_value(tool.output_schema.as_ref().unwrap().as_ref()).unwrap();
    let statuses: Vec<&Value> = output["$defs"]["SendStatus"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|status| &status["const"])
        .collect();
    assert_eq!(statuses, [&json!("sent")], "{output}");
    assert_eq!(
        output["required"],
        json!(["status", "chat_id", "message_id", "date"]),
        "{output}"
    );
    assert!(
        output["properties"].get("confirm_url").is_none(),
        "{output}"
    );
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
async fn both_are_listed_as_bounded_idempotent_writes_others_see() {
    let server = sending().await;
    let tools = server.client.list_all_tools().await.unwrap();
    for name in ["react", "mark_read"] {
        let tool = tools.iter().find(|t| t.name == name).expect(name);
        let hints = tool.annotations.as_ref().unwrap();
        assert_eq!(hints.read_only_hint, Some(false), "{name}");
        assert_eq!(hints.destructive_hint, Some(false), "{name}");
        assert_eq!(hints.idempotent_hint, Some(true), "{name}");
        assert_eq!(hints.open_world_hint, Some(true), "{name}");
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
