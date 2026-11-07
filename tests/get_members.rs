//! `get_members` through `/mcp` with rmcp's own client, on the fake's model of
//! Telegram's member lists (src/tg/fake.rs, `MemberSource`): a large
//! supergroup Telegram stops listing before its end, a basic group, a group
//! that hides its members, a broadcast channel the account doesn't administer
//! and a basic group the account left.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, FakeMember, FakeMembers, fake_issued_id},
    members::Role,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ME: i64 = 42;
/// A supergroup of 450 members, of which Telegram lists the first 400.
const BIG: ChatId = ChatId::Channel(300);
const BIG_MEMBERS: usize = 450;
const BIG_CAP: usize = 400;
const FAMILY: ChatId = ChatId::Chat(404);
/// A basic group the account left.
const LEFT: ChatId = ChatId::Chat(405);
/// A broadcast channel the account only reads.
const NEWS: ChatId = ChatId::Channel(500);
/// A supergroup that hides its members from non-admins.
const SECRET: ChatId = ChatId::Channel(600);

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

fn member(id: i64, name: &str, username: Option<&str>, role: Role) -> FakeMember {
    FakeMember {
        id,
        name: name.into(),
        username: username.map(str::to_string),
        bot: false,
        role,
    }
}

fn bot(id: i64, name: &str, username: &str) -> FakeMember {
    FakeMember {
        bot: true,
        ..member(id, name, Some(username), Role::Member)
    }
}

fn big_members() -> Vec<FakeMember> {
    let mut members = vec![
        member(1, "Olga Petrova", Some("olga_p"), Role::Creator),
        member(2, "Anna Smith", None, Role::Admin),
        bot(3, "Helper", "helper_bot"),
        member(ME, "Me", Some("me"), Role::Member),
        member(5, "Joanna Ivanova", Some("annie"), Role::Member),
        // Hidden characters and a name longer than the tool shows.
        member(
            6,
            &format!("Ma\u{200B}x {}", "Long".repeat(30)),
            Some("max\u{2060}"),
            Role::Admin,
        ),
    ];
    let filler = BIG_MEMBERS - members.len();
    members.extend(
        (0..filler as i64).map(|n| member(1000 + n, &format!("User {n}"), None, Role::Member)),
    );
    members
}

async fn client() -> (common::TestServer, Client) {
    let data = FakeData::new(account(ME, "Me"))
        .with_chats(vec![
            chat(BIG, "Big", ChatKind::Supergroup),
            chat(FAMILY, "Family", ChatKind::Group),
            chat(LEFT, "Old group", ChatKind::Group),
            chat(NEWS, "News", ChatKind::Channel),
            chat(SECRET, "Secret", ChatKind::Supergroup),
            chat(ChatId::User(101), "Alice", ChatKind::Private),
        ])
        .with_members(
            BIG,
            FakeMembers {
                members: big_members(),
                cap: Some(BIG_CAP),
                ..FakeMembers::default()
            },
        )
        .with_members(
            FAMILY,
            FakeMembers {
                members: vec![
                    member(11, "Mom", None, Role::Creator),
                    member(12, "Dad", Some("dad"), Role::Admin),
                    member(ME, "Me", Some("me"), Role::Member),
                    member(13, "Sister Ann", None, Role::Member),
                    bot(14, "Family Bot", "fam_bot"),
                ],
                ..FakeMembers::default()
            },
        )
        .with_members(
            LEFT,
            FakeMembers {
                forbidden: true,
                ..FakeMembers::default()
            },
        )
        .with_members(
            NEWS,
            FakeMembers {
                members: vec![
                    member(21, "Editor", None, Role::Creator),
                    member(ME, "Me", Some("me"), Role::Member),
                ],
                admins_required: true,
                ..FakeMembers::default()
            },
        )
        .with_members(
            SECRET,
            FakeMembers {
                members: vec![
                    member(31, "Boss", None, Role::Creator),
                    member(32, "Quiet One", None, Role::Member),
                    bot(33, "Guard", "guard_bot"),
                    member(ME, "Me", Some("me"), Role::Member),
                    member(34, "Mod", None, Role::Admin),
                ],
                hidden: true,
                ..FakeMembers::default()
            },
        );
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

async fn members(client: &Client, arguments: Value) -> Result<Value, rmcp::ServiceError> {
    call(client, "get_members", arguments).await
}

fn invalid_params(result: Result<Value, rmcp::ServiceError>, case: &str) -> String {
    match result {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{case}: {err:?}");
            err.message.to_string()
        }
        other => panic!("{case} was accepted: {other:?}"),
    }
}

