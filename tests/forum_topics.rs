//! Forum Topics (#28) through `/mcp` with rmcp's own client: get_chat_info
//! lists a forum's Topics, every forum message names its Topic and a page
//! titles the Topics it mentions, and get_messages / search_messages read one
//! Topic with `topic`.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, FakeTopic, fake_issued_id},
    messages::RawHistoryMessage,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
/// A supergroup that is no forum.
const TEAM: ChatId = ChatId::Channel(303);
/// A forum: General, Releases (10, pinned), Bugs (20, closed), Offtopic (30).
const FORUM: ChatId = ChatId::Channel(505);
/// A forum with 60 Topics.
const BIG: ChatId = ChatId::Channel(606);
const FAMILY: ChatId = ChatId::Chat(404);
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

/// Message `id`, `id` minutes into DAY, in `topic` (`None`: General).
fn message(id: i32, text: &str, topic: Option<i32>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id * 60,
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
        topic,
    }
}

fn topic(id: i32, title: &str) -> FakeTopic {
    FakeTopic::new(id, title)
}

/// The forum's Topics, in Telegram's order.
fn forum_topics() -> Vec<FakeTopic> {
    vec![
        FakeTopic {
            pinned: true,
            ..topic(10, "Releases")
        },
        topic(30, "Off\u{200B}topic\u{202E}"),
        FakeTopic {
            closed: true,
            ..topic(20, "Bugs")
        },
        topic(1, "General"),
    ]
}

/// The forum's history: each Topic's first message (its id is the Topic's),
/// messages in Topics, a reply within one, and General's messages.
fn forum() -> Vec<RawHistoryMessage> {
    vec![
        message(2, "hello all", None),
        message(10, "[service message]", Some(10)),
        message(11, "release 1.0 is out", Some(10)),
        RawHistoryMessage {
            reply_to: Some(11),
            pinned: true,
            ..message(12, "release notes please", Some(10))
        },
        message(20, "[service message]", Some(20)),
        message(21, "crash on start", Some(20)),
        message(22, "release party?", None),
        message(30, "[service message]", Some(30)),
        message(31, "cats", Some(30)),
    ]
}

fn data() -> FakeData {
    FakeData::new(account(42, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice", ChatKind::Private),
            chat(TEAM, "Team", ChatKind::Supergroup),
            chat(FORUM, "Dev forum", ChatKind::Supergroup),
            chat(BIG, "Big forum", ChatKind::Supergroup),
            chat(FAMILY, "Family", ChatKind::Group),
        ])
        .with_messages(ALICE, vec![message(1, "hi", None)])
        .with_messages(TEAM, vec![message(5, "release day", None)])
        .with_messages(FORUM, forum())
        .with_messages(BIG, vec![message(100, "first", Some(100))])
        .with_topics(FORUM, forum_topics())
        .with_topics(
            BIG,
            (1..=60)
                .map(|i| topic(i * 100, &format!("Topic {i}")))
                .collect(),
        )
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

fn invalid_params(result: Result<Value, rmcp::ServiceError>) -> String {
    match result {
        Err(rmcp::ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS => {
            e.message.to_string()
        }
        other => panic!("expected invalid_params, got {other:?}"),
    }
}

fn ids(messages: &Value) -> Vec<i64> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect()
}

/// Each message's id and Topic, newest first.
fn topics_of(messages: &Value) -> Vec<(i64, Option<i64>)> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_i64().unwrap(), m["topic"].as_i64()))
        .collect()
}

#[tokio::test]
async fn a_forums_info_lists_its_topics_and_says_when_there_are_more() {
    let (_server, client) = client_with(data()).await;
    let info = call(
        &client,
        "get_chat_info",
        json!({ "chat_id": fake_issued_id(FORUM) }),
    )
    .await
    .unwrap();
    assert_eq!(
        info["topics"],
        json!([
            { "id": 10, "title": "Releases", "pinned": true },
            { "id": 30, "title": "Offtopic" },
            { "id": 20, "title": "Bugs", "closed": true },
            { "id": 1, "title": "General" },
        ]),
        "in Telegram's order, titles sanitised"
    );
    assert_eq!(info.get("topics_truncated"), None);

    let big = call(
        &client,
        "get_chat_info",
        json!({ "chat_id": fake_issued_id(BIG) }),
    )
    .await
    .unwrap();
    let listed = big["topics"].as_array().unwrap();
    assert_eq!(listed.len(), 50);
    assert_eq!(listed[0]["id"], 100);
    assert_eq!(listed[49]["id"], 5000);
    assert_eq!(big["topics_truncated"], true);

    for other in [TEAM, FAMILY, ALICE] {
        let info = call(
            &client,
            "get_chat_info",
            json!({ "chat_id": fake_issued_id(other) }),
        )
        .await
        .unwrap();
        assert_eq!(info.get("topics"), None, "{other}");
        assert_eq!(info.get("topics_truncated"), None, "{other}");
    }
}

