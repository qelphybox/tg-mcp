//! The server's icon (#61): the tg-mcp mark at fixed paths, unauthenticated
//! and cacheable, advertised in `serverInfo.icons` with absolute URLs, and
//! the Login flow pages' favicon.

mod common;

use common::{account, start};
use reqwest::{StatusCode, header};

/// Width and height from a PNG's IHDR chunk.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG signature");
    assert_eq!(&bytes[12..16], b"IHDR");
    let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    (be(16), be(20))
}

#[tokio::test]
async fn each_icon_is_served_at_its_size_unauthenticated_and_cacheable() {
    let server = start(vec![account(42, "Me")]).await;
    for (path, side) in [
        ("/icon.png", 128),
        ("/icon-256.png", 256),
        ("/icon-512.png", 512),
        ("/favicon.ico", 32),
    ] {
        let res = server.get(path, None).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/png", "{path}");
        assert_eq!(
            res.headers()[header::CACHE_CONTROL],
            "public, max-age=86400",
            "{path}"
        );
        let bytes = res.bytes().await.unwrap();
        assert_eq!(png_size(&bytes), (side, side), "{path}");
    }
    let res = server.get("/icon.svg", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert_eq!(
        res.headers()[header::CACHE_CONTROL],
        "public, max-age=86400"
    );
    let svg = res.text().await.unwrap();
    assert!(svg.starts_with("<svg"), "{svg}");
    assert!(svg.contains("#FFB547"), "the mark's orange dot: {svg}");
}

#[tokio::test]
async fn server_info_advertises_the_icons_on_the_public_host_png_128_first() {
    let server = start(vec![account(42, "Me")]).await;
    let (_, tokens) = server.login().await;
    let client = server.mcp(tokens["access_token"].as_str().unwrap()).await;
    let peer = client.peer_info().unwrap();
    let info = peer.server_info.as_ref().expect("serverInfo");
    assert_eq!(info.name, "tg-mcp");
    assert_eq!(info.website_url.as_deref(), Some(server.url("").as_str()));
    let icons = serde_json::to_value(info.icons.as_ref().expect("icons")).unwrap();
    assert_eq!(
        icons,
        serde_json::json!([
            { "src": server.url("/icon.png"), "mimeType": "image/png", "sizes": ["128x128"] },
            { "src": server.url("/icon-256.png"), "mimeType": "image/png", "sizes": ["256x256"] },
            { "src": server.url("/icon.svg"), "mimeType": "image/svg+xml", "sizes": ["any"] },
        ])
    );
    client.cancel().await.unwrap();
}
