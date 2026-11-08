//! Chat folders through `/mcp`: whoami lists them, list_chats lists the Chats
//! of an explicit one (#29).

mod common;

use common::{account, start_with};
use rmcp::model::{CallToolRequestParams, ErrorCode};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary, MessagePreview},
    fake::{FakeChatFolder, FakeData, FolderRpc, fake_issued_id},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

/// Chat `user:n`, its last message `n` minutes past noon: a higher `n` is
/// more recent.
fn chat(n: i64) -> ChatSummary {
    ChatSummary {
        id: format!("user:{n}"),
        title: format!("Friend {n}"),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: Some(MessagePreview {
            date: format!("2026-11-06T{:02}:{:02}:00Z", 12 + n / 60, n % 60),
            text: format!("hello {n}"),
            media: None,
        }),
    }
}

fn users(ns: &[i64]) -> Vec<ChatId> {
    ns.iter().map(|&n| ChatId::User(n)).collect()
}

fn issued(ns: &[i64]) -> Vec<String> {
    ns.iter()
        .map(|&n| fake_issued_id(ChatId::User(n)))
        .collect()
}

/// An explicit Chat folder: `pinned` and `included` minus `excluded`.
fn explicit(
    id: i32,
    title: &str,
    pinned: &[i64],
    included: &[i64],
    excluded: &[i64],
) -> FakeChatFolder {
    FakeChatFolder {
        id,
        title: title.into(),
        emoji: None,
        rules: false,
        pinned: users(pinned),
        included: users(included),
        excluded: users(excluded),
    }
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

fn listed(page: &Value) -> Vec<String> {
    page["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

async fn client_for(data: FakeData) -> (common::TestServer, Client) {
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

#[tokio::test]
async fn whoami_lists_the_chat_folders_in_the_users_order_and_says_which_use_rules() {
    let mut work = explicit(4, "Work", &[1], &[2, 3], &[]);
    work.emoji = Some("💼".into());
    let mut groups = explicit(2, "Groups", &[], &[], &[]);
    groups.rules = true;
    let data = FakeData::new(account(42, "Alice"))
        .with_chats((1..=3).map(chat).collect())
        .with_chat_folders(vec![work, groups]);
    let (_server, client) = client_for(data).await;

    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["id"], 42);
    assert_eq!(me["first_name"], "Alice");
    assert_eq!(
        me["chat_folders"],
        json!([
            { "id": 4, "title": "Work", "emoji": "💼", "rules": false },
            { "id": 2, "title": "Groups", "rules": true },
        ])
    );
}

#[tokio::test]
async fn whoami_without_chat_folders_lists_none() {
    let (_server, client) = client_for(FakeData::new(account(42, "Alice"))).await;
    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["chat_folders"], json!([]));
}

#[tokio::test]
async fn whoami_still_answers_when_telegram_fails_to_list_the_folders() {
    let mut data = FakeData::new(account(42, "Alice"));
    data.chat_folders_fail = true;
    let (_server, client) = client_for(data).await;

    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["id"], 42);
    assert_eq!(me.get("chat_folders"), None);

    // A flood is the caller's to know: whoami says so instead.
    let mut data = FakeData::new(account(43, "Bob"));
    data.flood_wait = Some(30);
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    match call(&client, "whoami", json!({})).await {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert!(err.message.contains("FLOOD_WAIT"), "{err:?}")
        }
        other => panic!("a flood was swallowed: {other:?}"),
    }
}

#[tokio::test]
async fn folder_titles_are_sanitised_and_bounded() {
    let mut hidden = explicit(3, "Wo\u{202E}rk\u{200B}", &[], &[], &[]);
    hidden.emoji = Some("\u{2063}".into());
    let long = explicit(5, &"F".repeat(500), &[], &[], &[]);
    let data = FakeData::new(account(42, "Alice")).with_chat_folders(vec![hidden, long]);
    let (_server, client) = client_for(data).await;

    let me = call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(me["chat_folders"][0]["title"], "Work");
    assert_eq!(me["chat_folders"][0].get("emoji"), None, "nothing visible");
    let long = me["chat_folders"][1]["title"].as_str().unwrap();
    assert_eq!(long.chars().count(), 64, "{long}");
}

/// Chats 1-8 (8 the most recent); 8 is pinned in the main list and 7 is
/// archived. Folder 5 "Work" pins 3 then 1, includes 2, 5, 6, 7, 8 and 99
/// (a chat the account left: Telegram has no dialog for it) and excludes 6.
fn work_account() -> FakeData {
    let mut chats: Vec<ChatSummary> = (1..=8).map(chat).collect();
    chats[7].pinned = true;
    FakeData::new(account(42, "Alice"))
        .with_chats(chats)
        .with_archived(users(&[7]))
        .with_chat_folders(vec![explicit(
            5,
            "Work",
            &[3, 1],
            &[2, 5, 6, 7, 8, 99],
            &[6],
        )])
}