#[tokio::test]
async fn every_forum_message_names_its_topic_and_the_page_titles_each_once() {
    // Whether Telegram describes the Topics along with the messages or they
    // are looked up, the page reads the same.
    for data in [data(), data().with_topics_in_answers()] {
        let (_server, client) = client_with(data).await;
        let page = call(
            &client,
            "get_messages",
            json!({ "chat_id": fake_issued_id(FORUM) }),
        )
        .await
        .unwrap();
        assert_eq!(
            topics_of(&page["messages"]),
            [
                (31, Some(30)),
                (30, Some(30)),
                (22, Some(1)),
                (21, Some(20)),
                (20, Some(20)),
                (12, Some(10)),
                (11, Some(10)),
                (10, Some(10)),
                (2, Some(1)),
            ]
        );
        assert_eq!(
            page["topics"],
            json!({ "1": "General", "10": "Releases", "20": "Bugs", "30": "Offtopic" })
        );

        // A page names only the Topics it mentions.
        let newest = call(
            &client,
            "get_messages",
            json!({ "chat_id": fake_issued_id(FORUM), "limit": 2 }),
        )
        .await
        .unwrap();
        assert_eq!(newest["topics"], json!({ "30": "Offtopic" }));

        // No forum, no Topics.
        for other in [TEAM, ALICE] {
            let page = call(
                &client,
                "get_messages",
                json!({ "chat_id": fake_issued_id(other) }),
            )
            .await
            .unwrap();
            assert_eq!(page.get("topics"), None, "{other}");
            assert!(
                page["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|m| m.get("topic").is_none()),
                "{other}: {page}"
            );
        }
    }
}

#[tokio::test]
async fn in_a_topic_only_real_replies_have_reply_to() {
    let (_server, client) = client_with(data()).await;
    let page = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(FORUM), "topic": 10 }),
    )
    .await
    .unwrap();
    let messages = page["messages"].as_array().unwrap();
    let by_id = |id: i64| messages.iter().find(|m| m["id"] == id).unwrap();
    assert_eq!(by_id(12)["reply_to"], 11);
    assert_eq!(by_id(11).get("reply_to"), None);
    assert_eq!(by_id(10).get("reply_to"), None);
}

#[tokio::test]
async fn get_messages_with_topic_reads_that_topic_only() {
    let (_server, client) = client_with(data()).await;
    let forum = fake_issued_id(FORUM);
    let read = |args: Value| {
        let client = &client;
        async move { call(client, "get_messages", args).await.unwrap() }
    };

    let releases = read(json!({ "chat_id": forum, "topic": 10 })).await;
    assert_eq!(ids(&releases["messages"]), [12, 11, 10]);
    assert_eq!(releases["topics"], json!({ "10": "Releases" }));

    let general = read(json!({ "chat_id": forum, "topic": 1 })).await;
    assert_eq!(ids(&general["messages"]), [22, 2]);
    assert_eq!(general["topics"], json!({ "1": "General" }));

    // Paged within the Topic.
    let first = read(json!({ "chat_id": forum, "topic": 10, "limit": 2 })).await;
    assert_eq!(ids(&first["messages"]), [12, 11]);
    assert_eq!(first["next_before_id"], 11);
    let rest = read(json!({ "chat_id": forum, "topic": 10, "limit": 2, "before_id": 11 })).await;
    assert_eq!(ids(&rest["messages"]), [10]);
    assert_eq!(rest.get("next_before_id"), None);

    // A window around a message of the Topic holds only the Topic.
    let around = read(json!({ "chat_id": forum, "topic": 20, "around_id": 21, "limit": 5 })).await;
    assert_eq!(ids(&around["messages"]), [21, 20]);
    let after = read(json!({ "chat_id": forum, "topic": 1, "after_id": 2 })).await;
    assert_eq!(ids(&after["messages"]), [22]);

    // Time bounds and the pinned filter apply within it.
    let since = read(json!({
        "chat_id": forum,
        "topic": 10,
        "since": "2026-11-06T00:11:00Z",
    }))
    .await;
    assert_eq!(ids(&since["messages"]), [12, 11]);
    let pinned = read(json!({ "chat_id": forum, "topic": 10, "pinned_only": true })).await;
    assert_eq!(ids(&pinned["messages"]), [12]);
    let pinned_general = read(json!({ "chat_id": forum, "topic": 1, "pinned_only": true })).await;
    assert_eq!(ids(&pinned_general["messages"]), Vec::<i64>::new());

    // A message of another Topic is not in this one.
    let elsewhere = call(
        &client,
        "get_messages",
        json!({ "chat_id": forum, "topic": 10, "around_id": 21 }),
    )
    .await;
    assert!(invalid_params(elsewhere).contains("topic 10"));
}

