//! The confirmation page and its answers. Every interpolated value goes
//! through [`esc`]; names and titles also lose hidden characters first.
//! No script at all: the CSP allows none, nor framing, nor forms to anywhere
//! but here.

use axum::{http::StatusCode, response::Response};

use super::{Outcome, View};
use crate::{
    auth::{
        authorize::{redirect_is_loopback, redirect_target},
        clients::Client,
        pages::{esc, kind_label, page_with_csp},
    },
    tg::{
        markdown::{EntityKind, FormattedText},
        sanitise::sanitise,
    },
};

/// No scripts, no framing, forms only to us, nothing loaded from anywhere else.
/// Images and fonts only from here: the wallpaper pattern and the pages' fonts.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; font-src 'self'; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Most redirect targets named as the requester.
const MAX_TARGETS: usize = 3;

/// Send or Cancel from a browser without the Confirmation cookie.
pub const WRONG_BROWSER: &str = "You can only send or cancel this message in the browser where \
                                 you connected this app. Here you can make it a Draft.";
/// Telegram didn't answer a Send or Draft.
pub const TRY_AGAIN: &str = "Telegram didn't answer, so this may not have happened yet. Press the \
                             button again: the message can't be sent twice.";

/// Who asks: where its client's authorization codes went (the one fact the
/// server can vouch for) and the name it gave itself.
pub struct Requester {
    targets: Vec<String>,
    client_name: Option<String>,
}

impl Requester {
    /// The requester as the Grant's client describes it; `None`: a client
    /// no longer registered.
    pub fn of(client: Option<&Client>) -> Self {
        let mut targets: Vec<String> = Vec::new();
        for uri in client.iter().flat_map(|c| &c.redirect_uris) {
            let target = if redirect_is_loopback(uri) {
                "an app on this computer (localhost)".to_string()
            } else {
                redirect_target(uri)
            };
            if !targets.contains(&target) && targets.len() < MAX_TARGETS {
                targets.push(target);
            }
        }
        Self {
            targets,
            client_name: client.map(|c| sanitise(&c.name).trim().to_string()),
        }
    }
}

pub struct ConfirmView<'a> {
    /// The pending send's id, as in the page's own URL.
    pub id: &'a str,
    pub view: &'a View,
    pub requester: &'a Requester,
    /// The request carried the Grant's Confirmation cookie.
    pub confirmed_browser: bool,
    pub error: Option<&'a str>,
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    page_with_csp(status, title, body, CSP)
}

/// A page with a title and a sentence.
pub fn message(status: StatusCode, title: &str, text: &str) -> Response {
    page(
        status,
        title,
        &format!("<h1>{}</h1><p>{}</p>", esc(title), esc(text)),
    )
}

/// Unknown, expired, decided, or its Grant is gone: all look the same.
pub fn no_longer_valid() -> Response {
    message(
        StatusCode::GONE,
        "No longer valid",
        "This request is no longer valid: it was already decided, it expired, or it never \
         existed. Nothing more will happen with it.",
    )
}

pub fn unavailable() -> Response {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Try again",
        "The server can't show this request right now. Reload in a minute.",
    )
}

/// Loads the page again from this site, so the browser sends its
/// SameSite=Strict cookie, which a link from another site leaves out.
pub fn reload_here() -> Response {
    let mut response = page(
        StatusCode::OK,
        "Opening…",
        r#"<meta http-equiv="refresh" content="0">
<h1>Opening the request…</h1>
<p class="muted">If nothing happens, <a href="">open it</a>.</p>"#,
    );
    response.headers_mut().insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("Sec-Fetch-Site"),
    );
    response
}

