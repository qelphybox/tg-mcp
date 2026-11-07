//! `get_messages` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, PastNewest, fake_issued_id},
    messages::{MessageAt, RawHistoryMessage},
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
        media: None,
        reply_to: None,
        reply_elsewhere: None,
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

const TEAM: ChatId = ChatId::Channel(303);
const NEWS: ChatId = ChatId::Channel(202);
const BOB: ChatId = ChatId::User(102);

fn chat(id: ChatId, title: &str, kind: ChatKind) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        ..alice_chat()
    }
}

/// `id` replying to `to` in the same chat.
fn reply(id: i32, to: i32) -> RawHistoryMessage {
    RawHistoryMessage {
        reply_to: Some(to),
        ..message(id, &format!("m{id}"))
    }
}

/// A busy supergroup: `10 * k` replies to `10 * (k - 1)` for k in 2..=hops+1,
/// 10 is the root, the ids in between are unrelated chatter.
fn busy_team(hops: i32) -> Vec<RawHistoryMessage> {
    let newest = 10 * (hops + 1);
    (1..=newest + 5)
        .map(|id| match id {
            10 => message(10, "root"),
            id if id % 10 == 0 && id <= newest => reply(id, id - 10),
            id => message(id, "chatter"),
        })
        .collect()
}

/// An account with Alice and Bob (private), the Team supergroup and the News
/// channel, with these histories.
async fn client_with_chats(
    histories: Vec<(ChatId, Vec<RawHistoryMessage>)>,
) -> (common::TestServer, Client) {
    let mut data = FakeData::new(account(42, "Me")).with_chats(vec![
        alice_chat(),
        chat(BOB, "Bob", ChatKind::Private),
        chat(TEAM, "Team", ChatKind::Supergroup),
        chat(NEWS, "News", ChatKind::Channel),
    ]);
    for (chat, messages) in histories {
        data = data.with_messages(chat, messages);
    }
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

fn roots(page: &Value) -> Vec<i64> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m.get("thread_root").is_some())
        .inspect(|m| assert_eq!(m["thread_root"], true, "{m}"))
        .map(|m| m["id"].as_i64().unwrap())
        .collect()
}

/// What a chain page must always be: the message first and flagged, each
/// next message the one its predecessor replies to, no id twice, no cursors.
fn assert_chain(page: &Value, start: i64) {
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(messages[0]["id"], start, "{page}");
    assert_eq!(targets(page), [start], "{page}");
    for pair in messages.windows(2) {
        assert_eq!(pair[0]["reply_to"], pair[1]["id"], "{page}");
    }
    let mut seen = ids(page);
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), messages.len(), "no id twice: {page}");
    assert_eq!(page.get("next_before_id"), None);
    assert_eq!(page.get("next_after_id"), None);
}

