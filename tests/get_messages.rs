//! `get_messages` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, PastNewest, fake_issued_id},
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

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

async fn client_with(messages: Vec<RawHistoryMessage>) -> (common::TestServer, Client) {
    client_on(messages, PastNewest::default()).await
}

/// A client whose fake Telegram answers a getHistory slice that reaches past
/// the newest message the way `past_newest` says.
async fn client_on(
    messages: Vec<RawHistoryMessage>,
    past_newest: PastNewest,
) -> (common::TestServer, Client) {
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![alice_chat()])
        .with_messages(ALICE, messages)
        .with_past_newest(past_newest);
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
    for position in ["before_id", "around_id", "after_id"] {
        assert_eq!(
            tool.input_schema["properties"][position]["minimum"], 1,
            "{position}"
        );
    }
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.contains("around_id") && description.contains("next_after_id"),
        "{description}"
    );
}

/// Ids 1..=20 with 7 and 12 deleted: the gaps a real history has.
fn gapped_history(text: &str) -> Vec<RawHistoryMessage> {
    (1..=20)
        .filter(|id| ![7, 12].contains(id))
        .map(|i| message(i, text))
        .collect()
}

const BOTH_EDGES: [PastNewest; 2] = [PastNewest::Shorten, PastNewest::ShiftOlder];

fn targets(page: &Value) -> Vec<i64> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m.get("target").is_some())
        .inspect(|m| assert_eq!(m["target"], true, "{m}"))
        .map(|m| m["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn a_window_around_a_message_is_centred_on_it_and_flags_it() {
    let (_server, client) = client_with(gapped_history("x")).await;

    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "around_id": 10, "limit": 5 }),
    )
    .await
    .unwrap();
    // Two newer, the message, two older; deleted 12 is simply not there.
    assert_eq!(ids(&page), [13, 11, 10, 9, 8]);
    assert_eq!(targets(&page), [10]);
    assert_eq!(page["next_before_id"], 8);
    assert_eq!(page["next_after_id"], 13);

    // An even window gives the older side the extra message.
    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "around_id": 10, "limit": 4 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [11, 10, 9, 8]);

    // A window of one is the message itself, with both ways on.
    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "around_id": 10, "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [10]);
    assert_eq!(targets(&page), [10]);
    assert_eq!(page["next_before_id"], 10);
    assert_eq!(page["next_after_id"], 10);
}

#[tokio::test]
async fn a_window_at_the_newest_message_is_filled_from_older_ones() {
    for past_newest in BOTH_EDGES {
        let (_server, client) = client_on(gapped_history("x"), past_newest).await;
        for (around, expected) in [(19, [20, 19, 18, 17, 16]), (20, [20, 19, 18, 17, 16])] {
            let page = call(
                &client,
                json!({ "chat_id": alice_id(), "around_id": around, "limit": 5 }),
            )
            .await
            .unwrap();
            assert_eq!(ids(&page), expected, "{past_newest:?} around {around}");
            assert_eq!(targets(&page), [around]);
            assert_eq!(page.get("next_after_id"), None, "nothing is newer");
            assert_eq!(page["next_before_id"], 16);
        }
        // Exactly enough newer ones: like a full page, the cursor stays (and
        // leads to an empty page).
        let page = call(
            &client,
            json!({ "chat_id": alice_id(), "around_id": 18, "limit": 5 }),
        )
        .await
        .unwrap();
        assert_eq!(ids(&page), [20, 19, 18, 17, 16]);
        assert_eq!(page["next_after_id"], 20);
    }
}

#[tokio::test]
async fn a_window_at_the_oldest_message_has_nothing_older() {
    let (_server, client) = client_with(gapped_history("x")).await;
    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "around_id": 2, "limit": 5 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [4, 3, 2, 1]);
    assert_eq!(page.get("next_before_id"), None);
    assert_eq!(page["next_after_id"], 4);
}

/// Everything reachable from a window by following both cursors to the end,
/// oldest first, each page newest first as the tool returns it.
async fn walk_from_window(client: &Client, around: i64, limit: u32) -> Vec<i64> {
    let window = call(
        client,
        json!({ "chat_id": alice_id(), "around_id": around, "limit": limit }),
    )
    .await
    .unwrap();
    assert_eq!(targets(&window), [around]);
    let mut seen = ids(&window);

    let mut before = window.get("next_before_id").and_then(Value::as_i64);
    for _ in 0..50 {
        let Some(b) = before else { break };
        let page = call(
            client,
            json!({ "chat_id": alice_id(), "before_id": b, "limit": limit }),
        )
        .await
        .unwrap();
        seen.extend(ids(&page));
        before = page.get("next_before_id").and_then(Value::as_i64);
    }
    let mut after = window.get("next_after_id").and_then(Value::as_i64);
    for _ in 0..50 {
        let Some(a) = after else { break };
        let page = call(
            client,
            json!({ "chat_id": alice_id(), "after_id": a, "limit": limit }),
        )
        .await
        .unwrap();
        assert_eq!(page.get("next_before_id"), None, "older is the window");
        assert!(targets(&page).is_empty());
        let mut newer = ids(&page);
        assert!(newer.iter().all(|&id| id > a), "{newer:?} after {a}");
        newer.extend(seen);
        seen = newer;
        after = page.get("next_after_id").and_then(Value::as_i64);
    }
    seen.reverse();
    seen
}