#[tokio::test]
async fn an_explicit_folder_lists_exactly_its_chats_its_pinned_ones_first_then_the_most_recent() {
    let (_server, client) = client_for(work_account()).await;

    let page = call(&client, "list_chats", json!({ "chat_folder": 5 }))
        .await
        .unwrap();
    // Excluded 6 and the left 99 are absent; archived 7 is in the folder.
    assert_eq!(listed(&page), issued(&[3, 1, 8, 7, 5, 2]));
    assert_eq!(page.get("next_cursor"), None);
    let pinned: Vec<Option<&Value>> = page["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.get("pinned"))
        .collect();
    // `pinned` is the folder's pin: 8, pinned in the main list, isn't here.
    assert_eq!(
        pinned,
        [
            Some(&json!(true)),
            Some(&json!(true)),
            None,
            None,
            None,
            None
        ]
    );
    assert_eq!(page["chats"][0]["title"], "Friend 3");
    assert_eq!(page["chats"][0]["last_message"]["text"], "hello 3");
}

#[tokio::test]
async fn an_explicit_folder_is_paged_with_its_cursor_each_chat_once() {
    let (_server, client) = client_for(work_account()).await;

    for limit in [1, 2, 4] {
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let mut request = json!({ "chat_folder": 5, "limit": limit });
            if let Some(c) = &cursor {
                request["cursor"] = json!(c);
            }
            let page = call(&client, "list_chats", request).await.unwrap();
            let chats = listed(&page);
            assert!(chats.len() <= limit, "limit {limit}: {chats:?}");
            seen.extend(chats);
            match page.get("next_cursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        assert_eq!(seen, issued(&[3, 1, 8, 7, 5, 2]), "limit {limit}");
    }
}

#[tokio::test]
async fn unread_only_lists_the_folders_unread_and_marked_chats_with_their_flags() {
    let mut data = work_account();
    for chat in &mut data.chats {
        match chat.id.as_str() {
            "user:1" => chat.unread_count = 4,
            "user:5" => chat.marked_unread = true,
            "user:6" => chat.unread_count = 9, // excluded from the folder
            "user:7" => {
                chat.unread_count = 2;
                chat.muted = true;
            }
            _ => {}
        }
    }
    let (_server, client) = client_for(data).await;

    let page = call(
        &client,
        "list_chats",
        json!({ "chat_folder": 5, "unread_only": true }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&page), issued(&[1, 7, 5]));
    assert_eq!(page["chats"][0]["unread_count"], 4);
    assert_eq!(page["chats"][1]["muted"], true);
    assert_eq!(page["chats"][2]["marked_unread"], true);
    assert_eq!(page.get("unread_scan_truncated"), None);

    let first = call(
        &client,
        "list_chats",
        json!({ "chat_folder": 5, "unread_only": true, "limit": 2 }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&first), issued(&[1, 7]));
    let rest = call(
        &client,
        "list_chats",
        json!({ "chat_folder": 5, "unread_only": true, "cursor": first["next_cursor"] }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&rest), issued(&[5]));
    assert_eq!(rest.get("next_cursor"), None);
}

async fn invalid_params(client: &Client, arguments: Value) -> String {
    match call(client, "list_chats", arguments.clone()).await {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{arguments}: {err:?}");
            err.message.to_string()
        }
        other => panic!("{arguments} was accepted: {other:?}"),
    }
}

#[tokio::test]
async fn a_folder_defined_by_rules_is_a_clear_error_pointing_to_the_whole_list() {
    let mut data = work_account();
    let mut unread = explicit(7, "Unread", &[1], &[2], &[]);
    unread.rules = true;
    data.chat_folders.push(unread);
    let (_server, client) = client_for(data).await;

    let message = invalid_params(&client, json!({ "chat_folder": 7 })).await;
    assert!(
        message.contains("this Chat folder is defined by rules, not supported yet"),
        "{message}"
    );
    assert!(
        message.contains("list_chats without chat_folder"),
        "{message}"
    );
}

#[tokio::test]
async fn an_unknown_folder_and_a_folder_in_the_archive_are_invalid_params() {
    let (_server, client) = client_for(work_account()).await;

    let unknown = invalid_params(&client, json!({ "chat_folder": 9 })).await;
    assert!(unknown.contains("whoami"), "{unknown}");
    let archived = invalid_params(&client, json!({ "chat_folder": 5, "archived": true })).await;
    assert!(archived.contains("archived"), "{archived}");
    // `archived: false` is the default, no conflict.
    let page = call(
        &client,
        "list_chats",
        json!({ "chat_folder": 5, "archived": false, "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&page), issued(&[3]));
}

#[tokio::test]
async fn a_cursor_continues_only_the_folder_listing_it_came_from() {
    let mut data = work_account();
    data.chat_folders
        .push(explicit(6, "Friends", &[], &[1, 2, 3], &[]));
    data.chats[1].unread_count = 1;
    let (_server, client) = client_for(data).await;

    let next = |page: Value| page["next_cursor"].as_str().unwrap().to_string();
    let folder = next(
        call(
            &client,
            "list_chats",
            json!({ "chat_folder": 5, "limit": 1 }),
        )
        .await
        .unwrap(),
    );
    let plain = next(
        call(&client, "list_chats", json!({ "limit": 1 }))
            .await
            .unwrap(),
    );

    for bad in [
        json!({ "cursor": folder }),
        json!({ "cursor": folder, "chat_folder": 6 }),
        json!({ "cursor": plain, "chat_folder": 5 }),
    ] {
        let message = invalid_params(&client, bad.clone()).await;
        assert!(message.contains("chat_folder"), "{bad}: {message}");
    }
    let message = invalid_params(
        &client,
        json!({ "cursor": folder, "chat_folder": 5, "unread_only": true }),
    )
    .await;
    assert!(message.contains("unread_only"), "{message}");

    let rest = call(
        &client,
        "list_chats",
        json!({ "cursor": folder, "chat_folder": 5, "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(listed(&rest), issued(&[1]));
}

#[tokio::test]
async fn chat_folder_is_a_bounded_integer_in_the_schema() {
    let (_server, client) = client_for(FakeData::new(account(42, "Alice"))).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "list_chats").unwrap();
    let chat_folder = &tool.input_schema["properties"]["chat_folder"];
    assert!(
        chat_folder["type"].to_string().contains("integer"),
        "{chat_folder}"
    );
    assert_eq!(chat_folder["minimum"], 2, "{chat_folder}");
    assert_eq!(chat_folder["maximum"], 255, "{chat_folder}");
    let whoami = tools.iter().find(|t| t.name == "whoami").unwrap();
    assert_eq!(
        whoami.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    let folders = &whoami.output_schema.as_ref().unwrap()["properties"]["chat_folders"];
    assert_eq!(folders["maxItems"], 50, "{folders}");
}

#[tokio::test]
async fn paging_a_folder_reads_it_once_then_only_each_pages_chats() {
    // 40 chats: 2 pinned in the folder, 38 more; a limit=3 walk is 14 pages.
    let data = FakeData::new(account(42, "Alice"))
        .with_chats((1..=40).map(chat).collect())
        .with_chat_folders(vec![explicit(
            4,
            "Flats",
            &[1, 2],
            &(3..=40).collect::<Vec<_>>(),
            &[],
        )]);
    let (server, client) = client_for(data).await;
    server.connector.take_folder_rpcs();

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = Vec::new();
    loop {
        let mut request = json!({ "chat_folder": 4, "limit": 3 });
        if let Some(c) = &cursor {
            request["cursor"] = json!(c);
        }
        let page = call(&client, "list_chats", request).await.unwrap();
        let chats = listed(&page);
        pages.push((chats.len(), server.connector.take_folder_rpcs()));
        seen.extend(chats);
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
        assert!(pages.len() < 20, "the walk never ends");
    }
    let order: Vec<i64> = [1, 2].into_iter().chain((3..=40).rev()).collect();
    assert_eq!(seen, issued(&order));
    assert_eq!(pages.len(), 14);
    // Cold: the folder, then its 40 chats in one getPeerDialogs.
    assert_eq!(
        pages[0].1,
        [FolderRpc::DialogFilters, FolderRpc::PeerDialogs(40)]
    );
    // Then each page: one getPeerDialogs of its own chats only.
    for (chats, rpcs) in &pages[1..] {
        assert_eq!(*rpcs, [FolderRpc::PeerDialogs(*chats)]);
    }

    // whoami's getDialogFilters spares the next first page its own.
    call(&client, "whoami", json!({})).await.unwrap();
    assert_eq!(
        server.connector.take_folder_rpcs(),
        [FolderRpc::DialogFilters]
    );
    call(
        &client,
        "list_chats",
        json!({ "chat_folder": 4, "limit": 3 }),
    )
    .await
    .unwrap();
    assert_eq!(
        server.connector.take_folder_rpcs(),
        [FolderRpc::PeerDialogs(40)]
    );
}
