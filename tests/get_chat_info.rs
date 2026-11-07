//! `get_chat_info` through `/mcp` with rmcp's own client.

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{Birthday, ChatId, ChatKind, ChatSummary, RawProfile, RawUserStatus},
    fake::{FakeData, FakeDetails, FakeProfile, fake_issued_id},
};

const BOB: ChatId = ChatId::User(102);
const HELPER: ChatId = ChatId::User(103);
const SHY: ChatId = ChatId::User(104);
/// Groups Bob shares with the account, past what get_chat_info shows.
const BOB_GROUPS: i64 = 25;

fn summary(id: ChatId, title: &str, kind: ChatKind, username: Option<&str>) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: username.map(str::to_string),
        is_self: false,
        unread_count: 0,
        pinned: false,
        last_message: None,
    }
}

async fn client() -> (
    common::TestServer,
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    let rust = ChatId::Channel(202);
    let alice = ChatId::User(101);
    let family = ChatId::Chat(404);
    let groups = (1..=BOB_GROUPS).map(|n| ChatId::Chat(1000 + n));
    let data = FakeData::new(account(42, "Me"))
        .with_chats(
            [
                summary(rust, "Rust Weekly", ChatKind::Channel, Some("rustweekly")),
                summary(alice, "Alice", ChatKind::Private, None),
                summary(family, "Family", ChatKind::Group, None),
                summary(BOB, "Bob", ChatKind::Private, Some("bob")),
                summary(HELPER, "Helper", ChatKind::Bot, Some("helper_bot")),
                summary(SHY, "Shy", ChatKind::Private, None),
            ]
            .into_iter()
            .chain(
                groups
                    .clone()
                    .map(|g| summary(g, &format!("{g}"), ChatKind::Group, None)),
            )
            .collect(),
        )
        .with_profile(
            BOB,
            FakeProfile {
                profile: RawProfile {
                    bio: Some("Rustacean.\nLikes \u{200B}tea.".into()),
                    status: Some(RawUserStatus::Offline {
                        was_online: 1_700_000_000,
                    }),
                    verified: true,
                    premium: true,
                    phone: Some("79991234567".into()),
                    birthday: Some(Birthday {
                        day: 17,
                        month: 5,
                        year: Some(1990),
                    }),
                    common_chats_count: BOB_GROUPS as i32,
                    ..RawProfile::default()
                },
                // The channel first: Telegram's order is kept.
                common_chats: std::iter::once(rust).chain(groups).collect(),
            },
        )
        .with_profile(
            HELPER,
            FakeProfile {
                profile: RawProfile {
                    bot: true,
                    bio: Some("I help.".into()),
                    bot_description: Some(format!(
                        "What can this bot do?\nEverything.{}",
                        "z".repeat(2000)
                    )),
                    common_chats_count: 1,
                    ..RawProfile::default()
                },
                common_chats: vec![family],
            },
        )
        .with_profile(
            SHY,
            FakeProfile {
                // Phone, bio and birthday hidden, last seen approximate.
                profile: RawProfile {
                    status: Some(RawUserStatus::Recently),
                    scam: true,
                    fake: true,
                    ..RawProfile::default()
                },
                common_chats: Vec::new(),
            },
        )
        .with_details(
            rust,
            FakeDetails {
                description: Some(format!(
                    "News about Rust.\nEvery week.\n{}",
                    "z".repeat(3000)
                )),
                member_count: Some(5200),
            },
        )
        .with_profile(
            alice,
            FakeProfile {
                profile: RawProfile {
                    bio: Some("coffee & code".into()),
                    ..RawProfile::default()
                },
                common_chats: Vec::new(),
            },
        )
        .with_details(
            family,
            FakeDetails {
                description: Some("   ".into()),
                member_count: Some(5),
            },
        )
        .with_profile(
            ChatId::User(42),
            FakeProfile {
                profile: RawProfile {
                    bio: Some("my own bio".into()),
                    status: Some(RawUserStatus::Online { expires: i32::MAX }),
                    ..RawProfile::default()
                },
                common_chats: Vec::new(),
            },
        );
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn info(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    chat_id: &str,
) -> Result<Value, rmcp::ServiceError> {
    call(client, json!({ "chat_id": chat_id })).await
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
    arguments: Value,
) -> Result<Value, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("get_chat_info")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map(|r| r.structured_content.expect("structured output"))
}

