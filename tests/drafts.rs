//! Drafts (#43) through `/mcp` with rmcp's own client: save_draft (Scope
//! `draft`) writes, replaces and clears a chat's Draft, Markdown becomes
//! Telegram's entities, forums need a Topic, and get_drafts (Scope `read`)
//! lists the account's Drafts newest first.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, FakeDraft, FakeTopic, fake_issued_id},
    markdown::{Entity, EntityKind},
    messages::RawHistoryMessage,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
/// A supergroup that is no forum.
const TEAM: ChatId = ChatId::Channel(303);
/// A forum: General and Releases (10).
const FORUM: ChatId = ChatId::Channel(505);
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

fn message(id: i32, topic: Option<i32>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id,
        from: None,
        out: false,
        text: "a message".into(),
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

/// The user typed a Draft in Team on their phone the day before, with
/// characters that hide text.
fn data() -> FakeData {
    FakeData::new(account(ME, "Me"))
        .with_chats(vec![
            chat(ALICE, "Alice", ChatKind::Private),
            chat(TEAM, "Team\u{202E}", ChatKind::Supergroup),
            chat(FORUM, "Dev forum", ChatKind::Supergroup),
        ])
        .with_messages(ALICE, vec![message(1, None), message(2, None)])
        .with_messages(TEAM, vec![message(7, None)])
        .with_messages(
            FORUM,
            vec![
                message(5, None),
                message(10, Some(10)),
                message(12, Some(10)),
            ],
        )
        .with_topics(
            FORUM,
            vec![FakeTopic::new(10, "Releases"), FakeTopic::new(1, "General")],
        )
        .with_drafts(vec![FakeDraft::new(
            TEAM,
            "see you\u{200B} tomorrow\u{E0041}",
            DAY - 86_400,
        )])
}

/// A server with `data` and a client whose Grant has `scope`.
async fn client_with(data: FakeData, scope: &str) -> (common::TestServer, Client) {
    let server = start_with(vec![data]).await;
    let consent: &[(&str, &str)] = if scope == "read draft" {
        &[("scope", "draft"), ("decision", "allow")]
    } else {
        &[("decision", "allow")]
    };
    let (_, tokens) = server.login_scoped(Some(scope), consent).await;
    assert_eq!(tokens["scope"], scope);
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn drafter() -> (common::TestServer, Client) {
    client_with(data(), "read draft").await
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

async fn save(client: &Client, arguments: Value) -> Value {
    call(client, "save_draft", arguments).await.unwrap()
}

fn invalid_params(result: Result<Value, rmcp::ServiceError>) -> String {
    match result {
        Err(rmcp::ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS => {
            e.message.to_string()
        }
        other => panic!("expected invalid_params, got {other:?}"),
    }
}

/// (chat id, topic, text) of each Draft get_drafts lists, in its order.
async fn listed(client: &Client) -> Vec<(String, Option<i64>, String)> {
    let drafts = call(client, "get_drafts", json!({})).await.unwrap();
    drafts["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["chat"]["id"].as_str().unwrap().to_string(),
                d["topic"].as_i64(),
                d["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn id(chat: ChatId) -> String {
    fake_issued_id(chat)
}

#[tokio::test]
async fn save_replaces_and_clears_and_get_drafts_lists_them_newest_first() {
    let (server, client) = drafter().await;
    let team = (id(TEAM), None, "see you tomorrow".to_string());
    assert_eq!(
        listed(&client).await,
        std::slice::from_ref(&team),
        "hidden characters removed"
    );

    let saved = save(
        &client,
        json!({ "chat_id": id(ALICE), "text": "**hi** there" }),
    )
    .await;
    assert_eq!(
        saved,
        json!({ "status": "saved", "chat_id": id(ALICE), "chars": 8, "entities": 1 })
    );
    let alice = |text: &str| (id(ALICE), None, text.to_string());
    assert_eq!(listed(&client).await, [alice("hi there"), team.clone()]);

    // Saving again replaces the Draft: still one in Alice's chat.
    save(
        &client,
        json!({ "chat_id": id(ALICE), "text": "second", "reply_to": 2 }),
    )
    .await;
    let drafts = call(&client, "get_drafts", json!({})).await.unwrap();
    assert_eq!(drafts["drafts"][0]["reply_to"], 2);
    assert_eq!(drafts["drafts"][0]["chat"]["title"], "Alice");
    assert_eq!(drafts["drafts"][0]["chat"]["type"], "private");
    assert_eq!(drafts["drafts"][1]["chat"]["title"], "Team");
    assert_eq!(drafts.get("truncated"), None);
    assert_eq!(listed(&client).await, [alice("second"), team]);

    // The newest is first, whichever chat it is in.
    save(&client, json!({ "chat_id": id(TEAM), "text": "on my way" })).await;
    assert_eq!(
        listed(&client).await,
        [(id(TEAM), None, "on my way".into()), alice("second")]
    );

    let cleared = save(&client, json!({ "chat_id": id(ALICE), "text": "" })).await;
    assert_eq!(
        cleared,
        json!({ "status": "cleared", "chat_id": id(ALICE), "chars": 0, "entities": 0 })
    );
    assert_eq!(
        listed(&client).await,
        [(id(TEAM), None, "on my way".into())]
    );
    // Clearing what is not there is no error: the result is the same.
    let again = save(&client, json!({ "chat_id": id(ALICE), "text": "" })).await;
    assert_eq!(again, cleared);

    // Telegram keeps them for the Account, so another Grant sees them too.
    let (_, tokens) = server.login().await;
    let other = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    assert_eq!(listed(&other).await, [(id(TEAM), None, "on my way".into())]);
}

#[tokio::test]
async fn markdown_becomes_entities_and_plain_is_sent_as_is() {
    let (server, client) = drafter().await;
    let saved = save(
        &client,
        json!({
            "chat_id": id(ALICE),
            "text": "😀 **Release** _notes_: run `cargo test`, see [the PR](https://github.com/x/y/pull/1)\n```rust\nfn main() {}\n```",
        }),
    )
    .await;
    assert_eq!(saved["entities"], 5);
    let drafts = server.connector.drafts().of(ME);
    let text = &drafts[0].text;
    assert_eq!(
        text.text,
        "😀 Release notes: run cargo test, see the PR\nfn main() {}"
    );
    let entity = |kind, offset, length| Entity {
        kind,
        offset,
        length,
    };
    assert_eq!(
        text.entities,
        [
            // 😀 is two UTF-16 code units.
            entity(EntityKind::Bold, 3, 7),
            entity(EntityKind::Italic, 11, 5),
            entity(EntityKind::Code, 22, 10),
            entity(
                EntityKind::TextUrl {
                    url: "https://github.com/x/y/pull/1".into()
                },
                38,
                6
            ),
            entity(
                EntityKind::Pre {
                    language: "rust".into()
                },
                45,
                12
            ),
        ]
    );
    // get_drafts reads the text back without its formatting.
    assert_eq!(listed(&client).await[0].2, text.text);

    let raw = "**not bold** and `not code`";
    let plain = save(
        &client,
        json!({ "chat_id": id(ALICE), "text": raw, "format": "plain" }),
    )
    .await;
    assert_eq!(plain["entities"], 0);
    assert_eq!(plain["chars"], raw.chars().count());
    let drafts = server.connector.drafts().of(ME);
    assert_eq!(drafts[0].text.text, raw);
    assert!(drafts[0].text.entities.is_empty());
}

#[tokio::test]
async fn text_over_4096_characters_is_invalid_and_never_split() {
    let (server, client) = drafter().await;
    let at_limit = "a".repeat(4096);
    let saved = save(&client, json!({ "chat_id": id(ALICE), "text": at_limit })).await;
    assert_eq!(saved["chars"], 4096);

    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": id(ALICE), "text": "b".repeat(4097) }),
        )
        .await,
    );
    assert!(msg.contains("4097") && msg.contains("not split"), "{msg}");
    // Markers don't count; once formatted it is what counts.
    let bold = save(
        &client,
        json!({ "chat_id": id(ALICE), "text": format!("**{}**", "c".repeat(4096)) }),
    )
    .await;
    assert_eq!(
        (bold["chars"].as_i64(), bold["entities"].as_i64()),
        (Some(4096), Some(1))
    );
    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": id(ALICE), "text": format!("**{}**", "c".repeat(4096)), "format": "plain" }),
        )
        .await,
    );
    assert!(msg.contains("4100"), "{msg}");
    // The refused ones changed nothing.
    let drafts = server.connector.drafts().of(ME);
    assert_eq!(drafts[0].text.text, "c".repeat(4096));

    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": id(ALICE), "text": "  \n " }),
        )
        .await,
    );
    assert!(msg.contains("only whitespace"), "{msg}");
}