#[tokio::test]
async fn search_messages_with_topic_searches_that_topic_only() {
    for data in [data(), data().with_topics_in_answers()] {
        let (_server, client) = client_with(data).await;
        let forum = fake_issued_id(FORUM);
        let everywhere = call(
            &client,
            "search_messages",
            json!({ "chat_id": forum, "query": "release" }),
        )
        .await
        .unwrap();
        let hits: Vec<(i64, i64)> = everywhere["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["id"].as_i64().unwrap(), h["topic"].as_i64().unwrap()))
            .collect();
        assert_eq!(hits, [(22, 1), (12, 10), (11, 10)]);
        assert_eq!(
            everywhere["topics"],
            json!({ forum.clone(): { "1": "General", "10": "Releases" } })
        );

        let releases = call(
            &client,
            "search_messages",
            json!({ "chat_id": forum, "query": "release", "topic": 10 }),
        )
        .await
        .unwrap();
        assert_eq!(ids(&releases["hits"]), [12, 11]);
        assert_eq!(
            releases["topics"],
            json!({ forum.clone(): { "10": "Releases" } })
        );

        // Across all chats, forum hits carry their Topic, others none.
        let global = call(&client, "search_messages", json!({ "query": "release" }))
            .await
            .unwrap();
        for hit in global["hits"].as_array().unwrap() {
            let in_forum = hit["chat"]["id"] == forum.as_str();
            assert_eq!(hit.get("topic").is_some(), in_forum, "{hit}");
        }
        assert_eq!(
            global["topics"],
            json!({ forum.clone(): { "1": "General", "10": "Releases" } })
        );
    }
}

#[tokio::test]
async fn topic_needs_a_forum_and_a_topic_it_has() {
    let (_server, client) = client_with(data()).await;
    for other in [TEAM, ALICE, FAMILY] {
        let message = invalid_params(
            call(
                &client,
                "get_messages",
                json!({ "chat_id": fake_issued_id(other), "topic": 1 }),
            )
            .await,
        );
        assert!(message.contains("not a forum"), "{other}: {message}");
        let message = invalid_params(
            call(
                &client,
                "search_messages",
                json!({ "chat_id": fake_issued_id(other), "query": "release", "topic": 1 }),
            )
            .await,
        );
        assert!(message.contains("not a forum"), "{other}: {message}");
    }
    let forum = fake_issued_id(FORUM);
    for (tool, args) in [
        ("get_messages", json!({ "chat_id": forum, "topic": 99 })),
        (
            "search_messages",
            json!({ "chat_id": forum, "query": "release", "topic": 99 }),
        ),
    ] {
        let message = invalid_params(call(&client, tool, args).await);
        assert!(message.contains("topic 99"), "{tool}: {message}");
    }
    let message = invalid_params(
        call(
            &client,
            "get_messages",
            json!({ "chat_id": forum, "topic": 10, "thread_of": 12 }),
        )
        .await,
    );
    assert!(message.contains("thread_of"), "{message}");
    let message = invalid_params(
        call(
            &client,
            "search_messages",
            json!({ "query": "release", "topic": 10 }),
        )
        .await,
    );
    assert!(message.contains("chat_id"), "{message}");
    // A Topic id is positive, as the input schema says.
    assert!(
        call(
            &client,
            "get_messages",
            json!({ "chat_id": forum, "topic": 0 }),
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_chain_in_a_forum_names_the_topics_of_its_messages() {
    let (_server, client) = client_with(data()).await;
    let chain = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(FORUM), "thread_of": 12 }),
    )
    .await
    .unwrap();
    assert_eq!(
        topics_of(&chain["messages"]),
        [(12, Some(10)), (11, Some(10))]
    );
    assert_eq!(chain["topics"], json!({ "10": "Releases" }));
}

#[tokio::test]
async fn topics_are_bounded_in_the_schemas_and_answers_match_them() {
    let (_server, client) = client_with(data()).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = |name: &str| tools.iter().find(|t| t.name == name).unwrap().clone();

    let info = tool("get_chat_info");
    let output = info.output_schema.as_ref().unwrap();
    assert_eq!(output["properties"]["topics"]["maxItems"], 50);
    let messages = tool("get_messages");
    let output = serde_json::to_string(messages.output_schema.as_ref().unwrap()).unwrap();
    assert!(output.contains("\"maxProperties\":100"), "{output}");
    // Topic ids are the keys: each title is bounded, nothing else allowed.
    assert!(
        output.contains(
            r#""patternProperties":{"^[1-9][0-9]{0,9}$":{"maxLength":128,"type":"string"}}"#
        ),
        "{output}"
    );
    for name in ["get_messages", "search_messages"] {
        let input = tool(name).input_schema;
        assert_eq!(input["properties"]["topic"]["minimum"], 1, "{name}");
        assert!(tool(name).description.unwrap().contains("topic"), "{name}");
    }

    let forum = fake_issued_id(FORUM);
    for (name, args) in [
        ("get_chat_info", json!({ "chat_id": forum })),
        ("get_chat_info", json!({ "chat_id": fake_issued_id(BIG) })),
        ("get_messages", json!({ "chat_id": forum })),
        ("get_messages", json!({ "chat_id": forum, "topic": 10 })),
        ("search_messages", json!({ "query": "release" })),
    ] {
        let schema = Value::Object((*tool(name).output_schema.clone().unwrap()).clone());
        let validator = jsonschema::validator_for(&schema).unwrap();
        let answer = call(&client, name, args.clone()).await.unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&answer)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{name} {args}: {answer}: {errors:?}");
    }
}
