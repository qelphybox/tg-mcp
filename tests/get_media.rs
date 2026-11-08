//! get_media `download` (#36) through `/mcp` with rmcp's own client, and the
//! Media links it hands out fetched like curl would: no Token, no cookie.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use common::{SequenceLogin, TestServer, account, start_with, start_with_env};
use reqwest::{StatusCode, header};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use tg_mcp::{
    links::MAX_DOWNLOAD_BYTES,
    tg::{
        chats::{ChatId, ChatKind, ChatSummary},
        download::PART_SIZE,
        fake::{FakeData, fake_file_bytes, fake_file_id, fake_issued_id},
        messages::{Media, MediaKind, RawHistoryMessage},
    },
};

type Client = rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>;

const ALICE: ChatId = ChatId::User(101);
const BOB: ChatId = ChatId::User(202);
const VIDEO_NOTE_SIZE: u64 = 3 * PART_SIZE as u64 + 17;

fn message(id: i32, media: Option<Media>) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: 1_793_923_200 + id * 60,
        from: None,
        out: false,
        text: if media.is_none() {
            "hi".into()
        } else {
            String::new()
        },
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

fn file(kind: MediaKind, size: u64, mime: &str, name: Option<&str>) -> Option<Media> {
    Some(Media {
        size_bytes: Some(size),
        mime_type: Some(mime.into()),
        file_name: name.map(Into::into),
        ..Media::from(kind)
    })
}

