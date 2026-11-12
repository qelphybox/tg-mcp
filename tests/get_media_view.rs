//! get_media `view` (#34) through `/mcp` with rmcp's own client: the
//! picture of a message as MCP image content, with its structured content
//! matching the tool's own output schema.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use common::{TestServer, account, start_with};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use serde_json::{Value, json};
use tg_mcp::tg::{
    chats::{ChatId, ChatKind, ChatSummary},
    fake::{
        FakeData, FakeFormat, FakePicture, FakeSize, FakeTranscription, fake_file_id,
        fake_issued_id, fake_picture_bytes,
    },
    messages::{Media, MediaKind, RawHistoryMessage},
    view::{MAX_VIEW_BYTES, SizeForm},
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
const BOB: ChatId = ChatId::User(202);

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

fn photo(w: u32, h: u32, size: u64) -> Option<Media> {
    Some(Media {
        width: Some(w),
        height: Some(h),
        size_bytes: Some(size),
        ..Media::from(MediaKind::Photo)
    })
}

fn file(kind: MediaKind, size: u64, mime: &str) -> Option<Media> {
    Some(Media {
        size_bytes: Some(size),
        mime_type: Some(mime.into()),
        ..Media::from(kind)
    })
}

fn sized(r#type: &str, form: SizeForm, w: u32, h: u32, bytes: u64) -> FakeSize {
    FakeSize::new(r#type, form, (w, h), bytes)
}

fn stripped() -> FakeSize {
    sized("i", SizeForm::Stripped, 0, 0, 40)
}

/// A thumbnail `m` of `bytes` in `format`, after a stripped one.
fn thumb(w: u32, h: u32, bytes: u64, format: FakeFormat) -> Option<Vec<FakeSize>> {
    Some(vec![
        stripped(),
        sized("m", SizeForm::File, w, h, bytes).as_format(format),
    ])
}

fn picture(message_id: i32, grouped_id: Option<i64>, sizes: Option<Vec<FakeSize>>) -> FakePicture {
    FakePicture {
        message_id,
        grouped_id,
        sizes,
    }
}

/// Photos with only an `m` and an `x` size.
fn mx(m: u64, x: u64) -> Option<Vec<FakeSize>> {
    Some(vec![
        stripped(),
        sized("m", SizeForm::File, 320, 240, m),
        sized("x", SizeForm::File, 800, 600, x),
    ])
}

/// Alice's chat: photos, albums interleaved with other messages, and media
/// with and without thumbnails.
fn alice_data() -> FakeData {
    let mut history = vec![
        message(1, photo(1280, 960, 152_331)),
        message(3, photo(500, 375, 40_000)),
        message(9, photo(1280, 960, 150_000)),
        message(17, photo(1280, 960, 150_000)),
        message(30, photo(1280, 960, 150_000)),
        message(31, file(MediaKind::Video, 4_000_000, "video/mp4")),
        message(40, file(MediaKind::VideoNote, 900_000, "video/mp4")),
        message(41, file(MediaKind::Gif, 300_000, "video/mp4")),
        message(42, file(MediaKind::Video, 2_000_000, "video/mp4")),
        message(43, file(MediaKind::Sticker, 25_000, "image/webp")),
        message(
            44,
            file(MediaKind::Sticker, 12_000, "application/x-tgsticker"),
        ),
        message(45, file(MediaKind::Document, 120_000, "application/pdf")),
        message(46, file(MediaKind::Document, 2_000_000, "image/png")),
        message(47, file(MediaKind::Voice, 23_211, "audio/ogg")),
        message(48, Some(MediaKind::Location.into())),
        message(49, None),
        message(50, photo(5000, 4000, 3_000_000)),
        message(51, photo(5000, 4000, 2_000_000)),
        // A photo that is gone (an expired self-destructing one).
        message(52, Some(MediaKind::Photo.into())),
    ];
    // An album of 6 around a message of Bob's (outside channels ids are
    // the Account's), next to another album and a lone photo.
    for id in [10, 11, 13, 14, 15, 16] {
        history.push(message(id, photo(1280, 960, 100_000 + id as u64)));
    }
    for id in 60..64 {
        history.push(message(id, photo(5000, 4000, 400_000)));
    }
    for id in 70..73 {
        history.push(message(id, photo(5000, 4000, 900_000)));
    }
    let mut pictures = vec![
        picture(9, Some(66), None),
        picture(30, Some(88), None),
        picture(31, Some(88), thumb(320, 180, 9_000, FakeFormat::Jpeg)),
        picture(40, None, thumb(320, 320, 12_000, FakeFormat::Jpeg)),
        picture(41, None, thumb(320, 180, 8_000, FakeFormat::Jpeg)),
        picture(43, None, thumb(128, 128, 5_000, FakeFormat::Webp)),
        picture(
            44,
            None,
            Some(vec![
                sized("j", SizeForm::Path, 0, 0, 300),
                sized("m", SizeForm::File, 128, 128, 3_000).as_format(FakeFormat::Tgs),
            ]),
        ),
        // A file sent as image/png whose thumbnail is a JPEG.
        picture(46, None, thumb(320, 240, 10_000, FakeFormat::Jpeg)),
        picture(
            50,
            None,
            Some(vec![
                stripped(),
                sized("x", SizeForm::File, 800, 640, MAX_VIEW_BYTES + 1),
                sized("y", SizeForm::Progressive, 1280, 1024, 3_000_000),
            ]),
        ),
        picture(
            51,
            None,
            Some(vec![
                sized("m", SizeForm::File, 320, 256, 500_000),
                sized("x", SizeForm::File, 800, 640, 1_500_000),
                sized("y", SizeForm::Progressive, 1280, 1024, 2_000_000),
            ]),
        ),
    ];
    for id in [10, 11, 13, 14, 15, 16] {
        pictures.push(picture(id, Some(77), None));
    }
    for id in 60..64 {
        pictures.push(picture(id, Some(99), mx(100_000, 400_000)));
    }
    for id in 70..73 {
        pictures.push(picture(id, Some(111), mx(600_000, 900_000)));
    }
    FakeData::new(account(42, "Me"))
        .with_chats(vec![chat(ALICE, "Alice"), chat(BOB, "Bob")])
        .with_messages(ALICE, history)
        .with_messages(BOB, vec![message(12, photo(1280, 960, 99_000))])
        .with_pictures(ALICE, pictures)
        .with_pictures(BOB, vec![picture(12, Some(77), None)])
        .with_transcriptions(ALICE, vec![FakeTranscription::new(47, "hello", Some(0))])
}

fn chat(id: ChatId, title: &str) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind: ChatKind::Private,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: None,
    }
}

