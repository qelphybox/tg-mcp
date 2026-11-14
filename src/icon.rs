//! The server's icon (#61): the tg-mcp mark (`site/favicon.svg`, the master;
//! never Telegram's paper plane, Telegram's API ToS 2.4), served where MCP
//! clients and browsers look for it, and advertised in `serverInfo.icons`.
//!
//! The files are embedded from `assets/icon/`: a copy of the master and PNGs
//! rendered from it by `assets/icon/render.cjs` (see its header). Nothing is
//! drawn at runtime. Public, unauthenticated, cached for a day.

use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::model::Icon;

const SVG: &[u8] = include_bytes!("../assets/icon/icon.svg");
const PNG_32: &[u8] = include_bytes!("../assets/icon/favicon-32.png");
const PNG_128: &[u8] = include_bytes!("../assets/icon/icon-128.png");
const PNG_256: &[u8] = include_bytes!("../assets/icon/icon-256.png");
const PNG_512: &[u8] = include_bytes!("../assets/icon/icon-512.png");

/// `/icon.svg`, `/icon.png` (128×128), `/icon-256.png`, `/icon-512.png` and
/// `/favicon.ico` (a 32×32 PNG, as `image/png`).
pub fn router() -> Router {
    Router::new()
        .route("/icon.svg", get(|| async { file("image/svg+xml", SVG) }))
        .route("/icon.png", get(|| async { file("image/png", PNG_128) }))
        .route(
            "/icon-256.png",
            get(|| async { file("image/png", PNG_256) }),
        )
        .route(
            "/icon-512.png",
            get(|| async { file("image/png", PNG_512) }),
        )
        .route("/favicon.ico", get(|| async { file("image/png", PNG_32) }))
}

fn file(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

/// `serverInfo.icons` on the public `base_url`: PNG 128 first (some clients
/// take the first PNG and ignore SVG), then PNG 256, then the SVG.
pub fn icons(base_url: &str) -> Vec<Icon> {
    let png = |path: &str, side: u32| {
        Icon::new(format!("{base_url}{path}"))
            .with_mime_type("image/png")
            .with_sizes(vec![format!("{side}x{side}")])
    };
    vec![
        png("/icon.png", 128),
        png("/icon-256.png", 256),
        Icon::new(format!("{base_url}/icon.svg"))
            .with_mime_type("image/svg+xml")
            .with_sizes(vec!["any".into()]),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_svg_is_the_master() {
        let master = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/site/favicon.svg"))
            .expect("site/favicon.svg");
        assert!(
            master == super::SVG,
            "assets/icon/icon.svg differs from site/favicon.svg: run `node assets/icon/render.cjs`"
        );
    }
}
