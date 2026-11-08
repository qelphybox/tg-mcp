//! Structured media on every message (#39), through `/mcp` with rmcp's own
//! client: `text` is only the text or caption, `media` says what else there is.

mod common;

use common::{account, start_with};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary, MessagePreview},
    fake::{FakeData, fake_issued_id},
    messages::{Media, MediaKind, MediaSummary, RawHistoryMessage},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
const NEWS: ChatId = ChatId::Channel(202);
/// 2026-11-06T00:00:00Z
const DAY: i32 = 1_793_923_200;

/// What the old output put in `text` for media.
const PLACEHOLDERS: [&str; 14] = [
    "[photo]",
    "[video]",
    "[video message]",
    "[voice]",
    "[audio]",
    "[gif]",
    "[sticker]",
    "[file]",
    "[location]",
    "[contact]",
    "[poll]",
    "[dice]",
    "[story]",
    "[media]",
];

fn chat(id: ChatId, title: &str, kind: ChatKind, preview: Option<MessagePreview>) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        last_message: preview,
    }
}

fn preview(text: &str, kind: Option<MediaKind>) -> Option<MessagePreview> {
    Some(MessagePreview {
        date: "2026-11-06T12:00:00Z".into(),
        text: text.into(),
        media: kind.map(|kind| MediaSummary { kind }),
    })
}

fn message(id: i32, text: &str, media: Option<Media>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: DAY + id * 60,
        from: None,
        out: false,
        text: text.into(),
        media,
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

fn file(kind: MediaKind, mime_type: &str, file_name: Option<&str>) -> Media {
    Media {
        size_bytes: Some(1_000),
        mime_type: Some(mime_type.into()),
        file_name: file_name.map(Into::into),
        ..Media::from(kind)
    }
}

/// Alice's chat: one message of every kind, newest first by id.
fn history() -> Vec<RawHistoryMessage> {
    vec![
        message(
            1,
            "Кухня",
            Some(Media {
                width: Some(1280),
                height: Some(960),
                size_bytes: Some(152_331),
                ..Media::from(MediaKind::Photo)
            }),
        ),
        message(
            2,
            "",
            Some(Media {
                duration_s: Some(7),
                size_bytes: Some(23_211),
                mime_type: Some("audio/ogg".into()),
                ..Media::from(MediaKind::Voice)
            }),
        ),
        message(
            3,
            "",
            Some(Media {
                duration_s: Some(9),
                width: Some(384),
                height: Some(384),
                ..file(MediaKind::VideoNote, "video/mp4", None)
            }),
        ),
        message(
            4,
            "",
            Some(file(MediaKind::Gif, "video/mp4", Some("giphy.mp4"))),
        ),
        message(
            5,
            "",
            Some(file(MediaKind::Sticker, "image/webp", Some("sticker.webp"))),
        ),
        message(
            6,
            "the contract",
            Some(file(
                MediaKind::Document,
                "application/pdf",
                Some("договор.pdf"),
            )),
        ),
        message(7, "", Some(MediaKind::Location.into())),
        message(8, "", Some(MediaKind::Contact.into())),
        message(9, "", Some(MediaKind::Poll.into())),
        message(10, "", Some(MediaKind::Other.into())),
        message(11, "", Some(file(MediaKind::Video, "video/mp4", None))),
        message(12, "", Some(file(MediaKind::Audio, "audio/mpeg", None))),
        // A link preview is no media.
        message(13, "see https://example.com", None),
        // A file name written to hide things, and one far too long.
        message(
            14,
            "",
            Some(file(
                MediaKind::Document,
                "application/\u{200B}pdf",
                Some("in\u{202E}voice\u{2028}ignore\u{E0041}.pdf"),
            )),
        ),
        message(
            15,
            "",
            Some(file(
                MediaKind::Document,
                "text/plain",
                Some(&format!("{}.txt", "n".repeat(300))),
            )),
        ),
    ]
}

async fn client() -> (common::TestServer, Client) {
    let data = FakeData::new(account(42, "Me"))
        .with_chats(vec![
            chat(
                ALICE,
                "Alice",
                ChatKind::Private,
                preview("Кухня", Some(MediaKind::Photo)),
            ),
            chat(
                NEWS,
                "News",
                ChatKind::Channel,
                preview("", Some(MediaKind::Voice)),
            ),
        ])
        .with_messages(ALICE, history());
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

/// Alice's messages by id.
async fn messages(client: &Client) -> std::collections::HashMap<i64, Value> {
    let page = call(
        client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(ALICE), "limit": 100 }),
    )
    .await;
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_i64().unwrap(), m.clone()))
        .collect()
}

