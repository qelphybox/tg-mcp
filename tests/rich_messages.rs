//! Rich messages (#50) through `/mcp`: a post whose content is only page
//! blocks reads as its flattened text in get_messages, in the list_chats
//! preview and in search snippets, sanitised and capped like any other text.

mod common;

use common::start_with;
use grammers_client::tl;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use tg_mcp::tg::{
    Account,
    chats::{ChatId, ChatKind, ChatSummary, MessagePreview},
    fake::{FakeData, fake_issued_id, rich_listing},
    messages::RawHistoryMessage,
    rich::message_text,
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ESTATE: ChatId = ChatId::Channel(1804);

/// What the listing reads as: its lines, the ten photos on one line, the
/// spacer line and the hidden characters in the "Кондиционеры" cell gone.
const LISTING: &str = "[10 photos]\n\
                       🏡 Аренда: 4 комнаты, Ваке\n\
                       📍 Тбилиси, ул. Абашидзе, 12\n\
                       Сдается светлая, чистая квартира с видом на парк.\n\
                       Параметры и удобства\n\
                       Комнаты | 4\n\
                       Кондиционеры | 1\n\
                       Площадь | 120 м²\n\
                       ✅ Можно с животными\n\
                       Теги\n\
                       ID: 19752\n\
                       #аренда #ваке";

/// A listing with a 300-row price table and, after it, a pool.
fn long_listing() -> tl::enums::RichMessage {
    use tl::{enums::PageBlock as B, enums::RichText as T, types};
    let plain = |text: String| T::TextPlain(types::TextPlain { text });
    let cell = |text: String| {
        tl::enums::PageTableCell::Cell(types::PageTableCell {
            header: false,
            align_center: false,
            align_right: false,
            valign_middle: false,
            valign_bottom: false,
            text: Some(plain(text)),
            colspan: None,
            rowspan: None,
        })
    };
    let rows = (1..=300)
        .map(|n| {
            tl::enums::PageTableRow::Row(types::PageTableRow {
                cells: vec![cell(format!("Квартира {n}")), cell(format!("{n}00 $"))],
            })
        })
        .collect();
    tl::enums::RichMessage::Message(types::RichMessage {
        rtl: false,
        part: false,
        blocks: vec![
            B::Table(types::PageBlockTable {
                bordered: true,
                striped: false,
                compact: false,
                title: T::TextEmpty,
                rows,
            }),
            B::Paragraph(types::PageBlockParagraph {
                text: plain("Во дворе бассейн и парковка.".into()),
            }),
        ],
        photos: Vec::new(),
        documents: Vec::new(),
    })
}

/// A post as the Live client reads it: `message.message` empty, the content
/// in its Rich message.
fn post(id: i32, rich: &tl::enums::RichMessage) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: 1_793_923_200 + id,
        from: None,
        out: false,
        text: message_text(String::new(), Some(rich)),
        media: None,
        reply_to: None,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
        topic: None,
    }
}

async fn client() -> (common::TestServer, Client) {
    let data = FakeData::new(Account {
        id: 42,
        first_name: "Me".into(),
        last_name: None,
        username: None,
    })
    .with_chats(vec![ChatSummary {
        id: ESTATE.to_string(),
        title: "Аренда Тбилиси".into(),
        kind: ChatKind::Channel,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: Some(MessagePreview {
            date: "2026-11-06T00:00:02Z".into(),
            text: message_text(String::new(), Some(&rich_listing())),
            media: None,
        }),
    }])
    .with_messages(
        ESTATE,
        vec![post(1, &long_listing()), post(2, &rich_listing())],
    );
    let server = start_with(vec![data]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn call(client: &Client, tool: &'static str, arguments: Value) -> Value {
    client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
        .structured_content
        .expect("structured output")
}

#[tokio::test]
async fn get_messages_shows_a_rich_post_as_its_lines_sanitised_and_capped() {
    let (_server, client) = client().await;
    let page = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(ESTATE) }),
    )
    .await;
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(messages[0]["id"], 2);
    assert_eq!(messages[0]["text"], LISTING);

    let long = messages[1]["text"].as_str().unwrap();
    assert!(long.starts_with("Квартира 1 | 100 $\nКвартира 2 | 200 $\n"));
    assert_eq!(long.chars().count(), 1000, "get_messages' cap applies");
    assert!(long.ends_with('…'));
}

#[tokio::test]
async fn list_chats_previews_a_rich_post_on_one_line() {
    let (_server, client) = client().await;
    let page = call(&client, "list_chats", json!({})).await;
    let preview = page["chats"][0]["last_message"]["text"].as_str().unwrap();
    let one_line = LISTING.replace('\n', " ");
    let expected: String = one_line.chars().take(99).chain(['…']).collect();
    assert_eq!(preview, expected);
}

#[tokio::test]
async fn search_snippets_come_from_the_flattened_text() {
    let (_server, client) = client().await;
    let page = call(
        &client,
        "search_messages",
        json!({ "query": "кондиционеры" }),
    )
    .await;
    let hits = page["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], 2);
    assert_eq!(hits[0]["snippet"], LISTING.replace('\n', " "));

    // Deep in a long post, the snippet is centred on the match.
    let page = call(&client, "search_messages", json!({ "query": "бассейн" })).await;
    let snippet = page["hits"][0]["snippet"].as_str().unwrap();
    assert!(snippet.starts_with('…'), "{snippet}");
    assert!(
        snippet.contains("Во дворе бассейн и парковка."),
        "{snippet}"
    );
}