#[tokio::test]
async fn both_cursors_continue_the_window_without_gaps_or_overlap() {
    let history = gapped_history("x");
    let all: Vec<i64> = history.iter().map(|m| i64::from(m.id)).collect();
    for past_newest in BOTH_EDGES {
        let (_server, client) = client_on(history.clone(), past_newest).await;
        for around in [1, 2, 9, 10, 13, 19, 20] {
            for limit in [1, 2, 3, 5] {
                assert_eq!(
                    walk_from_window(&client, around, limit).await,
                    all,
                    "{past_newest:?} around {around} limit {limit}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_missing_message_is_a_clear_error_not_an_empty_window() {
    let (_server, client) = client_with(gapped_history("x")).await;
    for missing in [7, 12, 21, 5000] {
        match call(
            &client,
            json!({ "chat_id": alice_id(), "around_id": missing }),
        )
        .await
        {
            Err(rmcp::ServiceError::McpError(err)) => assert!(
                err.message
                    .contains(&format!("message {missing} not found")),
                "{err:?}"
            ),
            other => panic!("around_id {missing} answered {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_budget_cuts_a_window_at_its_ends_never_the_target() {
    let long = "y".repeat(5000);
    let (_server, client) = client_with((1..=200).map(|i| message(i, &long)).collect()).await;

    let page = call(
        &client,
        json!({ "chat_id": alice_id(), "around_id": 100, "limit": 100 }),
    )
    .await
    .unwrap();
    let kept = ids(&page);
    assert!(kept.len() < 100, "a 100 x 1000-char window must be cut");
    assert_eq!(targets(&page), [100]);
    let at = kept.iter().position(|&id| id == 100).unwrap();
    let (newer, older) = (at, kept.len() - at - 1);
    assert!(newer.abs_diff(older) <= 1, "still centred: {kept:?}");
    assert!(
        kept.windows(2).all(|w| w[0] == w[1] + 1),
        "contiguous: {kept:?}"
    );
    assert_eq!(page["next_after_id"], kept[0]);
    assert_eq!(page["next_before_id"], *kept.last().unwrap());

    // after_id pages are cut at their newest end, so the next one follows on.
    let after = call(
        &client,
        json!({ "chat_id": alice_id(), "after_id": kept[0], "limit": 100 }),
    )
    .await
    .unwrap();
    let newer = ids(&after);
    assert!(newer.len() < 100);
    assert_eq!(*newer.last().unwrap(), kept[0] + 1, "starts right after");
    assert_eq!(after["next_after_id"], newer[0]);
}

#[tokio::test]
async fn around_and_after_take_no_other_position() {
    let (_server, client) = client_with(gapped_history("x")).await;
    for bad in [
        json!({ "chat_id": alice_id(), "around_id": 10, "before_id": 11 }),
        json!({ "chat_id": alice_id(), "around_id": 10, "after_id": 9 }),
        json!({ "chat_id": alice_id(), "around_id": 10, "since": "2026-11-06" }),
        json!({ "chat_id": alice_id(), "around_id": 10, "until": "2026-11-06" }),
        json!({ "chat_id": alice_id(), "after_id": 9, "before_id": 11 }),
        json!({ "chat_id": alice_id(), "after_id": 9, "since": "2026-11-06" }),
        json!({ "chat_id": alice_id(), "after_id": 9, "until": "2026-11-06" }),
    ] {
        match call(&client, bad.clone()).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}: {err:?}");
                assert!(err.message.contains("can't be combined with"), "{err:?}");
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
    for bad in [
        json!({ "chat_id": alice_id(), "around_id": 0 }),
        json!({ "chat_id": alice_id(), "after_id": 0 }),
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
async fn plain_pages_have_no_newer_cursor_and_no_target() {
    let (_server, client) = client_with(gapped_history("x")).await;
    let page = call(&client, json!({ "chat_id": alice_id(), "limit": 3 }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [20, 19, 18]);
    assert_eq!(page.get("next_after_id"), None);
    assert!(targets(&page).is_empty());
}