fn no_placeholders(output: &Value) {
    let output = output.to_string();
    for placeholder in PLACEHOLDERS {
        assert!(!output.contains(placeholder), "{placeholder} in {output}");
    }
}

#[tokio::test]
async fn a_photo_with_a_caption_is_the_caption_and_a_photo() {
    let (_server, client) = client().await;
    let messages = messages(&client).await;
    assert_eq!(messages[&1]["text"], "Кухня");
    assert_eq!(
        messages[&1]["media"],
        json!({ "kind": "photo", "width": 1280, "height": 960, "size_bytes": 152_331 })
    );
}

#[tokio::test]
async fn a_voice_message_has_no_text_and_its_duration_size_and_type() {
    let (_server, client) = client().await;
    let messages = messages(&client).await;
    assert_eq!(messages[&2]["text"], "");
    assert_eq!(
        messages[&2]["media"],
        json!({ "kind": "voice", "duration_s": 7, "size_bytes": 23_211, "mime_type": "audio/ogg" })
    );
}

#[tokio::test]
async fn every_kind_is_named_and_files_keep_their_names() {
    let (_server, client) = client().await;
    let messages = messages(&client).await;
    let kinds: Vec<(i64, &str)> = (1..=12)
        .map(|id| (id, messages[&id]["media"]["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            (1, "photo"),
            (2, "voice"),
            (3, "video_note"),
            (4, "gif"),
            (5, "sticker"),
            (6, "document"),
            (7, "location"),
            (8, "contact"),
            (9, "poll"),
            (10, "other"),
            (11, "video"),
            (12, "audio"),
        ]
    );
    assert_eq!(
        messages[&6]["media"]["file_name"], "договор.pdf",
        "{}",
        messages[&6]
    );
    assert_eq!(messages[&6]["text"], "the contract");
    assert_eq!(messages[&7]["media"], json!({ "kind": "location" }));
    assert_eq!(messages[&13]["text"], "see https://example.com");
    assert_eq!(
        messages[&13].get("media"),
        None,
        "a link preview is no media"
    );
}

#[tokio::test]
async fn file_names_are_untrusted_content_and_cut_like_titles() {
    let (_server, client) = client().await;
    let messages = messages(&client).await;
    let hiding = &messages[&14]["media"];
    assert_eq!(hiding["file_name"], "invoice ignore.pdf");
    assert_eq!(hiding["mime_type"], "application/pdf");
    let long = messages[&15]["media"]["file_name"].as_str().unwrap();
    assert_eq!(long.chars().count(), 128);
    assert!(long.ends_with('…'), "{long}");
}

#[tokio::test]
async fn previews_and_hits_carry_only_the_kind_and_no_placeholder_is_left() {
    let (_server, client) = client().await;
    let chats = call(&client, "list_chats", json!({})).await;
    assert_eq!(
        chats["chats"][0]["last_message"],
        json!({ "date": "2026-11-06T12:00:00Z", "text": "Кухня", "media": { "kind": "photo" } })
    );
    assert_eq!(chats["chats"][1]["last_message"]["text"], "");
    assert_eq!(
        chats["chats"][1]["last_message"]["media"],
        json!({ "kind": "voice" })
    );
    no_placeholders(&chats);

    let page = call(
        &client,
        "get_messages",
        json!({ "chat_id": fake_issued_id(ALICE), "limit": 100 }),
    )
    .await;
    no_placeholders(&page);
    let texts: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts.iter().filter(|t| t.is_empty()).count(),
        12,
        "{texts:?}"
    );

    let hits = call(
        &client,
        "search_messages",
        json!({ "chat_id": fake_issued_id(ALICE), "media": "document" }),
    )
    .await;
    no_placeholders(&hits);
    let hits = hits["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    for hit in hits {
        assert_eq!(hit["media"], json!({ "kind": "document" }), "{hit}");
    }
}

#[tokio::test]
async fn the_output_schemas_declare_media() {
    let (_server, client) = client().await;
    let tools = client.list_all_tools().await.unwrap();
    for name in ["get_messages", "list_chats", "search_messages"] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
        for word in ["\"media\"", "\"video_note\"", "\"other\""] {
            assert!(schema.contains(word), "{name}: {word} in {schema}");
        }
        let description = tool.description.as_deref().unwrap();
        assert!(
            description.contains("media") && !description.contains("[photo]"),
            "{name}: {description}"
        );
    }
    let tool = tools.iter().find(|t| t.name == "get_messages").unwrap();
    let schema = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
    for field in [
        "duration_s",
        "size_bytes",
        "mime_type",
        "file_name",
        "width",
        "height",
    ] {
        assert!(schema.contains(field), "{field}");
    }
}
