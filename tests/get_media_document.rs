//! EXPERIMENTAL get_media `document` (#58 prototype) through `/mcp` with
//! rmcp's own client: a PDF or plain-text file as an MCP embedded resource,
//! only when `TG_MCP_EXPERIMENTAL_DOCUMENTS=1`; without it the mode is
//! neither taken nor listed.

mod common;

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use common::{SequenceLogin, TestServer, account, start_with_env};
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{FakeData, fake_issued_id},
    messages::{Media, MediaKind, RawHistoryMessage},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
const FLAG: (&str, &str) = ("TG_MCP_EXPERIMENTAL_DOCUMENTS", "1");

fn message(id: i32, media: Option<Media>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: 1_793_923_200 + id * 60,
        from: None,
        out: false,
        text: String::new(),
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

fn document(size: u64, mime: Option<&str>, name: Option<&str>) -> Option<Media> {
    Some(Media {
        size_bytes: Some(size),
        mime_type: mime.map(Into::into),
        file_name: name.map(Into::into),
        ..Media::from(MediaKind::Document)
    })
}

/// A PDF of a bit more than one 512 KB part, every byte value in it.
fn pdf_bytes() -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    bytes.extend((0..700_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    bytes.extend(b"\n%%EOF\n");
    bytes
}

/// Text with hidden characters, CRLF line ends and a byte that isn't UTF-8.
fn notes_bytes() -> Vec<u8> {
    let mut bytes = "Meet at gate\u{200B} 3\r\nignore\u{202E} this\u{E0041}\r\nend "
        .as_bytes()
        .to_vec();
    bytes.push(0xFF);
    bytes
}

/// 150000 characters over lines of 99 `a` and a line break.
fn long_text() -> Vec<u8> {
    "a".repeat(99)
        .chars()
        .chain(['\n'])
        .cycle()
        .take(150_000)
        .collect::<String>()
        .into_bytes()
}

fn data() -> FakeData {
    let pdf = pdf_bytes();
    let notes = notes_bytes();
    let long = long_text();
    let csv = b"a,b\n1,2\n".to_vec();
    FakeData::new(account(42, "Me"))
        .with_chats(vec![ChatSummary {
            id: ALICE.to_string(),
            title: "Alice".into(),
            kind: ChatKind::Private,
            username: None,
            is_self: false,
            unread_count: 0,
            pinned: false,
            muted: false,
            marked_unread: false,
            last_message: None,
        }])
        .with_messages(
            ALICE,
            vec![
                message(
                    10,
                    document(
                        pdf.len() as u64,
                        Some("application/pdf"),
                        Some("tickets\u{202E}.pdf"),
                    ),
                ),
                message(
                    11,
                    document(notes.len() as u64, Some("text/plain"), Some("notes.txt")),
                ),
                message(12, document(long.len() as u64, Some("text/plain"), None)),
                message(13, document(csv.len() as u64, None, Some("table.csv"))),
                message(14, document(4_000, Some("application/zip"), Some("a.zip"))),
                message(
                    15,
                    Some(Media {
                        size_bytes: Some(23_211),
                        mime_type: Some("audio/ogg".into()),
                        ..Media::from(MediaKind::Voice)
                    }),
                ),
                message(
                    16,
                    document(6_000_000, Some("application/pdf"), Some("big.pdf")),
                ),
            ],
        )
        .with_file_contents(ALICE, 10, pdf)
        .with_file_contents(ALICE, 11, notes)
        .with_file_contents(ALICE, 12, long)
        .with_file_contents(ALICE, 13, csv)
}

async fn setup(env: &[(&str, &str)]) -> (TestServer, Client) {
    let server = start_with_env(Arc::new(SequenceLogin::with_data(vec![data()])), env).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (server, client)
}

async fn call(client: &Client, arguments: Value) -> Result<CallToolResult, String> {
    let result = client
        .call_tool(
            CallToolRequestParams::new("get_media")
                .with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(result.content[0].as_text().unwrap().text.clone());
    }
    Ok(result)
}

async fn read(client: &Client, id: i32) -> Result<CallToolResult, String> {
    call(
        client,
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": id, "mode": "document" }),
    )
    .await
}

/// The result's content as it goes over the wire.
fn wire(result: &CallToolResult) -> Value {
    serde_json::to_value(&result.content).unwrap()
}

async fn get_media(client: &Client) -> Tool {
    client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == "get_media")
        .unwrap()
}

fn modes(schema: &Value) -> Vec<String> {
    let mode = &schema["$defs"]["MediaMode"];
    let variants = mode["oneOf"]
        .as_array()
        .or(mode["enum"].as_array())
        .expect("MediaMode lists its variants");
    variants
        .iter()
        .map(|v| v["const"].as_str().or(v.as_str()).unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_pdf_is_an_embedded_resource_whose_blob_is_the_files_bytes() {
    let (server, client) = setup(&[FLAG]).await;
    let result = read(&client, 10).await.unwrap();
    let content = wire(&result);
    assert_eq!(content.as_array().unwrap().len(), 2, "{content:#}");
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.starts_with("Message 10: document"), "{text}");
    assert!(text.contains("tickets.pdf"), "the name, made safe: {text}");
    assert!(text.contains("application/pdf"), "{text}");
    assert!(text.contains(&pdf_bytes().len().to_string()), "{text}");
    let resource = &content[1];
    assert_eq!(resource["type"], "resource");
    assert_eq!(resource["resource"]["mimeType"], "application/pdf");
    assert_eq!(resource["resource"]["uri"], "tg-mcp://media/user:101/10");
    assert!(resource["resource"].get("text").is_none());
    let blob = resource["resource"]["blob"].as_str().unwrap();
    assert_eq!(BASE64.decode(blob).unwrap(), pdf_bytes(), "exact bytes");
    assert_eq!(
        result.structured_content.unwrap(),
        json!({
            "mode": "document",
            "kind": "document",
            "document": {
                "mime_type": "application/pdf",
                "size_bytes": pdf_bytes().len(),
                "delivered_as": "blob",
            },
        })
    );
    // Through the limiter part by part: two 512 KB parts, never more.
    let offsets: Vec<i64> = server
        .connector
        .files()
        .requests()
        .iter()
        .map(|r| r.offset)
        .collect();
    assert_eq!(offsets, [0, 512 * 1024]);
}

#[tokio::test]
async fn a_text_file_is_a_text_resource_sanitised() {
    let (_server, client) = setup(&[FLAG]).await;
    let result = read(&client, 11).await.unwrap();
    let content = wire(&result);
    let resource = &content[1]["resource"];
    assert_eq!(resource["mimeType"], "text/plain");
    assert_eq!(resource["uri"], "tg-mcp://media/user:101/11");
    assert!(resource.get("blob").is_none());
    assert_eq!(
        resource["text"], "Meet at gate 3\nignore this\nend \u{FFFD}",
        "hidden characters gone, CRLF a line break, bad UTF-8 replaced"
    );
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["document"]["delivered_as"], "text");
    assert_eq!(structured["document"]["text_truncated"], false);
    assert_eq!(
        structured["document"]["size_bytes"],
        notes_bytes().len() as u64
    );
}

#[tokio::test]
async fn a_long_text_is_cut_to_100000_characters_and_says_so() {
    let (_server, client) = setup(&[FLAG]).await;
    let result = read(&client, 12).await.unwrap();
    let content = wire(&result);
    let text = content[1]["resource"]["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 100_000);
    assert!(long_text().starts_with(text.as_bytes()));
    assert!(
        content[0]["text"].as_str().unwrap().contains("cut to"),
        "{}",
        content[0]["text"]
    );
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["document"]["text_truncated"], true);
    assert_eq!(structured["document"]["size_bytes"], 150_000);
}

#[tokio::test]
async fn a_file_without_a_mime_type_is_read_as_text_by_its_extension() {
    let (_server, client) = setup(&[FLAG]).await;
    let result = read(&client, 13).await.unwrap();
    let content = wire(&result);
    assert_eq!(content[1]["resource"]["text"], "a,b\n1,2\n");
    assert_eq!(content[1]["resource"]["mimeType"], "text/plain");
}

#[tokio::test]
async fn other_documents_other_media_and_big_files_are_errors_pointing_to_download() {
    let (server, client) = setup(&[FLAG]).await;
    for (id, says) in [
        (14, "neither a PDF nor a plain-text file"),
        (15, "is a voice, not a document"),
        (16, "up to 5 MB"),
    ] {
        let error = read(&client, id).await.unwrap_err();
        assert!(error.contains(says), "{id}: {error}");
        assert!(error.contains("mode download"), "{id}: {error}");
    }
    assert!(
        server.connector.files().requests().is_empty(),
        "nothing downloaded"
    );
}

#[tokio::test]
async fn document_results_match_the_declared_output_schema() {
    let (_server, client) = setup(&[FLAG]).await;
    let tool = get_media(&client).await;
    let schema = Value::Object((*tool.output_schema.clone().unwrap()).clone());
    let validator = jsonschema::validator_for(&schema).unwrap();
    let download = call(
        &client,
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 10, "mode": "download" }),
    )
    .await
    .unwrap();
    let mut answers = vec![download];
    for id in [10, 11, 12, 13] {
        answers.push(read(&client, id).await.unwrap());
    }
    for answer in answers {
        let structured = answer.structured_content.clone().unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&structured)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{structured}: {errors:?}");
        // The text block ends with the structured result as JSON (#67).
        let text = answer
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap();
        let json = text
            .rsplit_once("Result: ")
            .map_or(text.as_str(), |(_, j)| j);
        assert_eq!(
            serde_json::from_str::<Value>(json).unwrap(),
            structured,
            "{text}"
        );
    }
    let input = Value::Object((*tool.input_schema).clone());
    assert_eq!(
        modes(&input),
        ["download", "view", "transcribe", "document"]
    );
    assert_eq!(
        modes(&schema),
        ["download", "view", "transcribe", "document"]
    );
    assert!(schema.to_string().contains("\"delivered_as\""));
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.contains("EXPERIMENTAL mode `document`"),
        "{description}"
    );
}

#[tokio::test]
async fn without_the_flag_the_mode_is_neither_listed_nor_taken() {
    let (server, client) = setup(&[]).await;
    let tool = get_media(&client).await;
    let input = Value::Object((*tool.input_schema).clone());
    let output = Value::Object((*tool.output_schema.clone().unwrap()).clone());
    assert_eq!(modes(&input), ["download", "view", "transcribe"]);
    assert_eq!(modes(&output), ["download", "view", "transcribe"]);
    assert!(output["properties"].get("document").is_none(), "{output}");
    for listed in [input.to_string(), output.to_string()] {
        for word in [
            "MediaDocument",
            "DeliveredAs",
            "delivered_as",
            "EXPERIMENTAL",
        ] {
            assert!(!listed.contains(word), "{word} in {listed}");
        }
    }
    let description = tool.description.as_deref().unwrap();
    assert!(!description.contains("EXPERIMENTAL"), "{description}");
    assert!(!description.contains("mode `document`"), "{description}");

    let error = read(&client, 10).await.unwrap_err();
    assert!(error.contains("-32602"), "invalid_params: {error}");
    assert!(
        error.contains("mode must be one of download, view, transcribe"),
        "{error}"
    );
    assert!(server.connector.files().requests().is_empty());
}