async fn setup() -> (TestServer, Client) {
    let server = start_with(vec![alice_data()]).await;
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
        let text = result.content[0].as_text().unwrap().text.clone();
        return Err(text);
    }
    Ok(result)
}

fn view_args(id: i32) -> Value {
    json!({ "chat_id": fake_issued_id(ALICE), "message_id": id, "mode": "view" })
}

async fn view(client: &Client, id: i32) -> Result<CallToolResult, String> {
    call(client, view_args(id)).await
}

/// The images of a result: (MIME type, bytes).
fn images(result: &CallToolResult) -> Vec<(String, Vec<u8>)> {
    result
        .content
        .iter()
        .filter_map(ContentBlock::as_image)
        .map(|image| (image.mime_type.clone(), BASE64.decode(&image.data).unwrap()))
        .collect()
}

fn text(result: &CallToolResult) -> String {
    result.content[0]
        .as_text()
        .expect("text first")
        .text
        .clone()
}

fn shown(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("structured output")["view"]
        .clone()
}

fn shown_ids(result: &CallToolResult) -> Vec<i64> {
    shown(result)["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|image| image["message_id"].as_i64().unwrap())
        .collect()
}

fn bytes_of(chat: ChatId, id: i32, r#type: &str, size: u64, format: FakeFormat) -> Vec<u8> {
    fake_picture_bytes(fake_file_id(chat, id), r#type, size, format)
}

#[tokio::test]
async fn a_photo_is_one_jpeg_image_and_a_text_saying_what_it_is() {
    let (server, client) = setup().await;
    let result = view(&client, 1).await.unwrap();
    // Telegram's x size of a 1280x960 photo.
    let x = 152_331 * 800 * 600 / (1280 * 960);
    let images = images(&result);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].0, "image/jpeg");
    assert!(images[0].1 == bytes_of(ALICE, 1, "x", x, FakeFormat::Jpeg));
    assert_eq!(result.content.len(), 2, "a text, then the image");
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["mode"], "view");
    assert_eq!(structured["kind"], "photo");
    assert!(structured.get("download").is_none());
    assert_eq!(
        shown(&result),
        json!({
            "images": [{
                "message_id": 1, "kind": "photo", "mime_type": "image/jpeg",
                "width": 800, "height": 600, "size_bytes": x,
            }],
            "album_truncated": false,
        })
    );
    let text = text(&result);
    assert!(text.contains("Message 1 (photo): 1 image."), "{text}");
    assert!(
        text.contains(&format!("message 1, photo, image/jpeg, 800x600, {x} bytes")),
        "{text}"
    );
    // One album lookup over the ids around it, one getFile.
    assert_eq!(
        server.connector.files().album_fetches(),
        [(1..=10).collect::<Vec<_>>()]
    );
    assert_eq!(server.connector.files().requests().len(), 1);
    // Nothing that fetches the file without the server is in the answer.
    let printed = serde_json::to_string(&result).unwrap();
    for leak in [
        "fake-file-reference",
        "access_hash",
        "dc_id",
        "file_reference",
    ] {
        assert!(!printed.contains(leak), "{leak}");
    }
}