fn names(page: &Value) -> Vec<String> {
    page["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_supergroup_pages_through_its_members_until_telegram_stops() {
    let (_server, client) = client().await;
    let big = fake_issued_id(BIG);

    let first = members(&client, json!({ "chat_id": big })).await.unwrap();
    let shown = first["members"].as_array().unwrap();
    assert_eq!(shown.len(), 50, "the default limit");
    assert_eq!(first["total"], BIG_MEMBERS);
    assert_eq!(first.get("members_hidden"), None);
    assert_eq!(
        shown[0],
        json!({
            "id": fake_issued_id(ChatId::User(1)),
            "name": "Olga Petrova",
            "username": "olga_p",
            "role": "creator",
        })
    );
    assert_eq!(shown[1]["role"], "admin");
    assert_eq!(
        shown[2],
        json!({
            "id": fake_issued_id(ChatId::User(3)),
            "name": "Helper",
            "username": "helper_bot",
            "role": "member",
            "bot": true,
        })
    );
    assert_eq!(shown[3]["is_self"], true, "the account itself");
    // Sanitised, then cut to 64 characters on one line.
    let long = shown[5]["name"].as_str().unwrap();
    assert!(long.starts_with("Max LongLong"), "{long}");
    assert_eq!(long.chars().count(), 64);
    assert!(long.ends_with('…'));
    assert_eq!(shown[5]["username"], "max");

    // Page by 200 to Telegram's cap: each member once, then no cursor.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut args = json!({ "chat_id": big, "limit": 200 });
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let page = members(&client, args).await.unwrap();
        pages += 1;
        assert_eq!(page["total"], BIG_MEMBERS, "Telegram's count, past its cap");
        for m in page["members"].as_array().unwrap() {
            seen.push(m["id"].as_str().unwrap().to_string());
        }
        match page.get("next_cursor") {
            Some(next) => cursor = Some(next.as_str().unwrap().to_string()),
            None => break,
        }
        assert!(pages < 10, "the listing ends");
    }
    assert_eq!(seen.len(), BIG_CAP, "only as many as Telegram lists");
    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "no member twice");
    assert_eq!(pages, 3, "two full pages, then an empty one at the cap");
}

#[tokio::test]
async fn a_query_is_telegrams_member_search_in_a_supergroup() {
    let (_server, client) = client().await;
    let big = fake_issued_id(BIG);
    let found = members(&client, json!({ "chat_id": big, "query": "ann" }))
        .await
        .unwrap();
    // The beginning of a word or of the @username, as Telegram matches.
    assert_eq!(names(&found), ["Anna Smith", "Joanna Ivanova"]);
    assert_eq!(found["total"], 2);
    assert_eq!(found.get("next_cursor"), None);

    let by_username = members(&client, json!({ "chat_id": big, "query": "@olga_" }))
        .await
        .unwrap();
    assert_eq!(names(&by_username), ["Olga Petrova"]);

    // A query pages like the plain list, and its cursor is its own.
    let page = members(
        &client,
        json!({ "chat_id": big, "query": "user", "limit": 100 }),
    )
    .await
    .unwrap();
    assert_eq!(page["members"].as_array().unwrap().len(), 100);
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = members(
        &client,
        json!({ "chat_id": big, "query": "user", "limit": 100, "cursor": cursor }),
    )
    .await
    .unwrap();
    assert_eq!(next["members"][0]["name"], "User 100");
    let message = invalid_params(
        members(
            &client,
            json!({ "chat_id": big, "query": "olga", "cursor": cursor }),
        )
        .await,
        "a cursor of another query",
    );
    assert!(message.contains("different listing"), "{message}");
}

