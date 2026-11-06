//! `get_messages` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, fake_issued_id},
    messages::RawHistoryMessage,
};

const ALICE: ChatId = ChatId::User(101);

/// Alice's id as list_chats hands it out, access hash included.
fn alice_id() -> String {
    fake_issued_id(ALICE)
}
/// 2026-11-06T00:00:00Z
const DAY: i32 = 1_793_923_200;

fn alice_chat() -> ChatSummary {
    ChatSummary {
        id: ALICE.to_string(),
        title: "Alice".into(),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        last_message: None,
    }
}

/// Message `id` sent `id` hours into DAY; even ids are ours.
fn message(id: i32, text: &str) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id * 3600,
        from: None,
        out: id % 2 == 0,
        text: text.into(),
        reply_to: None,
        forwarded: false,
        edited: false,
    }
}

async fn client_with(
    messages: Vec<RawHistoryMessage>,
) -> (
    common::TestServer,
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![alice_chat()])
        .with_messages(ALICE, messages);
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    arguments: Value,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("get_messages")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

fn ids(page: &Value) -> Vec<i64> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn pages_walk_back_through_history_without_gaps() {
    let (_server, client) =
        client_with((1..=7).map(|i| message(i, &format!("m{i}"))).collect()).await;

    let mut seen = Vec::new();
    let mut before: Option<i64> = None;
    for _ in 0..10 {
        let mut args = json!({ "chat_id": alice_id(), "limit": 3 });
        if let Some(b) = before {
            args["before_id"] = json!(b);
        }
        let page = call(&client, args).await.unwrap();
        seen.extend(ids(&page));
        match page.get("next_before_id").and_then(Value::as_i64) {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, [7, 6, 5, 4, 3, 2, 1]);
}

#[tokio::test]
async fn messages_say_who_wrote_them() {
    let mut reply = message(3, "re: hi");
    reply.reply_to = Some(1);
    reply.edited = true;
    let (_server, client) = client_with(vec![message(1, "hi"), message(2, "hello"), reply]).await;

    let page = call(&client, json!({ "chat_id": alice_id() }))
        .await
        .unwrap();
    assert_eq!(page["chat_id"], alice_id());
    assert_eq!(page.get("next_before_id"), None);
    assert_eq!(
        page["messages"],
        json!([
            { "id": 3, "date": "2026-11-06T03:00:00Z", "sender": { "id": alice_id(), "name": "Alice" },
              "text": "re: hi", "reply_to": 1, "edited": true },
            { "id": 2, "date": "2026-11-06T02:00:00Z", "outgoing": true, "text": "hello" },
            { "id": 1, "date": "2026-11-06T01:00:00Z", "sender": { "id": alice_id(), "name": "Alice" },
              "text": "hi" },
        ])
    );
}

#[tokio::test]
async fn ids_from_list_chats_and_legacy_ids_both_work() {
    let (_server, client) = client_with(vec![message(1, "hi")]).await;
    let listed = client
        .call_tool(CallToolRequestParams::new("list_chats"))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let issued = listed["chats"][0]["id"].as_str().unwrap();
    assert!(issued.starts_with("user:101~"), "{issued}");

    let page = call(&client, json!({ "chat_id": issued })).await.unwrap();
    assert_eq!(ids(&page), [1]);
    assert_eq!(page["chat_id"], issued);

    // An id from before ids carried the hash, still in some AI client's context.
    let page = call(&client, json!({ "chat_id": "user:101" }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [1]);
    assert_eq!(page["chat_id"], issued, "answers hand out the hashed id");
}

#[tokio::test]
async fn since_and_until_narrow_the_window() {
    let (_server, client) = client_with((1..=10).map(|i| message(i, "x")).collect()).await;

    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "since": "2026-11-06T03:00:00Z", "until": "2026-11-06T06:30:00+00:00" }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [6, 5, 4, 3]);
    assert_eq!(page.get("next_before_id"), None);

    // A plain date as `until` means the whole day.
    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "until": "2026-11-06" }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page).len(), 10);
}

#[tokio::test]
async fn the_page_budget_ends_a_page_early_without_losing_messages() {
    let long = "y".repeat(5000);
    let (_server, client) = client_with((1..=100).map(|i| message(i, &long)).collect()).await;

    let first = call(&client, json!({ "chat_id": alice_id(), "limit": 100 }))
        .await
        .unwrap();
    let first_ids = ids(&first);
    assert!(
        first_ids.len() < 100,
        "a 100 x 1000-char page must be cut by the budget"
    );
    assert!(
        first["messages"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            == 1000
    );
    let next = first["next_before_id"].as_i64().expect("more to read");
    assert_eq!(next, *first_ids.last().unwrap());

    let second = call(
        &client,
        json!({ "chat_id": alice_id(), "limit": 100, "before_id": next }),
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&second)[0],
        next - 1,
        "the next page starts right after the cut"
    );
}

#[tokio::test]
async fn bad_arguments_are_invalid_params() {
    let (_server, client) = client_with(vec![message(1, "hi")]).await;
    for bad in [
        json!({ "chat_id": "alice" }),
        json!({ "chat_id": "user:999" }),
        // A tampered access hash: Telegram refuses it, the tool says so.
        json!({ "chat_id": "user:101~AAAAAAAAAAA" }),
        json!({ "chat_id": format!("{}x", alice_id()) }),
        json!({ "chat_id": alice_id(), "limit": 0 }),
        json!({ "chat_id": alice_id(), "limit": 101 }),
        json!({ "chat_id": alice_id(), "before_id": 0 }),
        json!({ "chat_id": alice_id(), "since": "yesterday" }),
        json!({ "chat_id": alice_id(), "since": "2026-11-07", "until": "2026-11-06" }),
    ] {
        match call(&client, bad.clone()).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}: {err:?}")
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
}

#[tokio::test]
async fn tool_is_read_only_and_requires_a_chat() {
    let (_server, client) = client_with(vec![]).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "get_messages")
        .expect("get_messages listed");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tool.output_schema.is_some());
    assert_eq!(tool.input_schema["required"], json!(["chat_id"]));
    assert_eq!(tool.input_schema["properties"]["limit"]["maximum"], 100);
}