#[tokio::test]
async fn a_large_view_is_y_when_the_photo_has_one() {
    let (_server, client) = setup().await;
    let mut args = view_args(1);
    args["size"] = json!("large");
    let large = call(&client, args).await.unwrap();
    assert!(images(&large)[0].1 == bytes_of(ALICE, 1, "y", 152_331, FakeFormat::Jpeg));
    assert_eq!(shown(&large)["images"][0]["width"], 1280);
    // A small photo has no y: its x, its full size.
    let mut args = view_args(3);
    args["size"] = json!("large");
    let small = call(&client, args).await.unwrap();
    assert_eq!(
        (
            shown(&small)["images"][0]["width"].clone(),
            shown(&small)["images"][0]["height"].clone()
        ),
        (json!(500), json!(375))
    );
    let mut args = view_args(3);
    args["size"] = json!("standard");
    assert_eq!(shown_ids(&call(&client, args).await.unwrap()), [3]);
}

#[tokio::test]
async fn an_album_of_six_shows_four_in_order_with_the_asked_one() {
    let (server, client) = setup().await;
    let result = view(&client, 13).await.unwrap();
    assert_eq!(shown_ids(&result), [10, 11, 13, 14]);
    let answer = shown(&result);
    // Bob's 12 shares the album id, 9 is another album, 17 in none.
    assert_eq!(answer["album_message_ids"], json!([10, 11, 13, 14, 15, 16]));
    assert_eq!(answer["album_truncated"], true);
    assert_eq!(result.structured_content.as_ref().unwrap()["kind"], "photo");
    let images = images(&result);
    for (image, id) in images.iter().zip([10, 11, 13, 14]) {
        let x = (100_000 + id as u64) * 800 * 600 / (1280 * 960);
        assert!(
            image.1 == bytes_of(ALICE, id, "x", x, FakeFormat::Jpeg),
            "{id}"
        );
    }
    let text = text(&result);
    assert!(text.contains("album of 6 messages"), "{text}");
    assert!(
        text.contains("Not shown") && text.contains("15, 16"),
        "{text}"
    );
    assert_eq!(
        server.connector.files().album_fetches(),
        [(4..=22).collect::<Vec<_>>()],
        "one getMessages over the ids around it"
    );

    // The last one: the four up to it.
    assert_eq!(
        shown_ids(&view(&client, 16).await.unwrap()),
        [13, 14, 15, 16]
    );
    // Bob's message isn't Alice's 12.
    let error = view(&client, 12).await.unwrap_err();
    assert!(error.contains("not found in this chat"), "{error}");
}

#[tokio::test]
async fn an_album_of_two_shows_both_a_video_by_its_thumbnail() {
    let (_server, client) = setup().await;
    let result = view(&client, 31).await.unwrap();
    assert_eq!(shown_ids(&result), [30, 31]);
    let answer = shown(&result);
    assert_eq!(answer["album_truncated"], false);
    assert_eq!(answer["album_message_ids"], json!([30, 31]));
    assert_eq!(answer["images"][1]["kind"], "video");
    assert_eq!(result.structured_content.as_ref().unwrap()["kind"], "video");
    assert!(images(&result)[1].1 == bytes_of(ALICE, 31, "m", 9_000, FakeFormat::Jpeg));
    assert!(!text(&result).contains("Not shown"));
}

