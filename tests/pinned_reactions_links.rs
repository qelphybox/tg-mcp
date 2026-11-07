//! Reactions, message links and pins (#30) through `/mcp` with rmcp's own
//! client: on get_messages messages and Search hits alike, and
//! get_messages `pinned_only`.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, fake_issued_id},
    messages::{RawHistoryMessage, RawReaction, Reaction},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
/// A public channel: `@rust_weekly`.
const NEWS: ChatId = ChatId::Channel(202);
/// A private supergroup: no @username.
const TEAM: ChatId = ChatId::Channel(303);
const FAMILY: ChatId = ChatId::Chat(404);
/// 2026-11-06T00:00:00Z
const DAY: i32 = 1_793_923_200;

fn chat(id: ChatId, title: &str, kind: ChatKind, username: Option<&str>) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: username.map(Into::into),
        is_self: false,
        unread_count: 0,
        pinned: false,
        last_message: None,
    }
}

/// Message `id`, `id` minutes into DAY, with "release" in its text.
fn message(id: i32) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id * 60,
        from: None,
        out: false,
        text: format!("release note {id}"),
        media: None,
        reply_to: None,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
        topic: None,
    }
}

fn pinned(m: RawHistoryMessage) -> RawHistoryMessage {
    RawHistoryMessage { pinned: true, ..m }
}

fn reacted(reaction: Reaction, count: i32) -> RawReaction {
    RawReaction { reaction, count }
}

fn emoji(e: &str, count: i32) -> RawReaction {
    reacted(Reaction::Emoji(e.into()), count)
}

/// News 3 has reactions of every kind and is pinned; 2 has none.
fn news() -> Vec<RawHistoryMessage> {
    vec![
        message(1),
        message(2),
        pinned(RawHistoryMessage {
            reactions: vec![
                emoji("👍", 12),
                reacted(Reaction::Custom, 4),
                emoji("🔥", 3),
                reacted(Reaction::Custom, 2),
                reacted(Reaction::Paid, 100),
            ],
            ..message(3)
        }),
    ]
}

/// Team 1..=10, pinned: 2, 5, 7 and 9.
fn team() -> Vec<RawHistoryMessage> {
    (1..=10)
        .map(|id| match id {
            2 | 5 | 7 | 9 => pinned(message(id)),
            _ => message(id),
        })
        .collect()
}

fn data() -> FakeData {
    FakeData::new(account(42, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice", ChatKind::Private, Some("alice")),
            chat(NEWS, "Rust Weekly", ChatKind::Channel, Some("rust_weekly")),
            chat(TEAM, "Team", ChatKind::Supergroup, None),
            chat(FAMILY, "Family", ChatKind::Group, None),
        ])
        .with_messages(ALICE, vec![pinned(message(1)), message(2)])
        .with_messages(NEWS, news())
        .with_messages(TEAM, team())
        .with_messages(FAMILY, vec![pinned(message(11))])
}

async fn client_with(data: FakeData) -> (common::TestServer, Client) {
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn call(client: &Client, tool: &str, arguments: Value) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

async fn messages(client: &Client, chat: ChatId) -> Vec<Value> {
    let page = call(
        client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(chat) }),
    )
    .await
    .unwrap();
    page["messages"].as_array().unwrap().clone()
}

fn by_id(messages: &[Value], id: i64) -> &Value {
    messages.iter().find(|m| m["id"] == id).unwrap()
}

#[tokio::test]
async fn reactions_are_counted_per_emoji_with_custom_and_paid_summed() {
    let (_server, client) = client_with(data()).await;
    let news = messages(&client, NEWS).await;

    assert_eq!(
        by_id(&news, 3)["reactions"],
        json!({ "👍": 12, "🔥": 3, "custom": 6, "paid": 100 })
    );
    // None: no field at all, not an empty one.
    assert!(by_id(&news, 2).get("reactions").is_none(), "{news:?}");
}

#[tokio::test]
async fn links_open_messages_of_channels_and_supergroups_only() {
    let (_server, client) = client_with(data()).await;

    let news = messages(&client, NEWS).await;
    assert_eq!(by_id(&news, 3)["link"], "https://t.me/rust_weekly/3");
    let team = messages(&client, TEAM).await;
    assert_eq!(by_id(&team, 7)["link"], "https://t.me/c/303/7");
    // The private chat's @username is a person's, not a link to messages.
    for chat in [ALICE, FAMILY] {
        for m in messages(&client, chat).await {
            assert!(m.get("link").is_none(), "{chat}: {m}");
        }
    }
}

#[tokio::test]
async fn pinned_messages_say_so_wherever_they_are() {
    let (_server, client) = client_with(data()).await;
    for (chat, pinned_id, other_id) in [(ALICE, 1, 2), (NEWS, 3, 2), (TEAM, 9, 10)] {
        let page = messages(&client, chat).await;
        assert_eq!(by_id(&page, pinned_id)["pinned"], true, "{chat}");
        assert!(by_id(&page, other_id).get("pinned").is_none(), "{chat}");
    }
    assert_eq!(messages(&client, FAMILY).await[0]["pinned"], true);
}

