//! `list_chats` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary, MessagePreview},
    fake::{FakeData, fake_issued_id},
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
        muted: false,
        marked_unread: false,
        last_message: Some(MessagePreview {
            date: "2026-11-06T12:00:00Z".into(),
            text: format!("hello {n}"),
            media: None,
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
        (1..=7)
            .map(|n| fake_issued_id(ChatId::User(n)))
            .collect::<Vec<_>>()
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
            // The fake's access hash for user 1 is -7046029254386353131.
            "id": "user:1~njd5uX9KfBU",
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
    let unread_only = &tool.input_schema["properties"]["unread_only"];
    assert!(
        unread_only["type"].to_string().contains("boolean"),
        "{unread_only}"
    );
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.contains("Start here for 'what did I miss'"),
        "{description}"
    );
}

/// Chat `n` with `unread` unread messages, muted or marked as unread.
fn state(n: usize, unread: i32, muted: bool, marked_unread: bool) -> ChatSummary {
    ChatSummary {
        unread_count: unread,
        muted,
        marked_unread,
        ..chat(n)
    }
}

/// `count` read chats, `user:1` to `user:<count>`, except those in `unread`.
fn mostly_read(count: usize, unread: &[usize]) -> Vec<ChatSummary> {
    (1..=count)
        .map(|n| state(n, i32::from(unread.contains(&n)), false, false))
        .collect()
}

fn listed(page: &Value) -> Vec<String> {
    page["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

fn issued(ns: &[i64]) -> Vec<String> {
    ns.iter()
        .map(|&n| fake_issued_id(ChatId::User(n)))
        .collect()
}

#[tokio::test]
async fn unread_only_lists_the_unread_and_the_marked_chats_and_says_which_are_muted() {
    let chats = vec![
        state(1, 0, false, false),
        state(2, 3, false, false),
        state(3, 0, false, true),
        state(4, 120, true, false),
        state(5, 0, true, false),
    ];
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let page = call(&client, json!({ "unread_only": true })).await.unwrap();
    assert_eq!(listed(&page), issued(&[2, 3, 4]));
    assert_eq!(page.get("next_cursor"), None);
    assert_eq!(page.get("unread_scan_truncated"), None);
    let flags = |chat: &Value| {
        (
            chat.get("muted").cloned(),
            chat.get("marked_unread").cloned(),
        )
    };
    assert_eq!(flags(&page["chats"][0]), (None, None));
    assert_eq!(flags(&page["chats"][1]), (None, Some(json!(true))));
    assert_eq!(page["chats"][1]["unread_count"], 0);
    assert_eq!(flags(&page["chats"][2]), (Some(json!(true)), None));

    // Every listing carries the flags; `unread_only: false` lists everything.
    let all = call(&client, json!({ "unread_only": false }))
        .await
        .unwrap();
    assert_eq!(listed(&all), issued(&[1, 2, 3, 4, 5]));
    assert_eq!(flags(&all["chats"][4]), (Some(json!(true)), None));
    assert_eq!(flags(&all["chats"][2]), (None, Some(json!(true))));
}

#[tokio::test]
async fn an_unread_page_is_filled_from_as_many_telegram_pages_as_it_takes() {
    let chats = mostly_read(250, &[5, 150, 240]);
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let page = call(&client, json!({ "unread_only": true, "limit": 3 }))
        .await
        .unwrap();
    assert_eq!(listed(&page), issued(&[5, 150, 240]));
    assert_eq!(page.get("next_cursor"), None, "the end of the list");
    assert_eq!(page.get("unread_scan_truncated"), None);

    // Cut at `limit`, the next page goes on right after its last chat.
    let first = call(&client, json!({ "unread_only": true, "limit": 2 }))
        .await
        .unwrap();
    assert_eq!(listed(&first), issued(&[5, 150]));
    let next = first["next_cursor"].as_str().expect("more to come");
    let rest = call(
        &client,
        json!({ "unread_only": true, "limit": 2, "cursor": next }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&rest), issued(&[240]));
    assert_eq!(rest.get("next_cursor"), None);
}

#[tokio::test]
async fn the_unread_scan_stops_at_its_cap_and_its_cursor_scans_on() {
    // 5 Telegram pages of 100 chats per call: chat 650 is out of reach.
    let chats = mostly_read(700, &[10, 650]);
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let page = call(&client, json!({ "unread_only": true })).await.unwrap();
    assert_eq!(listed(&page), issued(&[10]));
    assert_eq!(page["unread_scan_truncated"], true);
    let next = page["next_cursor"].as_str().expect("the scan goes on");

    let rest = call(&client, json!({ "unread_only": true, "cursor": next }))
        .await
        .unwrap();
    assert_eq!(listed(&rest), issued(&[650]));
    assert_eq!(
        rest.get("unread_scan_truncated"),
        None,
        "it reached the end"
    );
    assert_eq!(rest.get("next_cursor"), None);

    // Nothing unread in the first 500: an empty page that still goes on.
    let chats = mostly_read(700, &[650]);
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    let page = call(&client, json!({ "unread_only": true })).await.unwrap();
    assert_eq!(listed(&page), Vec::<String>::new());
    assert_eq!(page["unread_scan_truncated"], true);
    let next = page["next_cursor"].as_str().unwrap();
    let rest = call(&client, json!({ "unread_only": true, "cursor": next }))
        .await
        .unwrap();
    assert_eq!(listed(&rest), issued(&[650]));
}

#[tokio::test]
async fn unread_only_works_in_the_archive_too() {
    let chats = vec![
        state(1, 2, false, false),
        state(2, 0, false, false),
        state(3, 7, true, false),
        state(4, 0, false, true),
    ];
    let archived = vec![ChatId::User(3), ChatId::User(2)];
    let server = start_with(vec![
        FakeData::new(account(42, "Alice"))
            .with_chats(chats)
            .with_archived(archived),
    ])
    .await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let archive = call(&client, json!({ "unread_only": true, "archived": true }))
        .await
        .unwrap();
    assert_eq!(listed(&archive), issued(&[3]));
    let main = call(&client, json!({ "unread_only": true })).await.unwrap();
    assert_eq!(listed(&main), issued(&[1, 4]));
}

#[tokio::test]
async fn a_cursor_continues_only_the_listing_it_came_from() {
    let chats = mostly_read(10, &[2, 4, 6]);
    let server = start_with(vec![FakeData::new(account(42, "Alice")).with_chats(chats)]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;

    let unread = call(&client, json!({ "unread_only": true, "limit": 1 }))
        .await
        .unwrap();
    let unread = unread["next_cursor"].as_str().unwrap().to_string();
    let plain = call(&client, json!({ "limit": 1 })).await.unwrap();
    let plain = plain["next_cursor"].as_str().unwrap().to_string();

    for bad in [
        json!({ "cursor": unread }),
        json!({ "cursor": unread, "unread_only": false }),
        json!({ "cursor": plain, "unread_only": true }),
    ] {
        match call(&client, bad.clone()).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}: {err:?}");
                assert!(err.message.contains("unread_only"), "{bad}: {err:?}");
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
    // Each with its own listing, they go on.
    let next = call(&client, json!({ "cursor": unread, "unread_only": true }))
        .await
        .unwrap();
    assert_eq!(listed(&next), issued(&[4, 6]));
    let next = call(&client, json!({ "cursor": plain })).await.unwrap();
    assert_eq!(listed(&next)[0], fake_issued_id(ChatId::User(2)));
}