#[tokio::test]
async fn video_notes_gifs_stickers_and_files_show_their_thumbnail() {
    let (_server, client) = setup().await;
    for (id, kind, mime, size, format) in [
        (40, "video_note", "image/jpeg", 12_000, FakeFormat::Jpeg),
        (41, "gif", "image/jpeg", 8_000, FakeFormat::Jpeg),
        (43, "sticker", "image/webp", 5_000, FakeFormat::Webp),
        // Declared image/png; its thumbnail is a JPEG and says so.
        (46, "document", "image/jpeg", 10_000, FakeFormat::Jpeg),
    ] {
        let result = view(&client, id).await.unwrap();
        let images = images(&result);
        assert_eq!(images.len(), 1, "{id}");
        assert_eq!(images[0].0, mime, "{id}");
        assert!(
            images[0].1 == bytes_of(ALICE, id, "m", size, format),
            "{id}"
        );
        let image = &shown(&result)["images"][0];
        assert_eq!(
            (image["kind"].as_str(), image["mime_type"].as_str()),
            (Some(kind), Some(mime))
        );
    }
}

#[tokio::test]
async fn media_without_a_picture_is_a_clear_tool_error() {
    let (server, client) = setup().await;
    let error = |id| {
        let client = &client;
        async move { view(client, id).await.unwrap_err() }
    };
    let video = error(42).await;
    assert!(
        video.contains("no thumbnail") && video.contains("mode download"),
        "{video}"
    );
    let pdf = error(45).await;
    assert!(
        pdf.contains("has no thumbnail to view") && pdf.contains("download"),
        "{pdf}"
    );
    let lottie = error(44).await;
    assert!(lottie.contains("not an image clients can show"), "{lottie}");
    assert!(error(47).await.contains("is audio"));
    assert!(error(48).await.contains("no picture to view"));
    assert!(error(49).await.contains("has no media"));
    assert!(error(52).await.contains("no longer available"));
    assert!(error(99).await.contains("not found in this chat"));
    // Only the animated sticker's thumbnail was fetched (to find out what
    // it is); nothing else reached the file servers.
    assert_eq!(server.connector.files().requests().len(), 1);
}

#[tokio::test]
async fn a_view_fits_in_its_bytes_with_smaller_sizes_or_fewer_images() {
    let (_server, client) = setup().await;
    let total = |result: &CallToolResult| -> u64 {
        images(result)
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum()
    };
    // Even its smallest size is too big: an error, nothing downloaded.
    let error = view(&client, 50).await.unwrap_err();
    assert!(
        error.contains(&format!("{} bytes", MAX_VIEW_BYTES + 1)) && error.contains("download"),
        "{error}"
    );
    // x doesn't fit, m does.
    let result = view(&client, 51).await.unwrap();
    assert_eq!(shown(&result)["images"][0]["size_bytes"], 500_000);
    // Four of 400 KB: the first two step down to m.
    let result = view(&client, 62).await.unwrap();
    assert_eq!(shown_ids(&result), [60, 61, 62, 63]);
    let sizes: Vec<u64> = images(&result)
        .iter()
        .map(|(_, b)| b.len() as u64)
        .collect();
    assert_eq!(sizes, [100_000, 100_000, 400_000, 400_000]);
    assert!(total(&result) <= MAX_VIEW_BYTES);
    assert_eq!(shown(&result)["album_truncated"], false);
    // Three whose smallest size is 600 KB: only the asked one, at x.
    let result = view(&client, 71).await.unwrap();
    assert_eq!(shown_ids(&result), [71]);
    assert_eq!(shown(&result)["images"][0]["size_bytes"], 900_000);
    assert_eq!(shown(&result)["album_truncated"], true);
    assert!(text(&result).contains("70, 72"), "{}", text(&result));
}

#[tokio::test]
async fn size_goes_only_with_view() {
    let (_server, client) = setup().await;
    let error = call(
        &client,
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 1, "mode": "download",
                "size": "large" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("size only applies to mode view"), "{error}");
    assert!(error.contains("-32602"), "invalid_params: {error}");
    let mut args = view_args(1);
    args["size"] = json!("huge");
    assert!(call(&client, args).await.is_err());
}