fn assert_invalid_params(result: Result<Value, rmcp::ServiceError>, case: &str) {
    match result {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{case}")
        }
        other => panic!("{case} was accepted: {other:?}"),
    }
}

#[tokio::test]
async fn describes_channels_people_and_groups() {
    let (_server, client) = client().await;

    let channel = info(&client, "channel:202~18YMXnDF6JI").await.unwrap();
    assert_eq!(channel["id"], "channel:202~18YMXnDF6JI");
    assert_eq!(channel["title"], "Rust Weekly");
    assert_eq!(channel["type"], "channel");
    assert_eq!(channel["username"], "rustweekly");
    assert_eq!(channel["member_count"], 5200);
    let description = channel["description"].as_str().unwrap();
    assert!(
        description.starts_with("News about Rust.\nEvery week.\n"),
        "line breaks are kept"
    );
    assert_eq!(description.chars().count(), 1000);

    let alice = fake_issued_id(ChatId::User(101));
    assert_eq!(
        info(&client, &alice).await.unwrap(),
        json!({
            "id": alice,
            "title": "Alice",
            "type": "private",
            "bio": "coffee & code",
            "common_chats_count": 0,
        }),
        "people have a bio but no member count"
    );
    assert_eq!(
        info(&client, "chat:404").await.unwrap(),
        json!({ "id": "chat:404", "title": "Family", "type": "group", "member_count": 5 }),
        "a blank description is left out"
    );
    // The account itself ("Saved Messages") is marked, with its own bio.
    let me = info(&client, &fake_issued_id(ChatId::User(42)))
        .await
        .unwrap();
    assert_eq!(me["title"], "Me");
    assert_eq!(me["is_self"], true);
    assert_eq!(me["bio"], "my own bio");
    assert_eq!(me.get("status"), None, "its own status is noise");
    assert_eq!(
        channel.get("is_self"),
        None,
        "only the account itself is marked"
    );
}

#[tokio::test]
async fn ids_without_a_hash_still_work_and_come_back_with_it() {
    let (_server, client) = client().await;
    let alice = info(&client, "user:101").await.unwrap();
    assert_eq!(alice["title"], "Alice");
    assert_eq!(alice["id"], fake_issued_id(ChatId::User(101)));
    assert_eq!(
        info(&client, "channel:202").await.unwrap()["title"],
        "Rust Weekly"
    );
}

#[tokio::test]
async fn bad_or_unknown_ids_are_invalid_params() {
    let (_server, client) = client().await;
    for bad in [
        "rustweekly",
        "channel:-5",
        "channel:999",
        // A tampered access hash: Telegram refuses it, the tool says so.
        "channel:202~AAAAAAAAAAA",
        "user:101~18YMXnDF6JI",
        // Basic groups have no access hash.
        "chat:404~18YMXnDF6JI",
        "channel:202~not-a-hash",
    ] {
        match info(&client, bad).await {
            Err(rmcp::ServiceError::McpError(err)) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{bad}")
            }
            other => panic!("{bad} was accepted: {other:?}"),
        }
    }
}

#[tokio::test]
async fn tool_is_read_only_and_requires_a_chat() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "get_chat_info")
        .expect("get_chat_info listed");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tool.output_schema.is_some());
    assert_eq!(tool.input_schema["required"], json!(["chat_id"]));
    assert_eq!(
        tool.input_schema["properties"]["common_chats"]["type"],
        "boolean"
    );
    let output = tool.output_schema.as_ref().unwrap();
    assert_eq!(output["properties"]["common_chats"]["maxItems"], 20);
    let chat_id = &tool.input_schema["properties"]["chat_id"];
    assert_eq!(chat_id["maxLength"], 39, "{chat_id}");
    let pattern = chat_id["pattern"].as_str().expect("chat_id has a pattern");
    assert!(pattern.contains("~"), "{pattern}");
}

#[tokio::test]
async fn a_user_has_the_profile_telegram_shows() {
    let (_server, client) = client().await;
    let bob = fake_issued_id(BOB);
    assert_eq!(
        info(&client, &bob).await.unwrap(),
        json!({
            "id": bob,
            "title": "Bob",
            "type": "private",
            "username": "bob",
            // Sanitised, line breaks kept.
            "bio": "Rustacean.\nLikes tea.",
            "status": "last_seen",
            "last_seen_at": "2023-11-14T22:13:20Z",
            "verified": true,
            "premium": true,
            "phone": "+79991234567",
            "birthday": { "day": 17, "month": 5, "year": 1990 },
            "common_chats_count": BOB_GROUPS,
        })
    );
}

