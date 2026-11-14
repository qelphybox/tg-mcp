//! Comments under channel posts (#55) through `/mcp` with rmcp's own client:
//! a post says how many comments it has, and `comments_of` reads them from
//! the channel's discussion group, newest first, paged like a history.

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

const ALICE: ChatId = ChatId::User(101);
const NEWS: ChatId = ChatId::Channel(202);
/// NEWS's discussion group; the account is not a member.
const NEWS_CHAT: ChatId = ChatId::Channel(303);
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
        muted: false,
        marked_unread: false,
        last_message: None,
    }
}

fn message(id: i32, text: &str, reply_to: Option<i32>, comments: Option<u32>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id * 60,
        from: None,
        out: false,
        text: text.into(),
        media: None,
        reply_to,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
        topic: None,
        comments,
    }
}

/// Post 10 has three comments (one replying to another), post 11 has
/// comments off, post 12 a discussion nobody wrote in.
async fn client() -> (common::TestServer, Client) {
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice", ChatKind::Private),
            chat(NEWS, "News", ChatKind::Channel),
        ])
        .with_messages(
            NEWS,
            vec![
                message(10, "post with comments", None, Some(3)),
                message(11, "post without", None, None),
                message(12, "post nobody commented", None, Some(0)),
            ],
        )
        .with_messages(
            NEWS_CHAT,
            vec![
                message(100, "post with comments", None, None),
                message(101, "first\u{200B} comment", Some(100), None),
                message(102, "second comment", Some(100), None),
                message(103, "a reply to the first", Some(101), None),
                message(104, "chatter elsewhere", None, None),
                message(200, "post nobody commented", None, None),
            ],
        )
        .with_discussion(NEWS, 10, NEWS_CHAT, 100)
        .with_discussion(NEWS, 12, NEWS_CHAT, 200);
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn call(client: &Client, arguments: Value) -> Result<Value, rmcp::ServiceError> {
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

fn invalid_params(result: Result<Value, rmcp::ServiceError>) -> String {
    match result {
        Err(rmcp::ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS => {
            e.message.to_string()
        }
        other => panic!("expected invalid_params, got {other:?}"),
    }
}

#[tokio::test]
async fn a_channel_post_says_how_many_comments_it_has() {
    let (_server, client) = client().await;
    let page = call(&client, json!({ "chat_id": fake_issued_id(NEWS) }))
        .await
        .unwrap();
    let comments: Vec<(i64, Value)> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_i64().unwrap(), m["comments"].clone()))
        .collect();
    assert_eq!(
        comments,
        [(12, json!(0)), (11, Value::Null), (10, json!(3))]
    );
}

#[tokio::test]
async fn comments_are_read_newest_first_from_the_discussion_group() {
    let (_server, client) = client().await;
    let page = call(
        &client,
        json!({ "chat_id": fake_issued_id(NEWS), "comments_of": 10 }),
    )
    .await
    .unwrap();
    assert_eq!(page["chat_id"], fake_issued_id(NEWS_CHAT));
    assert_eq!(
        page["comments_of"],
        json!({ "chat_id": fake_issued_id(NEWS), "message_id": 10 })
    );
    assert_eq!(ids(&page), [103, 102, 101]);
    // Sanitised like any message.
    assert_eq!(page["messages"][2]["text"], "first comment");
    assert_eq!(page["messages"][0]["reply_to"], 101);
    assert!(page.get("next_before_id").is_none(), "{page}");
}

#[tokio::test]
async fn comments_page_with_limit_and_before_id() {
    let (_server, client) = client().await;
    let first = call(
        &client,
        json!({ "chat_id": fake_issued_id(NEWS), "comments_of": 10, "limit": 2 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&first), [103, 102]);
    assert_eq!(first["next_before_id"], 102);
    let second = call(
        &client,
        json!({
            "chat_id": fake_issued_id(NEWS),
            "comments_of": 10,
            "limit": 2,
            "before_id": 102,
        }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&second), [101]);
    assert!(second.get("next_before_id").is_none(), "{second}");
}

#[tokio::test]
async fn a_discussion_nobody_wrote_in_is_an_empty_page() {
    let (_server, client) = client().await;
    let page = call(
        &client,
        json!({ "chat_id": fake_issued_id(NEWS), "comments_of": 12 }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), Vec::<i64>::new());
    assert_eq!(page["chat_id"], fake_issued_id(NEWS_CHAT));
    assert!(page.get("next_before_id").is_none(), "{page}");
}

#[tokio::test]
async fn a_post_with_comments_off_is_a_clear_error() {
    let (_server, client) = client().await;
    let message = invalid_params(
        call(
            &client,
            json!({ "chat_id": fake_issued_id(NEWS), "comments_of": 11 }),
        )
        .await,
    );
    assert!(
        message.contains("comments are off for this post"),
        "{message}"
    );
}

#[tokio::test]
async fn comments_of_takes_only_limit_and_before_id_and_only_a_channel() {
    let (_server, client) = client().await;
    let news = fake_issued_id(NEWS);
    for (other, value) in [
        ("around_id", json!(10)),
        ("after_id", json!(10)),
        ("thread_of", json!(10)),
        ("pinned_only", json!(true)),
        ("since", json!("2026-11-01")),
        ("until", json!("2026-11-30")),
        ("topic", json!(1)),
    ] {
        let mut arguments = json!({ "chat_id": news, "comments_of": 10 });
        arguments[other] = value;
        let message = invalid_params(call(&client, arguments).await);
        assert_eq!(
            message,
            format!("comments_of can't be combined with {other}"),
            "{other}"
        );
    }
    let message = invalid_params(
        call(
            &client,
            json!({ "chat_id": fake_issued_id(NEWS), "comments_of": 0 }),
        )
        .await,
    );
    assert!(
        message.contains("comments_of must be a positive message id"),
        "{message}"
    );
    let message = invalid_params(
        call(
            &client,
            json!({ "chat_id": fake_issued_id(ALICE), "comments_of": 10 }),
        )
        .await,
    );
    assert!(
        message.contains("comments_of is for channel posts"),
        "{message}"
    );
}