#[tokio::test]
async fn a_forum_draft_needs_a_topic_or_a_reply_in_one() {
    let (_server, client) = drafter().await;
    let forum = id(FORUM);
    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": forum, "text": "hi" }),
        )
        .await,
    );
    assert!(msg.contains("is a forum") && msg.contains("topic"), "{msg}");

    let saved = save(
        &client,
        json!({ "chat_id": forum, "text": "in releases", "topic": 10 }),
    )
    .await;
    assert_eq!(saved["topic"], 10);
    // A reply in Releases puts the Draft there, replacing that Topic's Draft.
    let reply = save(
        &client,
        json!({ "chat_id": forum, "text": "reply", "reply_to": 12 }),
    )
    .await;
    assert_eq!(
        (reply["topic"].as_i64(), reply["reply_to"].as_i64()),
        (Some(10), Some(12))
    );
    let general = save(
        &client,
        json!({ "chat_id": forum, "text": "in general", "reply_to": 5 }),
    )
    .await;
    assert_eq!(general["topic"], 1);
    save(
        &client,
        json!({ "chat_id": forum, "text": "general again", "topic": 1 }),
    )
    .await;
    let drafts = listed(&client).await;
    assert_eq!(
        drafts[..2],
        [
            (forum.clone(), Some(1), "general again".into()),
            (forum.clone(), Some(10), "reply".into()),
        ],
        "one Draft per Topic"
    );

    for (arguments, expected) in [
        (
            json!({ "topic": 10, "reply_to": 5 }),
            "in Topic 1, not in topic 10",
        ),
        (json!({ "reply_to": 99 }), "reply_to 99 is not a message"),
        (json!({ "topic": 77 }), "topic 77 is not a Topic"),
    ] {
        let mut arguments = arguments.as_object().unwrap().clone();
        arguments.insert("chat_id".into(), json!(forum));
        arguments.insert("text".into(), json!("x"));
        let msg = invalid_params(call(&client, "save_draft", Value::Object(arguments)).await);
        assert!(msg.contains(expected), "{msg}");
    }

    // topic only means something in a forum.
    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": id(TEAM), "text": "x", "topic": 1 }),
        )
        .await,
    );
    assert!(msg.contains("not a forum"), "{msg}");
}