#[tokio::test]
async fn thread_of_returns_the_chain_newest_first_with_its_root_flagged() {
    let team = fake_issued_id(TEAM);
    let (_server, client) = client_with_chats(vec![(TEAM, busy_team(4))]).await;
    let page = call(&client, json!({ "chat_id": team, "thread_of": 50 }))
        .await
        .unwrap();
    assert_chain(&page, 50);
    assert_eq!(ids(&page), [50, 40, 30, 20, 10]);
    assert_eq!(roots(&page), [10]);
    assert_eq!(page["messages"][4].get("reply_to"), None);
    assert_eq!(page.get("broken_at"), None);
    assert_eq!(page.get("continues_in"), None);
    assert_eq!(page["chat_id"], team);

    // A message that replies to nothing is its own root.
    let page = call(&client, json!({ "chat_id": team, "thread_of": 7 }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [7]);
    assert_eq!((targets(&page), roots(&page)), (vec![7], vec![7]));
}

#[tokio::test]
async fn a_private_chats_chain_is_never_another_chats_messages() {
    // Outside channels Telegram's message ids are the account's: Bob's 5 is
    // found by id from any private chat, and must not show up as Alice's.
    let (_server, client) = client_with_chats(vec![
        (ALICE, vec![message(3, "q"), reply(9, 3), reply(11, 9)]),
        (BOB, vec![message(5, "b"), reply(8, 5)]),
    ])
    .await;
    let page = call(&client, json!({ "chat_id": alice_id(), "thread_of": 11 }))
        .await
        .unwrap();
    assert_chain(&page, 11);
    assert_eq!(ids(&page), [11, 9, 3]);
    assert_eq!(roots(&page), [3]);
    for bobs in [5, 8] {
        match call(&client, json!({ "chat_id": alice_id(), "thread_of": bobs })).await {
            Err(rmcp::ServiceError::McpError(err)) => assert!(
                err.message
                    .contains(&format!("message {bobs} not found in this chat")),
                "{err:?}"
            ),
            other => panic!("Bob's {bobs} read as Alice's: {other:?}"),
        }
    }
}

#[tokio::test]
async fn depth_caps_the_chain_and_its_oldest_reply_to_goes_on() {
    let team = fake_issued_id(TEAM);
    let (_server, client) = client_with_chats(vec![(TEAM, busy_team(60))]).await;

    // Default: the message and 10 ancestors.
    let page = call(&client, json!({ "chat_id": team, "thread_of": 610 }))
        .await
        .unwrap();
    assert_chain(&page, 610);
    assert_eq!(ids(&page).len(), 11);
    assert!(roots(&page).is_empty());
    assert_eq!(page["messages"][10]["reply_to"], 500);

    // The maximum, then the rest from where it stopped: the whole chain once.
    let page = call(
        &client,
        json!({ "chat_id": team, "thread_of": 610, "depth": 50 }),
    )
    .await
    .unwrap();
    assert_chain(&page, 610);
    assert_eq!(ids(&page).len(), 51);
    let mut chain = ids(&page);
    let next = page["messages"][50]["reply_to"].as_i64().unwrap();
    let rest = call(
        &client,
        json!({ "chat_id": team, "thread_of": next, "depth": 50 }),
    )
    .await
    .unwrap();
    assert_chain(&rest, next);
    assert_eq!(roots(&rest), [10]);
    chain.extend(ids(&rest));
    assert_eq!(chain, (1..=61).rev().map(|k| 10 * k).collect::<Vec<i64>>());

    let page = call(
        &client,
        json!({ "chat_id": team, "thread_of": 30, "depth": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [30, 20]);
    assert!(roots(&page).is_empty(), "20 replies to 10: not a root");
}

#[tokio::test]
async fn a_deleted_parent_is_named_and_the_chain_up_to_it_returned() {
    let team = fake_issued_id(TEAM);
    let mut history = busy_team(6);
    history.retain(|m| m.id != 30);
    let (_server, client) = client_with_chats(vec![(TEAM, history)]).await;
    let page = call(&client, json!({ "chat_id": team, "thread_of": 60 }))
        .await
        .unwrap();
    assert_chain(&page, 60);
    assert_eq!(ids(&page), [60, 50, 40]);
    assert_eq!(page["broken_at"], 30);
    assert_eq!(page["messages"][2]["reply_to"], 30, "the broken link shows");
    assert!(roots(&page).is_empty());

    // The message itself deleted: an error, not an empty chain.
    match call(&client, json!({ "chat_id": team, "thread_of": 30 })).await {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert!(err.message.contains("message 30 not found"), "{err:?}")
        }
        other => panic!("a deleted message answered {other:?}"),
    }
}

#[tokio::test]
async fn a_parent_in_another_chat_is_where_the_chain_continues() {
    let team = fake_issued_id(TEAM);
    let mut history = busy_team(4);
    // 20 replies to post 7 of the News channel (reply_to_peer_id).
    let at = history.iter().position(|m| m.id == 20).unwrap();
    history[at] = RawHistoryMessage {
        reply_elsewhere: Some(MessageAt { chat: NEWS, id: 7 }),
        ..message(20, "comment")
    };
    let news: Vec<RawHistoryMessage> = (1..=9).map(|id| message(id, "post")).collect();
    let (_server, client) = client_with_chats(vec![(TEAM, history), (NEWS, news)]).await;
    let page = call(&client, json!({ "chat_id": team, "thread_of": 40 }))
        .await
        .unwrap();
    assert_chain(&page, 40);
    assert_eq!(ids(&page), [40, 30, 20]);
    assert_eq!(page["messages"][2].get("reply_to"), None);
    assert_eq!(
        page["continues_in"],
        json!({ "chat_id": fake_issued_id(NEWS), "message_id": 7 })
    );
    assert_eq!(page.get("broken_at"), None);
    assert!(roots(&page).is_empty());

    // The link opens as it is.
    let link = &page["continues_in"];
    let there = call(
        &client,
        json!({ "chat_id": link["chat_id"], "thread_of": link["message_id"] }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&there), [7]);
    assert_eq!(roots(&there), [7]);
}

#[tokio::test]
async fn the_budget_cuts_a_chain_at_its_old_end_never_the_message() {
    let team = fake_issued_id(TEAM);
    let long = "y".repeat(5000);
    let mut history: Vec<RawHistoryMessage> = busy_team(60)
        .into_iter()
        .map(|m| RawHistoryMessage {
            text: long.clone(),
            ..m
        })
        .collect();
    // 180's parent is deleted: 44 messages up to the break, more than fit.
    history.retain(|m| m.id != 170);
    let (_server, client) = client_with_chats(vec![(TEAM, history)]).await;
    let page = call(
        &client,
        json!({ "chat_id": team, "thread_of": 610, "depth": 50 }),
    )
    .await
    .unwrap();
    assert_chain(&page, 610);
    let kept = ids(&page);
    assert!(kept.len() < 44, "44 x 1000 characters must be cut");
    assert_eq!(page.get("broken_at"), None, "not shown: {kept:?}");
    let oldest = page["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(
        oldest["reply_to"],
        kept.last().unwrap() - 10,
        "goes on there"
    );
}

#[tokio::test]
async fn thread_of_takes_no_other_position_and_depth_only_with_it() {
    let team = fake_issued_id(TEAM);
    let (_server, client) = client_with_chats(vec![(TEAM, busy_team(4))]).await;
    for (bad, says) in [
        (
            json!({ "thread_of": 50, "before_id": 11 }),
            "can't be combined",
        ),
        (
            json!({ "thread_of": 50, "around_id": 40 }),
            "can't be combined",
        ),
        (
            json!({ "thread_of": 50, "after_id": 40 }),
            "can't be combined",
        ),
        (
            json!({ "thread_of": 50, "since": "2026-11-06" }),
            "can't be combined",
        ),
        (
            json!({ "thread_of": 50, "until": "2026-11-06" }),
            "can't be combined",
        ),
        (
            json!({ "around_id": 50, "thread_of": 40 }),
            "can't be combined",
        ),
        (json!({ "thread_of": 50, "limit": 5 }), "depth"),
        (json!({ "depth": 5 }), "only applies to thread_of"),
        (
            json!({ "around_id": 50, "depth": 5 }),
            "only applies to thread_of",
        ),
        (json!({ "thread_of": 50, "depth": 51 }), "between 1 and 50"),
        (json!({ "thread_of": 50, "depth": 0 }), "between 1 and 50"),
        (json!({ "thread_of": 0 }), "positive"),
    ] {
        let mut args = bad.clone();
        args["chat_id"] = json!(team);
        match call(&client, args).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}: {err:?}");
                assert!(err.message.contains(says), "{bad}: {err:?}");
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_schema_bounds_thread_of_and_depth() {
    let (_server, client) = client_with(vec![]).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_messages").unwrap();
    let properties = &tool.input_schema["properties"];
    assert_eq!(properties["thread_of"]["minimum"], 1);
    assert_eq!(properties["depth"]["minimum"], 1);
    assert_eq!(properties["depth"]["maximum"], 50);
    let description = tool.description.as_deref().unwrap();
    for word in ["thread_of", "thread_root", "broken_at", "continues_in"] {
        assert!(description.contains(word), "{word}: {description}");
    }
}

/// Team, where Bob (a member the account also has a private chat with) wrote
/// the newest message, 10, and Alice the rest. Telegram describes users and
/// channels as "min" (no hash to hand out) in a getHistory answer whose slice
/// reaches past the newest message, as it did in production (#51).
async fn team_client(past_newest: PastNewest) -> (common::TestServer, Client) {
    let history = (1..=10)
        .map(|id| RawHistoryMessage {
            from: Some(if id == 10 { BOB } else { ALICE }),
            out: false,
            ..message(id, "x")
        })
        .collect();
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![
            alice_chat(),
            chat(BOB, "Bob", ChatKind::Private),
            chat(TEAM, "Team", ChatKind::Supergroup),
        ])
        .with_messages(TEAM, history)
        .with_past_newest(past_newest)
        .with_min_past_newest();
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

/// The window of 3 around Team's newest message.
fn newest_window() -> Value {
    json!({ "chat_id": fake_issued_id(TEAM), "around_id": 10, "limit": 3 })
}

fn sender_of(page: &Value, id: i64) -> &str {
    let message = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    message["sender"]["id"].as_str().unwrap()
}

#[tokio::test]
async fn a_window_at_the_newest_message_keeps_the_hashes_seen_before() {
    for past_newest in BOTH_EDGES {
        let (_server, client) = team_client(past_newest).await;
        // Nothing has described Bob in full yet: his id has no hash, and none
        // is invented.
        let page = call(&client, newest_window()).await.unwrap();
        assert_eq!(ids(&page), [10, 9, 8], "{past_newest:?}");
        assert_eq!(sender_of(&page, 10), BOB.to_string(), "{past_newest:?}");
        if past_newest == PastNewest::Shorten {
            // Alice wrote 9, in the min answer, and 8, in the second call that
            // fills the shortened window: the merged answers give her hash.
            assert_eq!(sender_of(&page, 9), fake_issued_id(ALICE));
        }

        // A plain page describes Bob in full; from then on the window
        // hands out his id with its hash.
        call(
            &client,
            json!({ "chat_id": fake_issued_id(TEAM), "limit": 5 }),
        )
        .await
        .unwrap();
        let page = call(&client, newest_window()).await.unwrap();
        assert_eq!(sender_of(&page, 10), fake_issued_id(BOB), "{past_newest:?}");
        assert_eq!(
            sender_of(&page, 9),
            fake_issued_id(ALICE),
            "{past_newest:?}"
        );
        assert_eq!(
            sender_of(&page, 8),
            fake_issued_id(ALICE),
            "{past_newest:?}"
        );
    }
}

#[tokio::test]
async fn every_answer_teaches_the_hashes_later_ids_carry() {
    let team = fake_issued_id(TEAM);
    for (tool, arguments) in [
        ("list_chats", json!({})),
        ("get_chat_info", json!({ "chat_id": fake_issued_id(BOB) })),
        ("find_chats", json!({ "query": "bob" })),
        ("search_messages", json!({ "query": "x" })),
        ("get_messages", json!({ "chat_id": team, "thread_of": 9 })),
        ("get_messages", json!({ "chat_id": team, "limit": 5 })),
    ] {
        let (_server, client) = team_client(PastNewest::Shorten).await;
        client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        let page = call(&client, newest_window()).await.unwrap();
        assert_eq!(
            sender_of(&page, 10),
            fake_issued_id(BOB),
            "{tool} {arguments}"
        );
    }
}