#[tokio::test]
async fn search_hits_carry_reactions_link_and_pin_too() {
    let (_server, client) = client_with(data()).await;
    let page = call(
        &client,
        "search_messages",
        json!({ "chat_id": fake_issued_id(NEWS), "query": "release" }),
    )
    .await
    .unwrap();
    let hits = page["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    let hit = by_id(hits, 3);
    assert_eq!(
        hit["reactions"],
        json!({ "👍": 12, "🔥": 3, "custom": 6, "paid": 100 })
    );
    assert_eq!(hit["link"], "https://t.me/rust_weekly/3");
    assert_eq!(hit["pinned"], true);
    let plain = by_id(hits, 2);
    for field in ["reactions", "pinned"] {
        assert!(plain.get(field).is_none(), "{plain}");
    }
    assert_eq!(plain["link"], "https://t.me/rust_weekly/2");

    // Across all chats, each hit links as its own chat does.
    let page = call(&client, "search_messages", json!({ "query": "release" }))
        .await
        .unwrap();
    for hit in page["hits"].as_array().unwrap() {
        let link = hit.get("link").and_then(Value::as_str);
        let expected = match hit["chat"]["type"].as_str().unwrap() {
            "channel" => Some(format!("https://t.me/rust_weekly/{}", hit["id"])),
            "supergroup" => Some(format!("https://t.me/c/303/{}", hit["id"])),
            _ => None,
        };
        assert_eq!(link, expected.as_deref(), "{hit}");
    }
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
async fn pinned_only_pages_through_the_pinned_messages_newest_first() {
    let (_server, client) = client_with(data()).await;
    let mut seen = Vec::new();
    let mut before: Option<i64> = None;
    for _ in 0..10 {
        let mut args = json!({ "chat_id": fake_issued_id(TEAM), "pinned_only": true, "limit": 3 });
        if let Some(b) = before {
            args["before_id"] = json!(b);
        }
        let page = call(&client, "get_messages", args).await.unwrap();
        for m in page["messages"].as_array().unwrap() {
            assert_eq!(m["pinned"], true, "{m}");
        }
        seen.extend(ids(&page));
        match page.get("next_before_id").and_then(Value::as_i64) {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, [9, 7, 5, 2]);

    // `before_id` from anywhere: only pinned ones older than it.
    let page = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(TEAM), "pinned_only": true, "before_id": 8 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [7, 5, 2]);
    assert!(page.get("next_before_id").is_none());

    // In a private chat and a basic group as well; `false` is the plain history.
    for (chat, expected) in [(ALICE, vec![1]), (FAMILY, vec![11])] {
        let page = call(
            &client,
            "get_messages",
            json!({ "chat_id": fake_issued_id(chat), "pinned_only": true }),
        )
        .await
        .unwrap();
        assert_eq!(ids(&page), expected, "{chat}");
    }
    let page = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(TEAM), "pinned_only": false, "limit": 3 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [10, 9, 8]);
}

#[tokio::test]
async fn pinned_only_with_anything_but_before_id_and_limit_is_invalid_params() {
    let (_server, client) = client_with(data()).await;
    let team = fake_issued_id(TEAM);
    for (extra, other) in [
        (json!({ "around_id": 5 }), "around_id"),
        (json!({ "after_id": 5 }), "after_id"),
        (json!({ "thread_of": 5 }), "thread_of"),
        (json!({ "since": "2026-11-06" }), "since"),
        (json!({ "until": "2026-11-06" }), "until"),
    ] {
        let mut args = json!({ "chat_id": team, "pinned_only": true });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let err = call(&client, "get_messages", args)
            .await
            .expect_err("conflicting parameters");
        let rmcp::ServiceError::McpError(err) = err else {
            panic!("not a tool error: {err}");
        };
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{other}");
        assert!(
            err.message.contains("pinned_only") && err.message.contains(other),
            "{other}: {}",
            err.message
        );
    }
}

#[tokio::test]
async fn pinned_only_is_a_search_and_waits_out_its_flood_wait() {
    let mut flooded = data();
    flooded.search_flood_wait = Some(120);
    let (_server, client) = client_with(flooded).await;
    let err = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(TEAM), "pinned_only": true }),
    )
    .await
    .expect_err("searches wait");
    let message = err.to_string();
    assert!(
        message.contains("wait 120 s") && message.contains("pinned_only"),
        "{message}"
    );
    // The plain history is another method class: it still reads.
    assert_eq!(messages(&client, TEAM).await.len(), 10);
}

#[tokio::test]
async fn the_schemas_declare_the_new_fields_and_bound_them() {
    let (_server, client) = client_with(data()).await;
    let tools = client.list_all_tools().await.unwrap();
    for name in ["get_messages", "search_messages"] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        assert_eq!(
            tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        let schema = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
        for word in [
            "\"reactions\"",
            "\"maxProperties\":22",
            "\"link\"",
            "\"maxLength\":64",
            "\"pinned\"",
        ] {
            assert!(schema.contains(word), "{name}: {word} in {schema}");
        }
        let description = tool.description.as_deref().unwrap();
        for word in ["reactions", "link", "pinned"] {
            assert!(description.contains(word), "{name}: {word}");
        }
    }
    let tool = tools.iter().find(|t| t.name == "get_messages").unwrap();
    assert_eq!(
        tool.input_schema["properties"]["pinned_only"]["type"],
        json!(["boolean", "null"])
    );
    assert!(tool.description.as_deref().unwrap().contains("pinned_only"));
}
