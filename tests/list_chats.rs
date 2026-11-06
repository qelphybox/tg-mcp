//! `list_chats` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatKind, ChatSummary, MessagePreview},
    fake::FakeData,
};

fn chat(n: usize) -> ChatSummary {
    ChatSummary {
        id: format!("user:{n}"),
        title: format!("Friend {n}"),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: n as i32,
        pinned: false,
        last_message: Some(MessagePreview {
            date: "2026-11-06T12:00:00Z".into(),
            text: format!("hello {n}"),
        }),
    }
}

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    arguments: Value,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(CallToolRequestParams::new("list_chats").with_arguments(args(arguments)))
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

#[tokio::test]
async fn pages_cover_every_chat_exactly_once() {
    let chats: Vec<ChatSummary> = (1..=7).map(chat).collect();
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut request = json!({ "limit": 3 });
        if let Some(c) = &cursor {
            request["cursor"] = json!(c);
        }
        let page = call(&client, request).await.unwrap();
        pages += 1;
        for chat in page["chats"].as_array().unwrap() {
            seen.push(chat["id"].as_str().unwrap().to_string());
        }
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
        assert!(pages < 10, "pagination never ends");
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        (1..=7).map(|n| format!("user:{n}")).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn first_page_has_the_documented_shape() {
    let server = start_with(vec![
        FakeData::new(account(42, "Alice")).with_chats(vec![chat(1)]),
    ])
    .await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let page = call(&client, json!({})).await.unwrap();
    assert_eq!(
        page,
        json!({ "chats": [{
            "id": "user:1",
            "title": "Friend 1",
            "type": "private",
            "unread_count": 1,
            "last_message": { "date": "2026-11-06T12:00:00Z", "text": "hello 1" }
        }]})
    );
}

#[tokio::test]
async fn long_titles_and_previews_are_cut() {
    let mut noisy = chat(1);
    noisy.title = "T".repeat(500);
    noisy.last_message.as_mut().unwrap().text = format!("first line\n{}", "x".repeat(5000));
    let server = start_with(vec![
        FakeData::new(account(42, "Alice")).with_chats(vec![noisy]),
    ])
    .await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let page = call(&client, json!({})).await.unwrap();
    let title = page["chats"][0]["title"].as_str().unwrap();
    let preview = page["chats"][0]["last_message"]["text"].as_str().unwrap();
    assert_eq!(title.chars().count(), 128);
    assert_eq!(preview.chars().count(), 100);
    assert!(
        preview.starts_with("first line x") && preview.ends_with('…'),
        "{preview}"
    );
}

#[tokio::test]
async fn bad_arguments_are_invalid_params() {
    let server = start_with(vec![
        FakeData::new(account(42, "Alice")).with_chats(vec![chat(1)]),
    ])
    .await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    for bad in [
        json!({ "limit": 0 }),
        json!({ "limit": 101 }),
        json!({ "cursor": "made-up" }),
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
async fn tool_is_read_only_and_bounded_in_its_schema() {
    let server = start_with(vec![FakeData::new(account(42, "Alice"))]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "list_chats")
        .expect("list_chats listed");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tool.output_schema.is_some());
    let limit = &tool.input_schema["properties"]["limit"];
    assert_eq!(limit["maximum"], 100, "{limit}");
    assert_eq!(limit["minimum"], 1, "{limit}");
}
