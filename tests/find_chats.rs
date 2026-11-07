//! `find_chats` through `/mcp` with rmcp's own client, on the fake's model of
//! Telegram's `contacts.search` (src/tg/fake.rs, `contacts_search`): the
//! Account's own Chats and contacts in `my_results`, public Chats it hasn't
//! joined (and its own public ones again) in the global `results`.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, fake_issued_id},
    messages::RawHistoryMessage,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
/// A public supergroup the Account is in: in `my_results` and in `results`.
const ACCOUNTING: ChatId = ChatId::Channel(303);
const FAMILY: ChatId = ChatId::Chat(404);
/// A contact the Account has no chat with.
const CAROL: ChatId = ChatId::User(105);
/// A chat Telegram describes as "min": no access hash to hand out.
const DAVE: ChatId = ChatId::User(106);
/// A public channel the Account hasn't joined: only in `results`.
const PUBLIC: ChatId = ChatId::Channel(909);
/// A public bot the Account never talked to: only in `results`.
const PUBLIC_BOT: ChatId = ChatId::User(910);

fn chat(id: ChatId, title: &str, kind: ChatKind, username: Option<&str>) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: username.map(str::to_string),
        is_self: false,
        unread_count: 3,
        pinned: false,
        last_message: None,
    }
}

fn data() -> FakeData {
    FakeData::new(account(ME, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice Smith", ChatKind::Private, Some("alice_s")),
            chat(
                ACCOUNTING,
                "Accounting",
                ChatKind::Supergroup,
                Some("books_dept"),
            ),
            chat(FAMILY, "Family", ChatKind::Group, None),
            chat(DAVE, "Dave Accardi", ChatKind::Private, None),
        ])
        .with_archived(vec![FAMILY])
        .with_contacts(vec![chat(
            CAROL,
            "Carol Accountant",
            ChatKind::Private,
            None,
        )])
        .with_public(vec![
            chat(
                PUBLIC,
                "Accounting News",
                ChatKind::Channel,
                Some("acc_news"),
            ),
            chat(PUBLIC_BOT, "Books Bot", ChatKind::Bot, Some("books_bot")),
        ])
        .with_min_peers(vec![DAVE])
        .with_messages(
            ALICE,
            vec![RawHistoryMessage {
                id: 1,
                date: 1_793_923_200,
                from: None,
                out: false,
                text: "hi".into(),
                media: None,
                reply_to: None,
                reply_elsewhere: None,
                forwarded: false,
                edited: false,
            }],
        )
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

async fn find(client: &Client, arguments: Value) -> Result<Value, rmcp::ServiceError> {
    call_tool(client, "find_chats", arguments).await
}

fn ids(found: &Value) -> Vec<String> {
    found["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
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
async fn own_chats_and_contacts_are_found_and_public_ones_never() {
    let (_server, client) = client().await;
    let found = find(&client, json!({ "query": "acc" })).await.unwrap();
    // Accounting is in both of Telegram's lists: once. Accounting News is a
    // public channel the account isn't in: never.
    assert_eq!(
        ids(&found),
        [
            fake_issued_id(ACCOUNTING),
            "user:106".to_string(),
            fake_issued_id(CAROL),
        ]
    );
    assert_eq!(
        found["chats"][0],
        json!({
            "id": fake_issued_id(ACCOUNTING),
            "title": "Accounting",
            "type": "supergroup",
            "username": "books_dept",
        }),
        "no unread count, no last message"
    );
    assert_eq!(
        found["chats"][2],
        json!({ "id": fake_issued_id(CAROL), "title": "Carol Accountant", "type": "private" })
    );

    // By @username: own public chats yes, public ones the account isn't in no.
    let found = find(&client, json!({ "query": "books" })).await.unwrap();
    assert_eq!(ids(&found), [fake_issued_id(ACCOUNTING)]);
    let found = find(&client, json!({ "query": "acc_news" })).await.unwrap();
    assert_eq!(ids(&found), Vec::<String>::new());

    // The Archive is the account's too.
    let found = find(&client, json!({ "query": "fam" })).await.unwrap();
    assert_eq!(ids(&found), ["chat:404"]);
}

#[tokio::test]
async fn at_name_and_name_give_the_same_results() {
    let (_server, client) = client().await;
    let plain = find(&client, json!({ "query": "books" })).await.unwrap();
    assert_eq!(ids(&plain), [fake_issued_id(ACCOUNTING)]);
    for query in ["@books", " @books ", "@books_dept", "BOOKS"] {
        let found = find(&client, json!({ "query": query })).await.unwrap();
        assert_eq!(found, plain, "{query}");
    }
    let found = find(&client, json!({ "query": "@alice_s" })).await.unwrap();
    assert_eq!(ids(&found), [fake_issued_id(ALICE)]);
}

#[tokio::test]
async fn every_id_found_is_issued_and_works_with_the_other_tools() {
    let (_server, client) = client().await;
    let mut seen = Vec::new();
    for query in ["a", "acc", "alice", "carol", "fam", "books", "dave"] {
        let found = find(&client, json!({ "query": query })).await.unwrap();
        let ids = ids(&found);
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{query}: a chat repeats");
        seen.extend(ids);
    }
    seen.sort();
    seen.dedup();
    assert!(!seen.iter().any(|id| id.starts_with("channel:909")));
    assert!(!seen.iter().any(|id| id.starts_with("user:910")));
    for id in &seen {
        let chat = id.split('~').next().unwrap().parse::<ChatId>().unwrap();
        let expected = match chat {
            // Min: Telegram gave no hash that may be handed out.
            DAVE => chat.to_string(),
            _ => fake_issued_id(chat),
        };
        assert_eq!(id, &expected);
        let info = call_tool(&client, "get_chat_info", json!({ "chat_id": id }))
            .await
            .unwrap();
        assert_eq!(info["id"], fake_issued_id(chat), "{id}");
    }
    assert_eq!(seen.len(), 5, "{seen:?}");

    // A contact without a chat: a profile, and an empty history.
    let carol = fake_issued_id(CAROL);
    let history = call_tool(&client, "get_messages", json!({ "chat_id": carol }))
        .await
        .unwrap();
    assert_eq!(history["messages"], json!([]));
    let history = call_tool(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(ALICE) }),
    )
    .await
    .unwrap();
    assert_eq!(history["messages"].as_array().unwrap().len(), 1);
    // A contact isn't a chat of the list.
    let listed = call_tool(&client, "list_chats", json!({})).await.unwrap();
    assert!(!listed.to_string().contains(&carol));
}

#[tokio::test]
async fn limit_caps_the_answer_and_titles_are_cut() {
    let chats: Vec<ChatSummary> = (1..=40)
        .map(|i| {
            chat(
                ChatId::Channel(1000 + i),
                &format!("Project {i} {}", "x".repeat(300)),
                ChatKind::Supergroup,
                None,
            )
        })
        .collect();
    let (_server, client) =
        client_for(FakeData::new(account(ME, "Me")).with_chats(chats.clone())).await;
    let found = find(&client, json!({ "query": "project" })).await.unwrap();
    assert_eq!(ids(&found).len(), 10, "default 10");
    let found = find(&client, json!({ "query": "project", "limit": 30 }))
        .await
        .unwrap();
    let expected: Vec<String> = (1..=30)
        .map(|i| fake_issued_id(ChatId::Channel(1000 + i)))
        .collect();
    assert_eq!(ids(&found), expected, "Telegram's order, cut at 30");
    for chat in found["chats"].as_array().unwrap() {
        let title = chat["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), 128, "{title}");
        assert!(title.ends_with('…'));
    }
    assert!(
        found.to_string().len() < 8_000,
        "{}",
        found.to_string().len()
    );
}

#[tokio::test]
async fn empty_overlong_or_unbounded_parameters_are_invalid_params() {
    let (_server, client) = client().await;
    for (bad, says) in [
        (json!({ "query": "" }), "query is empty"),
        (json!({ "query": "  " }), "query is empty"),
        (json!({ "query": "@" }), "query is empty"),
        (json!({ "query": " @ " }), "query is empty"),
        (json!({ "query": "x".repeat(65) }), "at most 64"),
        (json!({ "query": "acc", "limit": 0 }), "between 1 and 30"),
        (json!({ "query": "acc", "limit": 31 }), "between 1 and 30"),
    ] {
        let message = invalid(find(&client, bad.clone()).await, &bad);
        assert!(message.contains(says), "{bad}: {message}");
    }
    // 64 characters, Cyrillic or not, are fine.
    find(&client, json!({ "query": "д".repeat(64) }))
        .await
        .unwrap();
    // query is required.
    let result = client
        .call_tool(CallToolRequestParams::new("find_chats").with_arguments(Default::default()))
        .await;
    assert!(
        result.is_err() || result.as_ref().unwrap().is_error == Some(true),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_search_flood_holds_back_find_chats_but_not_reading() {
    let mut flooded = data();
    flooded.search_flood_wait = Some(300);
    let (_server, client) = client_for(flooded).await;
    let message = find(&client, json!({ "query": "acc" }))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("wait 300 s") && message.contains("searches"),
        "{message}"
    );
    call_tool(&client, "list_chats", json!({})).await.unwrap();

    // A reading flood leaves it alone.
    let mut flooded = data();
    flooded.flood_wait = Some(300);
    let (_server, client) = client_for(flooded).await;
    assert!(call_tool(&client, "list_chats", json!({})).await.is_err());
    find(&client, json!({ "query": "acc" })).await.unwrap();
}

#[tokio::test]
async fn the_tool_is_read_only_closed_world_and_bounded() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "find_chats").unwrap();
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.open_world_hint, Some(false));
    let output = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
    assert!(
        output.contains("\"chats\"")
            && !output.contains("unread_count")
            && !output.contains("last_message"),
        "{output}"
    );
    let input = &tool.input_schema["properties"];
    assert_eq!(input["query"]["maxLength"], 64);
    assert_eq!(input["limit"]["maximum"], 30);
    assert_eq!(tool.input_schema["required"], json!(["query"]));
    let description = tool.description.as_deref().unwrap();
    for needle in [
        "@username",
        "contacts",
        "hasn't joined",
        "page through list_chats",
        "untrusted content",
    ] {
        assert!(description.contains(needle), "{needle}: {description}");
    }
}
