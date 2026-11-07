//! `search_messages` through `/mcp` with rmcp's own client, on the fake's
//! model of Telegram's search paging (src/tg/fake.rs, `search_answer`).

mod common;

use std::collections::HashSet;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, fake_issued_id},
    messages::{Media, MediaKind, RawHistoryMessage},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
const BOB: ChatId = ChatId::User(102);
const NEWS: ChatId = ChatId::Channel(202);
const TEAM: ChatId = ChatId::Channel(303);
const FAMILY: ChatId = ChatId::Chat(404);
/// Has messages, but Telegram doesn't describe it (not in `chats`).
const GHOST: ChatId = ChatId::Channel(505);
/// 2026-11-06T00:00:00Z
const DAY: i32 = 1_793_923_200;

fn chat(id: ChatId, title: &str, kind: ChatKind) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        last_message: None,
    }
}

/// Message `id`, `minute` minutes into DAY.
fn message(id: i32, minute: i32, text: &str) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + minute * 60,
        from: None,
        out: false,
        text: text.into(),
        media: None,
        reply_to: None,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
    }
}

fn from(sender: ChatId, m: RawHistoryMessage) -> RawHistoryMessage {
    RawHistoryMessage {
        from: Some(sender),
        ..m
    }
}

fn mine(m: RawHistoryMessage) -> RawHistoryMessage {
    RawHistoryMessage { out: true, ..m }
}

/// Media with what Telegram tells about it besides the kind, which hits leave
/// to get_messages.
fn with_media(kind: MediaKind, m: RawHistoryMessage) -> RawHistoryMessage {
    RawHistoryMessage {
        media: Some(Media {
            size_bytes: Some(70_000),
            width: Some(800),
            height: Some(600),
            ..Media::from(kind)
        }),
        ..m
    }
}

/// Ids clash across chats on purpose: message ids are per chat.
fn data() -> FakeData {
    FakeData::new(account(ME, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice", ChatKind::Private),
            chat(BOB, "Bob", ChatKind::Private),
            chat(NEWS, "News", ChatKind::Channel),
            chat(TEAM, "Team", ChatKind::Supergroup),
            chat(FAMILY, "Family", ChatKind::Group),
        ])
        .with_archived(vec![FAMILY])
        .with_messages(
            ALICE,
            vec![
                message(1, 1, "Deploy is planned for Friday"),
                mine(message(2, 5, "ok, deploying then")),
                message(3, 9, "lunch?"),
                with_media(MediaKind::Photo, message(4, 30, "")),
                with_media(MediaKind::Photo, message(5, 31, "the deploy dashboard")),
            ],
        )
        .with_messages(BOB, vec![message(1, 2, "did the deploy go out?")])
        .with_messages(
            NEWS,
            vec![
                message(1, 3, "Release notes: https://example.com/deploy"),
                with_media(MediaKind::Video, message(2, 40, "deploy walkthrough")),
            ],
        )
        .with_messages(
            TEAM,
            vec![
                from(ALICE, message(1, 4, "deploy is green")),
                mine(message(2, 6, "deploy done")),
                from(BOB, message(3, 7, "redeploy please")),
                from(BOB, message(4, 8, "Deploy again tomorrow")),
                with_media(MediaKind::Voice, from(ALICE, message(5, 50, ""))),
                from(ChatId::Channel(999), message(6, 51, "deploy as a channel")),
            ],
        )
        .with_messages(FAMILY, vec![from(BOB, message(1, 10, "deploy the kids"))])
        .with_messages(GHOST, vec![message(1, 11, "deploy from nowhere")])
}