#[tokio::test]
async fn every_modes_structured_content_matches_the_declared_output_schema() {
    let (_server, client) = setup().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_media").unwrap();
    let schema = Value::Object((*tool.output_schema.clone().unwrap()).clone());
    let validator = jsonschema::validator_for(&schema).unwrap();
    let download = call(
        &client,
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 40, "mode": "download" }),
    )
    .await
    .unwrap();
    let transcribe = call(
        &client,
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 47, "mode": "transcribe" }),
    )
    .await
    .unwrap();
    let mut answers = vec![download, transcribe];
    for id in [1, 13, 31, 43, 71] {
        answers.push(view(&client, id).await.unwrap());
    }
    for answer in answers {
        let structured = answer.structured_content.clone().unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&structured)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{structured}: {errors:?}");
        // Clients that show the model only `content` (claude.ai) still give
        // it the structured result: the text block ends with it as JSON.
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
    // The schema describes every mode, and the input takes `size`.
    let output = schema.to_string();
    for word in [
        "\"view\"",
        "\"album_truncated\"",
        "\"album_message_ids\"",
        "\"download\"",
        "\"transcribe\"",
    ] {
        assert!(output.contains(word), "{word} in {output}");
    }
    let input = serde_json::to_string(&tool.input_schema).unwrap();
    for word in ["\"view\"", "\"large\"", "\"standard\""] {
        assert!(input.contains(word), "{word} in {input}");
    }
    let description = tool.description.as_deref().unwrap();
    for word in ["view", "image", "1 MB", "album_truncated", "thumbnail"] {
        assert!(description.contains(word), "{word}");
    }
}

#[tokio::test]
async fn a_files_flood_ends_the_view_without_a_retry() {
    let (server, client) = setup().await;
    server.connector.files().flood_after(0, 30);
    let error = view(&client, 13).await.unwrap_err();
    assert!(
        error.contains("wait 30 s") && error.contains("FLOOD_WAIT"),
        "{error}"
    );
    assert_eq!(
        server.connector.files().requests().len(),
        1,
        "never retried"
    );
}

#[tokio::test]
async fn a_file_in_another_dc_is_viewed_after_one_migration() {
    let (server, client) = setup().await;
    server.connector.files().move_to_dc(4);
    let result = view(&client, 31).await.unwrap();
    assert_eq!(shown_ids(&result), [30, 31]);
    let dcs: Vec<i32> = server
        .connector
        .files()
        .requests()
        .iter()
        .map(|r| r.dc_id)
        .collect();
    assert_eq!(dcs, [2, 4, 2, 4], "each image asks its message's DC first");
}

#[tokio::test]
async fn another_grants_chat_cant_be_viewed() {
    let bob = FakeData::new(account(7, "Bob")).with_chats(vec![chat(BOB, "Bob's friend")]);
    let server = start_with(vec![alice_data(), bob]).await;
    let (_, _alice) = server.login().await;
    let (_, tokens) = server.login().await;
    let bob = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    let error = view(&bob, 1).await.unwrap_err();
    assert!(error.contains("not one of your chats"), "{error}");
    assert!(server.connector.files().requests().is_empty());
}

/// Everything the server logs while this test binary runs.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        tracing_subscriber::fmt()
            .with_env_filter("info,tg_mcp=trace,rmcp=info")
            .with_ansi(false)
            .with_writer(move || Sink(sink.clone()))
            .init();
        logs
    })
    .clone()
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_view_logs_ids_and_sizes_never_bytes() {
    let logs = captured_logs();
    let (_server, client) = setup().await;
    let result = view(&client, 13).await.unwrap();
    let image = &result.content[1].as_image().unwrap().data;
    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    let line = logs
        .lines()
        .find(|l| l.contains("media view served") && l.contains("message_id=13"))
        .unwrap_or_else(|| panic!("{logs}"));
    assert!(
        line.contains("images=4") && line.contains("bytes="),
        "{line}"
    );
    for secret in [&image[..40], "fake-file-reference", "JFIF"] {
        assert!(!logs.contains(secret), "{secret} in the logs");
    }
}