/// How a decision ended.
pub fn decided(outcome: &Outcome) -> Response {
    match outcome {
        Outcome::Sent(_) => message(
            StatusCode::OK,
            "Sent",
            "The message was sent. You can close this page.",
        ),
        Outcome::Drafted(_) => message(
            StatusCode::OK,
            "Saved as a Draft",
            "The message is in the chat's message field on all your devices. Only you can send \
             it, from Telegram. You can close this page.",
        ),
        Outcome::Cancelled => message(
            StatusCode::OK,
            "Cancelled",
            "Nothing was sent. You can close this page.",
        ),
        Outcome::Expired => no_longer_valid(),
        Outcome::Refused(refusal) => message(StatusCode::OK, "Not sent", &refusal.message),
    }
}

/// The pending send, and the buttons this browser may press.
pub fn confirm(status: StatusCode, v: &ConfirmView<'_>) -> Response {
    let shown = &v.view.shown;
    let asker = match v.requester.targets.as_slice() {
        [] => "An app".to_string(),
        targets => targets
            .iter()
            .map(|t| format!("<b>{}</b>", esc(t)))
            .collect::<Vec<_>>()
            .join(" / "),
    };
    let client = match v.requester.client_name.as_deref() {
        None | Some("") => "The app gave no name.".to_string(),
        Some(name) => format!("The app calls itself “{}” (not verified).", esc(name)),
    };
    let (chat, kind) = if shown.is_self {
        ("Saved Messages".to_string(), "yours")
    } else {
        (
            esc(&sanitise(&shown.chat_title)),
            kind_label(shown.chat_kind),
        )
    };
    let topic = shown
        .topic
        .as_ref()
        .map(|(id, title)| {
            let title = title
                .as_deref()
                .map(|t| esc(&sanitise(t)))
                .unwrap_or_else(|| format!("Topic {id}"));
            format!("<dt>Topic</dt><dd>{title}</dd>")
        })
        .unwrap_or_default();
    let reply = shown
        .reply
        .as_ref()
        .map(|r| {
            let from = r
                .from
                .as_deref()
                .map(|f| esc(&sanitise(f)))
                .unwrap_or_else(|| "you".into());
            format!(
                r#"<div class="reply" dir="auto"><span class="muted">Replying to {from} (message {id}):</span> {text}</div>"#,
                id = r.id,
                text = esc(&sanitise(&r.text)),
            )
        })
        .unwrap_or_default();
    let error = v
        .error
        .map(|e| format!(r#"<p class="error" role="alert">{}</p>"#, esc(e)))
        .unwrap_or_default();
    let draft_note = "<b>Make it a Draft</b> puts it in the chat's message field on all your \
                      devices instead; only you can send it.";
    let actions = if v.view.deciding {
        r#"<p class="muted">This message is being sent or saved right now. Reload in a moment.</p>"#
            .to_string()
    } else if v.confirmed_browser {
        format!(
            r#"<p class="muted">{draft_note}</p>
<form method="post" action="/confirm/{id}">
  <input type="hidden" name="csrf" value="{csrf}">
  <div class="row">
    <button type="submit" name="decision" value="cancel">Cancel</button>
    <button type="submit" name="decision" value="draft">Make it a Draft</button>
    <button type="submit" name="decision" value="send" class="primary">Send</button>
  </div>
</form>"#,
            id = esc(v.id),
            csrf = esc(&v.view.csrf),
        )
    } else {
        format!(
            r#"<p><b>You can only send this message in the browser where you connected this app.</b> {draft_note}</p>
<form method="post" action="/confirm/{id}">
  <input type="hidden" name="csrf" value="{csrf}">
  <div class="row">
    <button type="submit" name="decision" value="draft" class="primary">Make it a Draft</button>
  </div>
</form>"#,
            id = esc(v.id),
            csrf = esc(&v.view.csrf),
        )
    };
    page(
        status,
        "Send a message?",
        &format!(
            r#"<h1 class="ask">{asker} asks to send a message as you</h1>
<p class="muted">{client}</p>
<dl class="facts"><dt>To</dt><dd>{chat} <span class="muted">{kind}</span></dd>{topic}</dl>
{reply}<div class="message" dir="auto">{text}</div>
<p class="muted">Check the chat and the text. Links show where they lead. This request expires at {expires} UTC.</p>
{error}{actions}"#,
            text = render(&v.view.text),
            expires = esc(&v.view.expires_at.replace('T', " ").replace('Z', "")),
        ),
    )
}

/// `text` as HTML, formatted as Telegram will show it: bold, italic, code,
/// and links with where they lead written after them. Entity offsets are
/// UTF-16 code units; overlapping entities are each applied to every piece
/// of text they cover.
pub fn render(text: &FormattedText) -> String {
    let mut units: Vec<(usize, char)> = Vec::new();
    let mut at = 0usize;
    for c in text.text.chars() {
        units.push((at, c));
        at += c.len_utf16();
    }
    let total = at;
    let span = |e: &crate::tg::markdown::Entity| {
        let start = usize::try_from(e.offset).unwrap_or(0).min(total);
        let end = start
            .saturating_add(usize::try_from(e.length).unwrap_or(0))
            .min(total);
        (start, end)
    };
    let mut cuts: Vec<usize> = vec![0, total];
    for entity in &text.entities {
        let (start, end) = span(entity);
        cuts.extend([start, end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = String::new();
    for pair in cuts.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let piece: String = units
            .iter()
            .filter(|(at, _)| *at >= from && *at < to)
            .map(|(_, c)| *c)
            .collect();
        let mut open = String::new();
        let mut close = String::new();
        for entity in &text.entities {
            let (start, end) = span(entity);
            if start <= from && to <= end && start < end {
                let (o, c) = match &entity.kind {
                    EntityKind::Bold => ("<b>", "</b>"),
                    EntityKind::Italic => ("<i>", "</i>"),
                    EntityKind::Code | EntityKind::Pre { .. } => ("<code>", "</code>"),
                    EntityKind::TextUrl { .. } => ("<u>", "</u>"),
                };
                open.push_str(o);
                close.insert_str(0, c);
            }
        }
        out.push_str(&open);
        out.push_str(&esc(&piece));
        out.push_str(&close);
        for entity in &text.entities {
            if let EntityKind::TextUrl { url } = &entity.kind
                && span(entity).1 == to
                && span(entity).0 < to
            {
                out.push_str(&format!(r#" <span class="url">({})</span>"#, esc(url)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::markdown::{TextFormat, markdown};

    #[test]
    fn formatting_renders_escaped_and_links_show_where_they_lead() {
        let text = markdown("**bold** _it_ `x<y` [here](https://evil.example/?a=1&b=2) 😀 <b>");
        let html = render(&text);
        assert_eq!(
            html,
            "<b>bold</b> <i>it</i> <code>x&lt;y</code> <u>here</u> <span class=\"url\">\
             (https://evil.example/?a=1&amp;b=2)</span> 😀 &lt;b&gt;"
        );
        // Offsets are UTF-16: an emoji before the bold part takes two units.
        let text = markdown("😀 **b**");
        assert_eq!(render(&text), "😀 <b>b</b>");
        let plain = FormattedText::new("**not bold**", TextFormat::Plain);
        assert_eq!(render(&plain), "**not bold**");
    }

    #[test]
    fn nested_and_overlapping_entities_stay_well_formed() {
        let text = markdown("**bold _both_**");
        assert_eq!(render(&text), "<b>bold </b><b><i>both</i></b>");
        let odd = FormattedText {
            text: "abcd".into(),
            entities: vec![
                crate::tg::markdown::Entity {
                    kind: EntityKind::Bold,
                    offset: 0,
                    length: 3,
                },
                crate::tg::markdown::Entity {
                    kind: EntityKind::Italic,
                    offset: 2,
                    length: 99,
                },
            ],
        };
        assert_eq!(render(&odd), "<b>ab</b><b><i>c</i></b><i>d</i>");
    }
}