#[tokio::test]
async fn any_chat_of_the_account_works_and_no_other() {
    let (_server, client) = drafter().await;
    // Saved Messages, and a basic group by its id alone.
    let saved = save(
        &client,
        json!({ "chat_id": id(ChatId::User(ME)), "text": "note to self" }),
    )
    .await;
    assert_eq!(saved["status"], "saved");
    let drafts = call(&client, "get_drafts", json!({})).await.unwrap();
    assert_eq!(drafts["drafts"][0]["chat"]["is_self"], true);

    let msg = invalid_params(
        call(
            &client,
            "save_draft",
            json!({ "chat_id": id(ChatId::User(999)), "text": "x" }),
        )
        .await,
    );
    assert!(msg.contains("not one of your chats"), "{msg}");
    for bad in [
        json!({ "chat_id": "user:abc", "text": "x" }),
        json!({ "chat_id": id(ALICE), "text": "x", "reply_to": 0 }),
        json!({ "chat_id": id(ALICE), "text": "x", "format": "html" }),
    ] {
        let answer = client
            .call_tool(
                CallToolRequestParams::new("save_draft")
                    .with_arguments(bad.as_object().unwrap().clone()),
            )
            .await;
        // A JSON-RPC error, or (an unknown format) a tool result marked as one.
        assert!(
            !matches!(&answer, Ok(r) if r.is_error != Some(true)),
            "{bad}: {answer:?}"
        );
    }
}

#[tokio::test]
async fn a_read_grant_lists_get_drafts_but_not_save_draft_and_may_not_call_it() {
    let (server, reader) = client_with(data(), "read").await;
    let names: Vec<String> = reader
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"get_drafts".to_string()));
    assert!(!names.contains(&"save_draft".to_string()));
    let refused = call(
        &reader,
        "save_draft",
        json!({ "chat_id": id(ALICE), "text": "x" }),
    )
    .await
    .expect_err("needs draft");
    assert!(
        refused.to_string().contains("needs the `draft` Scope"),
        "{refused}"
    );
    assert_eq!(
        listed(&reader).await.len(),
        1,
        "reading Drafts needs only read"
    );
    assert_eq!(server.connector.drafts().of(ME).len(), 1, "nothing written");

    let (_server, drafter) = drafter().await;
    let tools = drafter.list_all_tools().await.unwrap();
    let save = tools.iter().find(|t| t.name == "save_draft").unwrap();
    let hints = save.annotations.as_ref().unwrap();
    assert_eq!(
        (
            hints.read_only_hint,
            hints.destructive_hint,
            hints.idempotent_hint,
            hints.open_world_hint
        ),
        (Some(false), Some(false), Some(true), Some(false))
    );
    let schema = serde_json::to_value(&save.input_schema).unwrap();
    assert_eq!(schema["properties"]["text"]["maxLength"], 16384, "{schema}");
    assert!(save.output_schema.is_some());
    let get = tools.iter().find(|t| t.name == "get_drafts").unwrap();
    assert_eq!(get.annotations.as_ref().unwrap().read_only_hint, Some(true));
    let output = serde_json::to_value(get.output_schema.as_ref().unwrap()).unwrap();
    assert_eq!(output["properties"]["drafts"]["maxItems"], 100, "{output}");
}

#[tokio::test]
async fn a_write_flood_wait_is_reported_and_reading_goes_on() {
    let flooded = FakeData {
        writes_flood_wait: Some(120),
        ..data()
    };
    let (server, client) = client_with(flooded, "read draft").await;
    let err = call(
        &client,
        "save_draft",
        json!({ "chat_id": id(ALICE), "text": "x" }),
    )
    .await
    .expect_err("flooded");
    let msg = err.to_string();
    assert!(
        msg.contains("wait 120 s before writing again") && msg.contains("Nothing was written"),
        "{msg}"
    );
    assert_eq!(server.connector.drafts().of(ME).len(), 1);
    assert_eq!(listed(&client).await.len(), 1);
}
