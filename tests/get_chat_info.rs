//! `get_chat_info` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, FakeDetails},
};

fn summary(id: ChatId, title: &str, kind: ChatKind, username: Option<&str>) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: username.map(str::to_string),
        unread_count: 0,
        pinned: false,
        last_message: None,
    }
}

async fn client() -> (
    common::TestServer,
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    let rust = ChatId::Channel(202);
    let alice = ChatId::User(101);
    let family = ChatId::Chat(404);
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![
            summary(rust, "Rust Weekly", ChatKind::Channel, Some("rustweekly")),
            summary(alice, "Alice", ChatKind::Private, None),
            summary(family, "Family", ChatKind::Group, None),
        ])
        .with_details(
            rust,
            FakeDetails {
                description: Some(format!(
                    "News about Rust.\nEvery week.\n{}",
                    "z".repeat(3000)
                )),
                member_count: Some(5200),
            },
        )
        .with_details(
            alice,
            FakeDetails {
                description: Some("coffee & code".into()),
                member_count: Some(3),
            },
        )
        .with_details(
            family,
            FakeDetails {
                description: Some("   ".into()),
                member_count: Some(5),
            },
        );
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn info(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    chat_id: &str,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("get_chat_info")
                .with_arguments(json!({ "chat_id": chat_id }).as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

#[tokio::test]
async fn describes_channels_people_and_groups() {
    let (_server, client) = client().await;

    let channel = info(&client, "channel:202").await.unwrap();
    assert_eq!(channel["title"], "Rust Weekly");
    assert_eq!(channel["type"], "channel");
    assert_eq!(channel["username"], "rustweekly");
    assert_eq!(channel["member_count"], 5200);
    let description = channel["description"].as_str().unwrap();
    assert!(
        description.starts_with("News about Rust.\nEvery week.\n"),
        "line breaks are kept"
    );
    assert_eq!(description.chars().count(), 1000);

    assert_eq!(
        info(&client, "user:101").await.unwrap(),
        json!({ "id": "user:101", "title": "Alice", "type": "private", "description": "coffee & code" }),
        "people have a bio but no member count"
    );
    assert_eq!(
        info(&client, "chat:404").await.unwrap(),
        json!({ "id": "chat:404", "title": "Family", "type": "group", "member_count": 5 }),
        "a blank description is left out"
    );
    // The account itself is a valid chat ("Saved Messages").
    assert_eq!(info(&client, "user:42").await.unwrap()["title"], "Me");
}

#[tokio::test]
async fn bad_or_unknown_ids_are_invalid_params() {
    let (_server, client) = client().await;
    for bad in ["rustweekly", "channel:-5", "channel:999"] {
        match info(&client, bad).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}")
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
}

#[tokio::test]
async fn tool_is_read_only_and_requires_a_chat() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "get_chat_info")
        .expect("get_chat_info listed");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tool.output_schema.is_some());
    assert_eq!(tool.input_schema["required"], json!(["chat_id"]));
}