async fn client_for(data: FakeData) -> (common::TestServer, Client) {
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn client() -> (common::TestServer, Client) {
    client_for(data()).await
}

async fn call_tool(
    client: &Client,
    tool: &'static str,
    arguments: Value,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

async fn search(client: &Client, arguments: Value) -> Result<Value, rmcp::ServiceError> {
    call_tool(client, "search_messages", arguments).await
}

fn hits(page: &Value) -> &Vec<Value> {
    page["hits"].as_array().unwrap()
}

/// (chat id, message id) of each hit.
fn keys(page: &Value) -> Vec<(String, i64)> {
    hits(page)
        .iter()
        .map(|h| {
            (
                h["chat"]["id"].as_str().unwrap().to_string(),
                h["id"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn hit(chat: ChatId, id: i64) -> (String, i64) {
    (fake_issued_id(chat), id)
}

/// Every page of a search, following `next_cursor` to the end.
async fn page_through(client: &Client, arguments: Value) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut arguments = arguments;
    loop {
        let page = search(client, arguments.clone()).await.unwrap();
        let next = page
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(String::from);
        pages.push(page);
        match next {
            Some(next) => arguments["cursor"] = json!(next),
            None => return pages,
        }
        assert!(pages.len() < 100, "paging never ends");
    }
}

fn invalid(result: Result<Value, rmcp::ServiceError>, case: &Value) -> String {
    match result {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{case}: {err:?}");
            err.message.to_string()
        }
        other => panic!("{case} was accepted: {other:?}"),
    }
}

#[tokio::test]
async fn a_global_search_finds_every_chat_newest_first() {
    let (_server, client) = client().await;
    let page = search(&client, json!({ "query": "deploy" })).await.unwrap();
    // Word beginnings only ("redeploy" is not one), any case, every folder.
    assert_eq!(
        keys(&page),
        [
            hit(TEAM, 6),
            hit(NEWS, 2),
            hit(ALICE, 5),
            (GHOST.to_string(), 1),
            hit(FAMILY, 1),
            hit(TEAM, 4),
            hit(TEAM, 2),
            hit(ALICE, 2),
            hit(TEAM, 1),
            hit(NEWS, 1),
            hit(BOB, 1),
            hit(ALICE, 1),
        ]
    );
    assert_eq!(page.get("next_cursor"), None, "one page holds them all");
    assert_eq!(page.get("count"), None);

    let team = &hits(&page)[5];
    assert_eq!(
        team,
        &json!({
            "chat": { "id": fake_issued_id(TEAM), "title": "Team", "type": "supergroup" },
            "id": 4,
            "date": "2026-11-06T00:08:00Z",
            "sender": { "id": fake_issued_id(BOB), "name": "Bob" },
            "snippet": "Deploy again tomorrow",
            "link": "https://t.me/c/303/4",
        })
    );
    // Our own message: no sender, outgoing.
    assert_eq!(hits(&page)[6]["outgoing"], true);
    assert_eq!(hits(&page)[6].get("sender"), None);
    // A photo with a caption: the caption, and only the kind of media.
    assert_eq!(hits(&page)[2]["media"], json!({ "kind": "photo" }));
    assert_eq!(hits(&page)[2]["snippet"], "the deploy dashboard");
    // A chat Telegram didn't describe: no hash is invented, the id still works.
    assert_eq!(hits(&page)[3]["chat"]["title"], "Unknown chat");
    // A sender Telegram didn't describe.
    assert_eq!(hits(&page)[0]["sender"]["id"], "channel:999");
}

#[tokio::test]
async fn a_hit_is_read_in_context_with_get_messages_around_it() {
    let (_server, client) = client().await;
    let page = search(&client, json!({ "query": "lunch" })).await.unwrap();
    let found = &hits(&page)[0];
    let window = call_tool(
        &client,
        "get_messages",
        json!({ "chat_id": found["chat"]["id"], "around_id": found["id"], "limit": 3 }),
    )
    .await
    .unwrap();
    let ids: Vec<i64> = window["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [4, 3, 2]);
    assert_eq!(window["messages"][1]["target"], true);
}

#[tokio::test]
async fn every_id_a_hit_hands_out_is_accepted_by_the_other_tools() {
    let (_server, client) = client().await;
    let page = search(&client, json!({ "query": "deploy" })).await.unwrap();
    for found in hits(&page) {
        let chat_id = found["chat"]["id"].as_str().unwrap();
        if chat_id != GHOST.to_string() {
            assert!(
                chat_id.starts_with("chat:") || chat_id.contains('~'),
                "{chat_id}"
            );
        }
        call_tool(
            &client,
            "get_messages",
            json!({ "chat_id": chat_id, "around_id": found["id"], "limit": 1 }),
        )
        .await
        .unwrap();
        // Senders Telegram described carry their hash; others none, not a made-up one.
        if let Some(sender) = found.get("sender").and_then(|s| s["id"].as_str()) {
            let id: ChatId = sender.split('~').next().unwrap().parse().unwrap();
            if [ALICE, BOB, NEWS, TEAM].contains(&id) {
                assert_eq!(sender, fake_issued_id(id));
            } else {
                assert_eq!(sender, id.to_string());
            }
        }
    }
}

#[tokio::test]
async fn paging_to_the_last_page_yields_every_hit_once_in_order() {
    let (_server, client) = client().await;
    let everything: Vec<(String, i64)> = keys(
        &search(&client, json!({ "query": "deploy", "limit": 50 }))
            .await
            .unwrap(),
    );
    assert_eq!(everything.len(), 12);
    for limit in 1..=5 {
        let pages = page_through(&client, json!({ "query": "deploy", "limit": limit })).await;
        for page in &pages[..pages.len() - 1] {
            assert_eq!(hits(page).len(), limit, "limit {limit}");
        }
        assert!(hits(pages.last().unwrap()).len() <= limit);
        let seen: Vec<(String, i64)> = pages.iter().flat_map(keys).collect();
        assert_eq!(seen, everything, "limit {limit}");
    }
    // Within one chat as well.
    let pages = page_through(
        &client,
        json!({ "query": "deploy", "chat_id": fake_issued_id(TEAM), "limit": 1 }),
    )
    .await;
    let seen: Vec<(String, i64)> = pages.iter().flat_map(keys).collect();
    assert_eq!(
        seen,
        [hit(TEAM, 6), hit(TEAM, 4), hit(TEAM, 2), hit(TEAM, 1)]
    );
}

#[tokio::test]
async fn a_cursor_reused_with_another_search_is_rejected() {
    let (_server, client) = client().await;
    let first = search(&client, json!({ "query": "deploy", "limit": 2 }))
        .await
        .unwrap();
    let cursor = first["next_cursor"].as_str().unwrap();
    for changed in [
        json!({ "query": "deploying", "cursor": cursor }),
        json!({ "query": "deploy", "media": "photo", "cursor": cursor }),
        json!({ "query": "deploy", "since": "2026-11-06", "cursor": cursor }),
        json!({ "query": "deploy", "chat_type": "private", "cursor": cursor }),
        json!({ "query": "deploy", "archived": false, "cursor": cursor }),
        json!({ "query": "deploy", "chat_id": fake_issued_id(TEAM), "cursor": cursor }),
    ] {
        let message = invalid(search(&client, changed.clone()).await, &changed);
        assert!(message.contains("different search"), "{changed}: {message}");
    }
    let message = invalid(
        search(&client, json!({ "query": "deploy", "cursor": "garbage" })).await,
        &json!("garbage"),
    );
    assert!(message.contains("cursor"), "{message}");
    // The same search continues, at another page size too.
    let next = search(
        &client,
        json!({ "query": "deploy", "limit": 3, "cursor": cursor }),
    )
    .await
    .unwrap();
    assert_eq!(
        keys(&next),
        [hit(ALICE, 5), (GHOST.to_string(), 1), hit(FAMILY, 1)]
    );
}

#[tokio::test]
async fn a_search_in_one_chat_filters_by_sender() {
    let (_server, client) = client().await;
    let team = fake_issued_id(TEAM);
    let ids = |page: &Value| -> Vec<i64> {
        hits(page)
            .iter()
            .map(|h| h["id"].as_i64().unwrap())
            .collect()
    };

    let page = search(&client, json!({ "chat_id": team, "query": "deploy" }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [6, 4, 2, 1]);
    assert!(hits(&page).iter().all(|h| h["chat"]["id"] == team));

    let page = search(
        &client,
        json!({ "chat_id": team, "query": "deploy", "from": fake_issued_id(BOB) }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [4]);
    // `me`, and an empty query is fine with a sender: all their messages.
    let page = search(&client, json!({ "chat_id": team, "from": "me" }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [2]);
    let page = search(
        &client,
        json!({ "chat_id": team, "from": fake_issued_id(ALICE) }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [5, 1]);
    // Posts sent as a channel.
    let page = search(&client, json!({ "chat_id": team, "from": "channel:999" }))
        .await
        .unwrap();
    assert_eq!(ids(&page), [6]);
    // In a private chat the other side is the sender even without from_id.
    let page = search(
        &client,
        json!({ "chat_id": fake_issued_id(ALICE), "from": fake_issued_id(ALICE) }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), [5, 4, 3, 1]);
}

#[tokio::test]
async fn media_and_dates_narrow_both_kinds_of_search() {
    let (_server, client) = client().await;
    // Media without text has an empty snippet; an empty query goes with media.
    let page = search(&client, json!({ "media": "photo" })).await.unwrap();
    assert_eq!(keys(&page), [hit(ALICE, 5), hit(ALICE, 4)]);
    assert_eq!(hits(&page)[1]["snippet"], "");
    assert_eq!(hits(&page)[1]["media"], json!({ "kind": "photo" }));
    let page = search(
        &client,
        json!({ "media": "voice", "chat_id": fake_issued_id(TEAM) }),
    )
    .await
    .unwrap();
    assert_eq!(keys(&page), [hit(TEAM, 5)]);
    let page = search(&client, json!({ "query": "deploy", "media": "video" }))
        .await
        .unwrap();
    assert_eq!(keys(&page), [hit(NEWS, 2)]);
    let page = search(&client, json!({ "query": "deploy", "media": "link" }))
        .await
        .unwrap();
    assert_eq!(keys(&page), [hit(NEWS, 1)]);

    // Inclusive bounds, as in get_messages: minute 4 through minute 8.
    let page = search(
        &client,
        json!({ "query": "deploy", "since": "2026-11-06T00:04:00Z", "until": "2026-11-06T00:08:00Z" }),
    )
    .await
    .unwrap();
    assert_eq!(
        keys(&page),
        [hit(TEAM, 4), hit(TEAM, 2), hit(ALICE, 2), hit(TEAM, 1)]
    );
    let page = search(
        &client,
        json!({ "query": "deploy", "chat_id": fake_issued_id(TEAM), "until": "2026-11-06T00:06:00Z" }),
    )
    .await
    .unwrap();
    assert_eq!(keys(&page), [hit(TEAM, 2), hit(TEAM, 1)]);
    let page = search(&client, json!({ "query": "deploy", "since": "2026-11-07" }))
        .await
        .unwrap();
    assert!(hits(&page).is_empty());
}

#[tokio::test]
async fn chat_type_and_archived_choose_the_chats_of_a_global_search() {
    let (_server, client) = client().await;
    let chats = |page: &Value| -> HashSet<String> {
        hits(page)
            .iter()
            .map(|h| h["chat"]["id"].as_str().unwrap().to_string())
            .collect()
    };
    let issued =
        |ids: &[ChatId]| -> HashSet<String> { ids.iter().map(|&c| fake_issued_id(c)).collect() };

    let page = search(
        &client,
        json!({ "query": "deploy", "chat_type": "private" }),
    )
    .await
    .unwrap();
    assert_eq!(chats(&page), issued(&[ALICE, BOB]));
    let page = search(&client, json!({ "query": "deploy", "chat_type": "groups" }))
        .await
        .unwrap();
    assert_eq!(chats(&page), issued(&[TEAM, FAMILY]));
    let page = search(
        &client,
        json!({ "query": "deploy", "chat_type": "channels" }),
    )
    .await
    .unwrap();
    let mut channels = issued(&[NEWS]);
    channels.insert(GHOST.to_string());
    assert_eq!(chats(&page), channels);

    let page = search(&client, json!({ "query": "deploy", "archived": true }))
        .await
        .unwrap();
    assert_eq!(chats(&page), issued(&[FAMILY]));
    let page = search(&client, json!({ "query": "deploy", "archived": false }))
        .await
        .unwrap();
    assert!(!chats(&page).contains(&fake_issued_id(FAMILY)));
    assert_eq!(hits(&page).len(), 11);
    // Absent: everywhere, the Archive included.
    let page = search(&client, json!({ "query": "deploy" })).await.unwrap();
    assert!(chats(&page).contains(&fake_issued_id(FAMILY)));
}

#[tokio::test]
async fn conflicting_or_unbounded_parameters_are_invalid_params() {
    let (_server, client) = client().await;
    let team = fake_issued_id(TEAM);
    for (bad, says) in [
        (
            json!({ "query": "deploy", "from": "me" }),
            "from only works within one chat",
        ),
        (
            json!({ "query": "deploy", "chat_id": team, "chat_type": "groups" }),
            "chat_type only applies",
        ),
        (
            json!({ "query": "deploy", "chat_id": team, "archived": true }),
            "archived only applies",
        ),
        (json!({}), "use get_messages"),
        (json!({ "query": "  \t" }), "use get_messages"),
        (json!({ "query": " ", "chat_id": team }), "use get_messages"),
        (json!({ "query": "x".repeat(257) }), "at most 256"),
        (json!({ "query": "deploy", "limit": 0 }), "between 1 and 50"),
        (
            json!({ "query": "deploy", "limit": 51 }),
            "between 1 and 50",
        ),
        (
            json!({ "query": "deploy", "since": "yesterday" }),
            "RFC 3339",
        ),
        (
            json!({ "query": "deploy", "since": "2026-11-07", "until": "2026-11-06" }),
            "since is after until",
        ),
        (
            json!({ "query": "deploy", "chat_id": "team" }),
            "not a chat id",
        ),
        (
            json!({ "query": "deploy", "chat_id": team, "from": "chat:404" }),
            "is a group",
        ),
        (
            json!({ "query": "deploy", "chat_id": team, "from": "bob" }),
            "not a chat id",
        ),
        (
            json!({ "query": "deploy", "chat_id": "user:999" }),
            "not one of your chats",
        ),
        (
            json!({ "query": "deploy", "chat_id": "user:101~AAAAAAAAAAA" }),
            "can't be read",
        ),
    ] {
        let message = invalid(search(&client, bad.clone()).await, &bad);
        assert!(message.contains(says), "{bad}: {message}");
    }
    // 256 characters, Cyrillic or not, are fine.
    search(&client, json!({ "query": "д".repeat(256) }))
        .await
        .unwrap();
    // Values outside the schema's enums: rmcp refuses them before the tool runs.
    for bad in [
        json!({ "query": "x", "media": "music" }),
        json!({ "query": "x", "chat_type": "bots" }),
    ] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("search_messages")
                    .with_arguments(bad.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{bad}");
        assert!(
            format!("{:?}", result.content).contains("unknown variant"),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn snippets_are_centred_on_the_match_and_hits_stay_small() {
    let long = format!(
        "{} the deploy failed {}",
        "a ".repeat(400),
        "b ".repeat(400)
    );
    let data = FakeData::new(account(ME, "Me"))
        .with_chats(vec![chat(TEAM, &"T".repeat(300), ChatKind::Supergroup)])
        .with_messages(
            TEAM,
            (1..=50)
                .map(|i| from(ALICE, message(i, i, &long)))
                .chain([with_media(
                    MediaKind::Photo,
                    message(51, 0, &format!("{} deploy", "c ".repeat(300))),
                )])
                .collect(),
        );
    let (_server, client) = client_for(data).await;
    let page = search(&client, json!({ "query": "deploy", "limit": 50 }))
        .await
        .unwrap();
    let snippet = hits(&page)[0]["snippet"].as_str().unwrap();
    assert_eq!(snippet.chars().count(), 300);
    let at = snippet.find("deploy").unwrap();
    assert!(
        (140..=152).contains(&snippet[..at].chars().count()),
        "{snippet}"
    );
    assert!(snippet.starts_with('…') && snippet.ends_with('…'));
    assert_eq!(
        hits(&page)[0]["chat"]["title"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        128
    );
    // 50 long hits fit well in the get_messages budget.
    assert!(
        page.to_string().chars().count() < 40_000,
        "{}",
        page.to_string().len()
    );

    // No query word in the text (here: no query, as when Telegram matched
    // another word form): the start of the text.
    let page = search(&client, json!({ "media": "photo" })).await.unwrap();
    let snippet = hits(&page)[0]["snippet"].as_str().unwrap();
    assert!(
        snippet.starts_with("c c c") && snippet.ends_with('…'),
        "{snippet}"
    );
    assert_eq!(snippet.chars().count(), 299);
}

#[tokio::test]
async fn a_search_flood_doesnt_stop_reading() {
    let mut flooded = data();
    flooded.search_flood_wait = Some(300);
    let (_server, client) = client_for(flooded).await;
    let err = search(&client, json!({ "query": "deploy" }))
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("wait 300 s")
            && message.contains("searches")
            && message.contains("get_messages"),
        "{message}"
    );
    call_tool(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(ALICE) }),
    )
    .await
    .unwrap();
    call_tool(&client, "list_chats", json!({})).await.unwrap();
    call_tool(
        &client,
        "get_chat_info",
        json!({ "chat_id": fake_issued_id(TEAM) }),
    )
    .await
    .unwrap();

    // And the other way round: a reading flood leaves search alone.
    let mut flooded = data();
    flooded.flood_wait = Some(300);
    let (_server, client) = client_for(flooded).await;
    assert!(
        call_tool(
            &client,
            "get_messages",
            json!({ "chat_id": fake_issued_id(ALICE) })
        )
        .await
        .is_err()
    );
    search(&client, json!({ "query": "deploy" })).await.unwrap();
}

#[tokio::test]
async fn the_tool_is_read_only_bounded_and_counts_nothing() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "search_messages").unwrap();
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));
    let output = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
    assert!(
        output.contains("\"next_cursor\"")
            && !output.contains("\"count\"")
            && !output.contains("total_count"),
        "{output}"
    );
    let input = &tool.input_schema["properties"];
    assert_eq!(input["query"]["maxLength"], 256);
    assert_eq!(input["limit"]["maximum"], 50);
    assert_eq!(input["cursor"]["maxLength"], 512);
    assert!(input["from"]["maxLength"].is_number());
    let schema = serde_json::to_string(&*tool.input_schema).unwrap();
    for value in ["video_note", "\"link\"", "\"channels\""] {
        assert!(schema.contains(value), "{value}: {schema}");
    }
    let description = tool.description.as_deref().unwrap();
    for needle in [
        "not by meaning",
        "around_id",
        "transliteration",
        "1-3",
        "untrusted content",
    ] {
        assert!(description.contains(needle), "{needle}: {description}");
    }
}

/// Hits in Saved Messages, which Telegram sends with the Account as sender
/// (`from_id`) and no `out` flag (#52), have the shape of get_messages: the
/// Account's own messages are outgoing, a saved message of Alice's is hers.
#[tokio::test]
async fn the_accounts_own_hits_in_saved_messages_are_outgoing() {
    let me = ChatId::User(ME);
    let saved = ChatSummary {
        is_self: true,
        ..chat(me, "Me", ChatKind::Private)
    };
    let alices = RawHistoryMessage {
        forwarded: true,
        forwarded_from: Some(ALICE),
        ..from(me, message(2, 2, "deploy at noon"))
    };
    let data = FakeData::new(account(ME, "Me"))
        .with_chats(vec![saved, chat(ALICE, "Alice", ChatKind::Private)])
        .with_messages(
            me,
            vec![from(me, message(1, 1, "deploy checklist")), alices],
        );
    let (_server, client) = client_for(data).await;
    let authors = |page: &Value| -> Vec<(i64, Value, Value)> {
        hits(page)
            .iter()
            .map(|h| {
                (
                    h["id"].as_i64().unwrap(),
                    h["sender"].clone(),
                    h["outgoing"].clone(),
                )
            })
            .collect()
    };
    let alice = json!({ "id": fake_issued_id(ALICE), "name": "Alice" });

    for arguments in [
        json!({ "query": "deploy" }),
        json!({ "query": "deploy", "chat_id": fake_issued_id(me) }),
    ] {
        let page = search(&client, arguments.clone()).await.unwrap();
        assert_eq!(
            authors(&page),
            [
                (2, alice.clone(), Value::Null),
                (1, Value::Null, json!(true))
            ],
            "{arguments}"
        );
    }
    // `from: "me"` still asks Telegram for the Account's messages.
    let page = search(
        &client,
        json!({ "chat_id": fake_issued_id(me), "query": "checklist", "from": "me" }),
    )
    .await
    .unwrap();
    assert_eq!(authors(&page), [(1, Value::Null, json!(true))]);
}