#[tokio::test]
async fn a_bot_has_its_description() {
    let (_server, client) = client().await;
    let helper = info(&client, &fake_issued_id(HELPER)).await.unwrap();
    assert_eq!(helper["type"], "bot");
    assert_eq!(helper["bot"], true);
    assert_eq!(
        helper["bio"], "I help.",
        "an About unlike the description is kept"
    );
    let description = helper["bot_description"].as_str().unwrap();
    assert!(description.starts_with("What can this bot do?\nEverything."));
    assert_eq!(description.chars().count(), 1000, "cut like a description");
    assert_eq!(helper.get("status"), None, "bots have no last seen");
    assert_eq!(helper["common_chats_count"], 1);
}

#[tokio::test]
async fn what_a_user_hides_is_left_out() {
    let (_server, client) = client().await;
    let shy = fake_issued_id(SHY);
    assert_eq!(
        info(&client, &shy).await.unwrap(),
        json!({
            "id": shy,
            "title": "Shy",
            "type": "private",
            "status": "recently",
            "scam": true,
            "fake": true,
            "common_chats_count": 0,
        }),
        "no phone, bio or birthday, and no false flag"
    );
}

#[tokio::test]
async fn common_chats_are_listed_on_request() {
    let (_server, client) = client().await;
    let bob = fake_issued_id(BOB);
    let plain = info(&client, &bob).await.unwrap();
    assert_eq!(plain.get("common_chats"), None, "only when asked for");

    let asked = call(&client, json!({ "chat_id": bob, "common_chats": true }))
        .await
        .unwrap();
    assert_eq!(asked["common_chats_count"], BOB_GROUPS);
    let chats = asked["common_chats"].as_array().unwrap();
    assert_eq!(chats.len(), 20, "at most 20, the count says how many");
    assert_eq!(
        chats[0],
        json!({
            "id": "channel:202~18YMXnDF6JI",
            "title": "Rust Weekly",
            "type": "channel",
            "username": "rustweekly",
        }),
        "in the list_chats shape, Telegram's order"
    );
    assert_eq!(
        chats[1],
        json!({ "id": "chat:1001", "title": "chat:1001", "type": "group" })
    );

    // The ids work with the other tools.
    for chat in chats {
        let id = chat["id"].as_str().unwrap();
        assert_eq!(info(&client, id).await.unwrap()["id"], id);
    }

    // A hashless id works too; no common chats is an empty list.
    let bot = call(
        &client,
        json!({ "chat_id": "user:103", "common_chats": true }),
    )
    .await
    .unwrap();
    assert_eq!(
        bot["common_chats"],
        json!([{ "id": "chat:404", "title": "Family", "type": "group" }])
    );
    let shy = call(
        &client,
        json!({ "chat_id": fake_issued_id(SHY), "common_chats": true }),
    )
    .await
    .unwrap();
    assert_eq!(shy["common_chats"], json!([]));
    let me = call(
        &client,
        json!({ "chat_id": fake_issued_id(ChatId::User(42)), "common_chats": true }),
    )
    .await
    .unwrap();
    assert_eq!(me["common_chats"], json!([]), "nothing shared with oneself");
}

#[tokio::test]
async fn common_chats_are_only_for_users() {
    let (_server, client) = client().await;
    for chat_id in ["chat:404", "channel:202~18YMXnDF6JI", "channel:202"] {
        assert_invalid_params(
            call(&client, json!({ "chat_id": chat_id, "common_chats": true })).await,
            chat_id,
        );
        // Not asked for: the group's details as always.
        let group = call(
            &client,
            json!({ "chat_id": chat_id, "common_chats": false }),
        )
        .await
        .unwrap();
        assert_eq!(group.get("common_chats"), None, "{chat_id}");
        assert_eq!(group.get("common_chats_count"), None, "{chat_id}");
    }
}

#[tokio::test]
async fn answers_match_the_declared_output_schema() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_chat_info").unwrap();
    let schema = Value::Object((*tool.output_schema.clone().unwrap()).clone());
    let validator = jsonschema::validator_for(&schema).unwrap();
    for chat in [BOB, HELPER, SHY, ChatId::User(42), ChatId::Channel(202)] {
        let common = matches!(chat, ChatId::User(_));
        let answer = call(
            &client,
            json!({ "chat_id": fake_issued_id(chat), "common_chats": common }),
        )
        .await
        .unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&answer)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{answer}: {errors:?}");
    }
}