#[tokio::test]
async fn admins_only_lists_the_creator_and_admins_and_takes_a_query() {
    let (_server, client) = client().await;
    let big = fake_issued_id(BIG);
    let admins = members(&client, json!({ "chat_id": big, "admins_only": true }))
        .await
        .unwrap();
    let roles: Vec<&str> = admins["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["creator", "admin", "admin"]);
    assert_eq!(admins["total"], 3);

    // Telegram's admins filter has no query: it is matched on our side, any
    // part of the name or @username.
    let anna = members(
        &client,
        json!({ "chat_id": big, "admins_only": true, "query": "SMI" }),
    )
    .await
    .unwrap();
    assert_eq!(names(&anna), ["Anna Smith"]);

    // Paged on our side too.
    let first = members(
        &client,
        json!({ "chat_id": big, "admins_only": true, "limit": 2 }),
    )
    .await
    .unwrap();
    assert_eq!(first["members"].as_array().unwrap().len(), 2);
    let second = members(
        &client,
        json!({
            "chat_id": big,
            "admins_only": true,
            "limit": 2,
            "cursor": first["next_cursor"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(second["members"].as_array().unwrap().len(), 1);
    assert_eq!(second.get("next_cursor"), None);
}

#[tokio::test]
async fn a_basic_group_is_listed_filtered_and_paged_here() {
    let (_server, client) = client().await;
    let all = members(&client, json!({ "chat_id": "chat:404" }))
        .await
        .unwrap();
    assert_eq!(
        names(&all),
        ["Mom", "Dad", "Me", "Sister Ann", "Family Bot"]
    );
    assert_eq!(all["total"], 5);
    assert_eq!(all["members"][4]["bot"], true);
    assert_eq!(all["members"][2]["is_self"], true);

    let first = members(&client, json!({ "chat_id": "chat:404", "limit": 2 }))
        .await
        .unwrap();
    assert_eq!(names(&first), ["Mom", "Dad"]);
    let second = members(
        &client,
        json!({ "chat_id": "chat:404", "limit": 2, "cursor": first["next_cursor"] }),
    )
    .await
    .unwrap();
    assert_eq!(names(&second), ["Me", "Sister Ann"]);

    let ann = members(&client, json!({ "chat_id": "chat:404", "query": "ann" }))
        .await
        .unwrap();
    assert_eq!(names(&ann), ["Sister Ann"]);
    let admins = members(
        &client,
        json!({ "chat_id": "chat:404", "admins_only": true }),
    )
    .await
    .unwrap();
    assert_eq!(names(&admins), ["Mom", "Dad"]);

    // Member ids work with the other tools.
    let dad = all["members"][1]["id"].as_str().unwrap();
    assert_eq!(dad, fake_issued_id(ChatId::User(12)));
    let info = call(&client, "get_chat_info", json!({ "chat_id": dad }))
        .await
        .unwrap();
    assert_eq!(info["title"], "Dad");
    assert_eq!(info["username"], "dad");

    let message = invalid_params(
        members(&client, json!({ "chat_id": "chat:405" })).await,
        "a group the account left",
    );
    assert!(message.contains("no longer in this group"), "{message}");
}

#[tokio::test]
async fn a_channel_that_shows_members_only_to_admins_is_a_clear_error() {
    let (_server, client) = client().await;
    let news = fake_issued_id(NEWS);
    let message = invalid_params(
        members(&client, json!({ "chat_id": news })).await,
        "a broadcast channel",
    );
    assert!(
        message.contains("only to its admins") && message.contains("admins_only"),
        "{message}"
    );
    let message = invalid_params(
        members(&client, json!({ "chat_id": news, "admins_only": true })).await,
        "a broadcast channel's admins",
    );
    assert!(
        message.contains("admins to this account either"),
        "{message}"
    );
}

#[tokio::test]
async fn hidden_members_are_said_to_be_hidden() {
    let (_server, client) = client().await;
    let secret = fake_issued_id(SECRET);
    let page = members(&client, json!({ "chat_id": secret }))
        .await
        .unwrap();
    assert_eq!(page["members_hidden"], true);
    assert_eq!(
        names(&page),
        ["Boss", "Guard", "Mod"],
        "only admins and bots"
    );
    let admins = members(&client, json!({ "chat_id": secret, "admins_only": true }))
        .await
        .unwrap();
    assert_eq!(names(&admins), ["Boss", "Mod"]);
}

#[tokio::test]
async fn bounds_and_ids_are_checked() {
    let (_server, client) = client().await;
    let big = fake_issued_id(BIG);
    for (case, args) in [
        ("limit 0", json!({ "chat_id": big, "limit": 0 })),
        ("limit 201", json!({ "chat_id": big, "limit": 201 })),
        (
            "a long query",
            json!({ "chat_id": big, "query": "q".repeat(65) }),
        ),
        (
            "a garbage cursor",
            json!({ "chat_id": big, "cursor": "not a cursor" }),
        ),
        (
            "a long cursor",
            json!({ "chat_id": big, "cursor": "A".repeat(129) }),
        ),
        (
            "a list_chats cursor",
            json!({ "chat_id": big, "cursor": "fake:30" }),
        ),
        (
            "a user",
            json!({ "chat_id": fake_issued_id(ChatId::User(101)) }),
        ),
        ("a hashless user", json!({ "chat_id": "user:101" })),
        ("not an id", json!({ "chat_id": "big" })),
        (
            "a wrong hash",
            json!({ "chat_id": "channel:300~AAAAAAAAAAA" }),
        ),
        ("an unknown chat", json!({ "chat_id": "chat:999" })),
    ] {
        invalid_params(members(&client, args).await, case);
    }
    let message = invalid_params(
        members(&client, json!({ "chat_id": "user:101" })).await,
        "a user",
    );
    assert!(message.contains("is a user"), "{message}");

    // A query of 64 characters is fine, and so is an empty one (no filter).
    members(&client, json!({ "chat_id": big, "query": "q".repeat(64) }))
        .await
        .unwrap();
    let all = members(
        &client,
        json!({ "chat_id": big, "query": "  ", "limit": 200 }),
    )
    .await
    .unwrap();
    assert_eq!(all["members"].as_array().unwrap().len(), 200);
    assert_eq!(all["total"], BIG_MEMBERS);
}

#[tokio::test]
async fn a_flood_wait_is_reported_with_its_seconds() {
    let mut data =
        FakeData::new(account(ME, "Me")).with_chats(vec![chat(FAMILY, "Family", ChatKind::Group)]);
    data.flood_wait = Some(30);
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    match members(&client, json!({ "chat_id": "chat:404" })).await {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert!(err.message.contains("wait 30 s"), "{err:?}")
        }
        other => panic!("the flood was hidden: {other:?}"),
    }
}

#[tokio::test]
async fn tool_is_read_only_and_bounded() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "get_members")
        .expect("get_members listed");
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.contains("may be incomplete") && description.contains("10,000"),
        "{description}"
    );
    assert_eq!(tool.input_schema["required"], json!(["chat_id"]));
    let properties = &tool.input_schema["properties"];
    assert_eq!(properties["chat_id"]["maxLength"], 39);
    assert_eq!(properties["query"]["maxLength"], 64);
    assert_eq!(properties["cursor"]["maxLength"], 128);
    assert_eq!(properties["limit"]["maximum"], 200);
    assert_eq!(properties["admins_only"]["type"], "boolean");
    let output = tool.output_schema.as_ref().expect("an output schema");
    assert_eq!(output["properties"]["members"]["maxItems"], 200);
}

#[tokio::test]
async fn answers_match_the_declared_output_schema() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_members").unwrap();
    let schema = Value::Object((*tool.output_schema.clone().unwrap()).clone());
    let validator = jsonschema::validator_for(&schema).unwrap();
    for args in [
        json!({ "chat_id": fake_issued_id(BIG), "limit": 200 }),
        json!({ "chat_id": fake_issued_id(BIG), "admins_only": true }),
        json!({ "chat_id": fake_issued_id(SECRET) }),
        json!({ "chat_id": "chat:404", "query": "ann" }),
    ] {
        let answer = members(&client, args).await.unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&answer)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{answer}: {errors:?}");
    }
}