/// Alice's chat: media of every kind.
fn history() -> Vec<RawHistoryMessage> {
    vec![
        message(
            1,
            Some(Media {
                width: Some(1280),
                height: Some(960),
                size_bytes: Some(152_331),
                ..Media::from(MediaKind::Photo)
            }),
        ),
        message(
            2,
            Some(Media {
                duration_s: Some(9),
                ..file(MediaKind::Voice, 23_211, "audio/ogg", None).unwrap()
            }),
        ),
        message(
            3,
            Some(Media {
                duration_s: Some(12),
                width: Some(384),
                height: Some(384),
                ..file(MediaKind::VideoNote, VIDEO_NOTE_SIZE, "video/mp4", None).unwrap()
            }),
        ),
        message(
            4,
            file(
                MediaKind::Document,
                1_000,
                "application/pdf",
                Some("билеты.pdf"),
            ),
        ),
        message(5, Some(MediaKind::Location.into())),
        message(6, Some(MediaKind::Contact.into())),
        message(7, Some(MediaKind::Poll.into())),
        message(8, None),
        message(
            9,
            file(MediaKind::Video, MAX_DOWNLOAD_BYTES + 1, "video/mp4", None),
        ),
        message(
            10,
            file(MediaKind::Video, MAX_DOWNLOAD_BYTES, "video/mp4", None),
        ),
        message(11, Some(MediaKind::Other.into())),
        message(12, file(MediaKind::Sticker, 300, "image/webp", None)),
        message(13, file(MediaKind::Gif, 5_000, "video/mp4", None)),
        message(14, file(MediaKind::Audio, 7_000, "audio/mpeg", None)),
        // A photo that is gone (an expired self-destructing one).
        message(15, Some(MediaKind::Photo.into())),
        message(
            16,
            file(
                MediaKind::Document,
                64,
                "text/html",
                Some("x\"; filename=evil.html\r\nSet-Cookie: a=b.pdf"),
            ),
        ),
    ]
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

fn alice_data() -> FakeData {
    FakeData::new(account(42, "Me"))
        .with_chats(vec![chat(ALICE, "Alice")])
        .with_messages(ALICE, history())
}

fn bob_data() -> FakeData {
    FakeData::new(account(7, "Bob"))
        .with_chats(vec![chat(BOB, "Bob's friend")])
        .with_messages(
            BOB,
            vec![message(
                2,
                file(MediaKind::Voice, 999, "audio/ogg", Some("bob.ogg")),
            )],
        )
}

async fn client_of(server: &TestServer) -> (String, Value, Client) {
    let (client_id, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    (client_id, tokens, client)
}

async fn setup() -> (TestServer, Client) {
    let server = start_with(vec![alice_data()]).await;
    let (_, _, client) = client_of(&server).await;
    (server, client)
}

async fn get_media(client: &Client, chat: ChatId, id: i32) -> Result<Value, String> {
    client
        .call_tool(
            CallToolRequestParams::new("get_media").with_arguments(
                json!({ "chat_id": fake_issued_id(chat), "message_id": id, "mode": "download" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .map(|result| result.structured_content.expect("structured output"))
        .map_err(|e| e.to_string())
}

async fn link(client: &Client, id: i32) -> String {
    get_media(client, ALICE, id).await.unwrap()["download"]["url"]
        .as_str()
        .unwrap()
        .to_string()
}

/// What curl sees: a plain GET, nothing else.
async fn fetch(server: &TestServer, url: &str) -> reqwest::Response {
    server.http.get(url).send().await.unwrap()
}

fn bytes_of(chat: ChatId, id: i32, size: u64) -> Vec<u8> {
    fake_file_bytes(fake_file_id(chat, id), size)
}

#[tokio::test]
async fn a_voice_message_downloads_as_its_exact_bytes_with_safe_headers() {
    let (server, client) = setup().await;
    let answer = get_media(&client, ALICE, 2).await.unwrap();
    let download = &answer["download"];
    assert_eq!(answer["mode"], "download");
    assert_eq!(answer["kind"], "voice");
    let url = download["url"].as_str().unwrap();
    assert!(url.starts_with(&server.url("/media/")), "{url}");
    assert_eq!(url.len(), server.url("/media/").len() + 43, "256 bits");
    assert_eq!(download["mime_type"], "audio/ogg");
    assert_eq!(download["size_bytes"], 23_211);
    assert_eq!(download["duration_s"], 9);
    assert_eq!(download["suggested_filename"], "voice-2.ogg");
    let expires = time::OffsetDateTime::parse(
        download["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let left = expires - time::OffsetDateTime::now_utc();
    assert!(
        left > time::Duration::minutes(4) && left <= time::Duration::minutes(5),
        "{left}"
    );
    // Nothing that downloads without the link is in the answer.
    let printed = answer.to_string();
    for leak in [
        "fake-file-reference",
        "access_hash",
        "dc_id",
        "file_reference",
    ] {
        assert!(!printed.contains(leak), "{leak} in {printed}");
    }

    let response = fetch(&server, url).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    for (name, value) in [
        (header::CONTENT_TYPE, "audio/ogg"),
        (header::CONTENT_LENGTH, "23211"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"voice-2.ogg\"; filename*=UTF-8''voice-2.ogg",
        ),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ] {
        assert_eq!(headers[&name], value, "{name}");
    }
    let body = response.bytes().await.unwrap();
    assert!(
        body.as_ref() == bytes_of(ALICE, 2, 23_211),
        "the voice message's bytes"
    );
}

#[tokio::test]
async fn a_multi_part_video_note_downloads_whole() {
    let (server, client) = setup().await;
    let url = link(&client, 3).await;
    let response = fetch(&server, &url).await;
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    let body = response.bytes().await.unwrap();
    assert!(body.as_ref() == bytes_of(ALICE, 3, VIDEO_NOTE_SIZE));
    let requests = server.connector.files().requests();
    assert_eq!(requests.len(), 4, "one getFile per part");
}

#[tokio::test]
async fn every_kind_with_a_file_has_the_kind_and_size_get_messages_gives() {
    let (_server, client) = setup().await;
    let page = client
        .call_tool(
            CallToolRequestParams::new("get_messages").with_arguments(
                json!({ "chat_id": fake_issued_id(ALICE), "limit": 100 })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let described = |id: i64| {
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()["media"]
            .clone()
    };
    for (id, mime) in [
        (1, "image/jpeg"),
        (2, "audio/ogg"),
        (3, "video/mp4"),
        (4, "application/pdf"),
        (10, "video/mp4"),
        (12, "image/webp"),
        (13, "video/mp4"),
        (14, "audio/mpeg"),
    ] {
        let answer = get_media(&client, ALICE, id).await.unwrap();
        let media = described(i64::from(id));
        assert_eq!(answer["kind"], media["kind"], "{id}");
        assert_eq!(
            answer["download"]["size_bytes"], media["size_bytes"],
            "{id}"
        );
        assert_eq!(answer["download"]["mime_type"], mime, "{id}");
    }
}

#[tokio::test]
async fn a_file_name_is_made_safe_for_the_disk_and_the_header() {
    let (server, client) = setup().await;
    let answer = get_media(&client, ALICE, 4).await.unwrap();
    assert_eq!(answer["download"]["suggested_filename"], "билеты.pdf");
    let response = fetch(&server, answer["download"]["url"].as_str().unwrap()).await;
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"______.pdf\"; \
         filename*=UTF-8''%D0%B1%D0%B8%D0%BB%D0%B5%D1%82%D1%8B.pdf"
    );

    let answer = get_media(&client, ALICE, 16).await.unwrap();
    let name = answer["download"]["suggested_filename"].as_str().unwrap();
    assert!(!name.contains(['"', '/', '\r', '\n']), "{name}");
    let response = fetch(&server, answer["download"]["url"].as_str().unwrap()).await;
    assert!(response.headers().get(header::SET_COOKIE).is_none());
    let disposition = response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap();
    assert_eq!(disposition.matches('"').count(), 2, "{disposition}");
    assert!(disposition.starts_with("attachment; filename=\""));
    // The sender said text/html: still a download, never rendered.
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
}

#[tokio::test]
async fn a_link_serves_three_downloads_then_answers_404_like_an_unknown_one() {
    let (server, client) = setup().await;
    let url = link(&client, 2).await;
    for _ in 0..3 {
        let response = fetch(&server, &url).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
    }
    let fourth = fetch(&server, &url).await;
    assert_eq!(fourth.status(), StatusCode::NOT_FOUND);
    assert_eq!(fourth.headers()[header::CACHE_CONTROL], "no-store");
    assert!(fourth.bytes().await.unwrap().is_empty(), "no detail");

    for unknown in [
        format!("/media/{}", "A".repeat(43)),
        "/media/x".into(),
        "/media/".into(),
    ] {
        let response = server.get(&unknown, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{unknown}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_link_leads_to_its_own_message_of_its_own_grant_only() {
    let server = start_with(vec![alice_data(), bob_data()]).await;
    let (_, _, alice) = client_of(&server).await;
    let (_, _, bob) = client_of(&server).await;

    let voice = link(&alice, 2).await;
    let pdf = link(&alice, 4).await;
    let bobs = get_media(&bob, BOB, 2).await.unwrap()["download"]["url"]
        .as_str()
        .unwrap()
        .to_string();
    for (url, bytes) in [
        (&voice, bytes_of(ALICE, 2, 23_211)),
        (&pdf, bytes_of(ALICE, 4, 1_000)),
        (&bobs, bytes_of(BOB, 2, 999)),
    ] {
        let body = fetch(&server, url).await.bytes().await.unwrap();
        assert!(body.as_ref() == bytes, "{url} serves its own file");
    }
    // A changed ticket leads nowhere.
    let mut tampered = voice.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(
        fetch(&server, &tampered).await.status(),
        StatusCode::NOT_FOUND
    );

    // Bob's Grant can't make a link to Alice's chat.
    let refused = get_media(&bob, ALICE, 2).await.unwrap_err();
    assert!(refused.contains("not one of your chats"), "{refused}");
}

#[tokio::test]
async fn media_without_a_file_or_too_big_is_a_clear_tool_error() {
    let (_server, client) = setup().await;
    let error = |id| {
        let client = &client;
        async move { get_media(client, ALICE, id).await.unwrap_err() }
    };
    let too_big = error(9).await;
    assert!(
        too_big.contains(&format!("{} bytes", MAX_DOWNLOAD_BYTES + 1))
            && too_big.contains("up to 20 MB"),
        "{too_big}"
    );
    assert!(error(8).await.contains("has no media"));
    assert!(error(5).await.contains("is a location: it has no file"));
    assert!(error(6).await.contains("is a contact"));
    assert!(error(7).await.contains("is a poll"));
    assert!(error(11).await.contains("no file this server can download"));
    assert!(error(15).await.contains("no longer available"));
    assert!(error(99).await.contains("not found in this chat"));
    // Exactly the cap is fine.
    get_media(&client, ALICE, 10).await.unwrap();

    for arguments in [
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 2, "mode": "transcribe" }),
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 0, "mode": "download" }),
        json!({ "chat_id": fake_issued_id(ALICE), "message_id": 2 }),
        json!({ "chat_id": "nope", "message_id": 2, "mode": "download" }),
    ] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("get_media")
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await;
        // Malformed arguments are a tool error result, the rest a call error.
        assert!(
            result.as_ref().map_or(true, |r| r.is_error == Some(true)),
            "{arguments}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_link_survives_an_expired_file_reference_and_a_file_in_another_dc() {
    let (server, client) = setup().await;
    let url = link(&client, 3).await;
    server.connector.files().expire_references();
    server.connector.files().move_to_dc(4);
    let body = fetch(&server, &url).await.bytes().await.unwrap();
    assert!(body.as_ref() == bytes_of(ALICE, 3, VIDEO_NOTE_SIZE));
    assert_eq!(
        server.connector.files().lookups(),
        2,
        "issued, refreshed once"
    );
}

#[tokio::test]
async fn a_files_flood_answers_503_and_spends_no_download() {
    let (server, client) = setup().await;
    let url = link(&client, 2).await;
    server.connector.files().flood_after(0, 30);
    // More tries than downloads: a refused one is given back.
    for _ in 0..5 {
        let response = fetch(&server, &url).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "30");
    }
    assert_eq!(server.connector.files().requests().len(), 5, "no retry");
}

#[tokio::test]
async fn a_flood_midway_cuts_the_download_short_of_its_length() {
    let (server, client) = setup().await;
    let url = link(&client, 3).await;
    server.connector.files().flood_after(1, 30);
    let response = fetch(&server, &url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.content_length(),
        Some(VIDEO_NOTE_SIZE),
        "the client can tell it's short"
    );
    assert!(response.bytes().await.is_err(), "the body ends early");
}

#[tokio::test]
async fn revoking_the_grant_ends_its_links() {
    let server = start_with(vec![alice_data()]).await;
    let (client_id, tokens, client) = client_of(&server).await;
    let url = link(&client, 2).await;
    let response = server
        .post_form(
            "/revoke",
            &[
                ("token", tokens["refresh_token"].as_str().unwrap()),
                ("client_id", &client_id),
            ],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fetch(&server, &url).await.status(), StatusCode::NOT_FOUND);
    assert!(server.connector.files().requests().is_empty());
}

#[tokio::test]
async fn a_session_ended_in_telegram_ends_its_links() {
    let server = start_with(vec![alice_data()]).await;
    let (_, tokens, client) = client_of(&server).await;
    let url = link(&client, 2).await;
    server.connector.end_session_in_telegram(42);
    // The next request finds the session dead and deletes the Grant.
    let status = server
        .http
        .post(server.url("/mcp"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(fetch(&server, &url).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn no_page_may_read_a_link_cross_origin() {
    let (server, client) = setup().await;
    let url = link(&client, 2).await;
    let response = server
        .http
        .get(&url)
        .header(header::ORIGIN, "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
    let preflight = server
        .http
        .request(reqwest::Method::OPTIONS, &url)
        .header(header::ORIGIN, "https://evil.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .send()
        .await
        .unwrap();
    assert!(
        preflight
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
    // /mcp keeps its CORS.
    let preflight = server
        .http
        .request(reqwest::Method::OPTIONS, server.url("/mcp"))
        .header(header::ORIGIN, "https://evil.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .send()
        .await
        .unwrap();
    assert!(
        preflight
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_some()
    );
}

#[tokio::test]
async fn media_links_are_rate_limited_per_client_ip() {
    let server = start_with_env(
        Arc::new(SequenceLogin::new(vec![account(42, "Me")])),
        &[
            ("TG_MCP_MEDIA_PER_MINUTE", "1"),
            ("TG_MCP_MEDIA_BURST", "2"),
        ],
    )
    .await;
    let unknown = format!("/media/{}", "A".repeat(43));
    for _ in 0..2 {
        assert_eq!(
            server.get(&unknown, None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let limited = server.get(&unknown, None).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn get_media_is_a_read_only_tool_with_bounded_schemas() {
    let (_server, client) = setup().await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "get_media").unwrap();
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    let input = serde_json::to_string(&tool.input_schema).unwrap();
    for word in [
        "\"download\"",
        "\"message_id\"",
        "\"minimum\":1",
        "\"maxLength\":39",
    ] {
        assert!(input.contains(word), "{word} in {input}");
    }
    let output = serde_json::to_string(tool.output_schema.as_ref().unwrap()).unwrap();
    for word in [
        "\"url\"",
        "\"expires_at\"",
        "\"suggested_filename\"",
        "\"video_note\"",
    ] {
        assert!(output.contains(word), "{word} in {output}");
    }
    let description = tool.description.as_deref().unwrap();
    for word in ["curl", "5 minutes", "credential", "OGG/Opus", "20 MB"] {
        assert!(description.contains(word), "{word}");
    }
}

/// Everything the server logs while this test binary runs.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        tracing_subscriber::fmt()
            // As in production (main.rs), and everything of ours.
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
async fn no_link_file_name_or_byte_reaches_the_logs() {
    let logs = captured_logs();
    let (server, client) = setup().await;
    let answer = get_media(&client, ALICE, 4).await.unwrap();
    let url = answer["download"]["url"].as_str().unwrap().to_string();
    let body = fetch(&server, &url).await.bytes().await.unwrap();
    assert_eq!(body.len(), 1_000);
    assert_eq!(fetch(&server, &url).await.status(), StatusCode::OK);
    let ticket = url.rsplit_once('/').unwrap().1.to_string();
    // The download finishes after its last byte went out.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let logs = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(logs.contains("media link issued"), "{logs}");
    assert!(logs.contains("media link download finished"), "{logs}");
    for secret in [
        ticket.as_str(),
        "билеты",
        "fake-file-reference",
        &format!("{:?}", &body[..16]),
    ] {
        assert!(!logs.contains(secret), "{secret} in the logs");
    }
}
