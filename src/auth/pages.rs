//! Server-rendered pages of the Login flow. No template engine: a handful of
//! pages, every interpolated value goes through [`esc`].

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use super::{
    CONSENT_OFFERS,
    send_list::{ChatChoice, Choices, FolderChoice, MAX_QUERY_CHARS, Picked},
};
use crate::{
    scope::{Scope, Scopes},
    tg::{chats::ChatKind, sanitise::sanitise},
};

/// Applied to every page. No framing (the Consent button must not be clickjackable),
/// scripts only from us. No `form-action`: Chrome applies it to the redirect after
/// the Consent POST, which goes to the client's own redirect_uri.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
                   img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'";

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · Telegram MCP (unofficial)</title>
<style>
  :root {{ color-scheme: light dark; --fg:#1b1b1f; --muted:#6b6b76; --bg:#f6f6f8; --card:#fff; --accent:#2a8bd8; --line:#e2e2e8; }}
  @media (prefers-color-scheme: dark) {{ :root {{ --fg:#ececf1; --muted:#9a9aa6; --bg:#121216; --card:#1c1c22; --accent:#4aa3ea; --line:#2c2c34; }} }}
  * {{ box-sizing: border-box; }}
  body {{ margin:0; min-height:100vh; display:grid; place-items:center; padding:16px;
         font:16px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; color:var(--fg); background:var(--bg); }}
  main {{ width:100%; max-width:420px; background:var(--card); border:1px solid var(--line); border-radius:16px; padding:28px; }}
  h1 {{ font-size:20px; margin:0 0 8px; }}
  h1.ask {{ font-weight:400; line-height:1.4; }}
  h1.ask b {{ font-weight:700; overflow-wrap:anywhere; }}
  p {{ margin:0 0 14px; }}
  .muted {{ color:var(--muted); font-size:14px; }}
  .qr {{ display:grid; place-items:center; min-height:240px; margin:16px 0; border:1px dashed var(--line); border-radius:12px; padding:12px; word-break:break-all; text-align:center; }}
  .qr img {{ width:100%; max-width:240px; height:auto; background:#fff; border-radius:8px; }}
  .row {{ display:flex; gap:10px; margin-top:20px; }}
  button {{ flex:1; font:inherit; padding:10px 14px; border-radius:10px; border:1px solid var(--line); background:transparent; color:var(--fg); cursor:pointer; }}
  button.primary {{ background:var(--accent); border-color:var(--accent); color:#fff; font-weight:600; }}
  label {{ display:block; font-weight:600; margin:0 0 6px; }}
  input {{ width:100%; font:inherit; padding:10px 12px; border-radius:10px; border:1px solid var(--line); background:transparent; color:var(--fg); }}
  .error {{ color:#d93a3a; font-weight:600; }}
  fieldset.scopes {{ border:0; padding:0; margin:0 0 14px; }}
  label.scope {{ display:flex; gap:10px; align-items:flex-start; font-weight:400; margin:0 0 8px; }}
  label.scope input {{ width:auto; padding:0; margin:5px 0 0; flex:none; }}
  main:has(input.write:checked) span.read-only, main:has(input.write:checked) p.read-only {{ display:none; }}
  main:has(input.write:checked) p.writes {{ display:block; }}
  main:not(:has(input.write:checked)) p.writes {{ display:none; }}
  main:not(:has(input.write:checked)) span.read-only {{ display:inline; }}
  main:not(:has(input.write:checked)) p.read-only {{ display:block; }}
  main:has(input.send:checked) p.writes {{ display:none; }}
  main:has(input.send:checked) p.sends, main:has(input.send:checked) section.send {{ display:block; }}
  main:not(:has(input.send:checked)) p.sends, main:not(:has(input.send:checked)) section.send {{ display:none; }}
  section.send {{ margin:0 0 14px; padding:14px; border:1px solid var(--line); border-radius:12px; }}
  h2 {{ font-size:16px; margin:0 0 8px; }}
  fieldset.pick {{ border:0; padding:0; margin:14px 0 0; min-width:0; }}
  fieldset.pick legend {{ font-weight:600; padding:0; margin:0 0 6px; }}
  .chats {{ max-height:260px; overflow:auto; }}
  label.chat {{ display:flex; gap:8px; align-items:flex-start; font-weight:400; margin:0 0 6px; overflow-wrap:anywhere; }}
  label.chat input {{ width:auto; padding:0; margin:5px 0 0; flex:none; }}
  .search {{ display:flex; gap:8px; margin:0 0 8px; }}
  .search button, .row button.pick {{ flex:none; }}
  footer {{ margin-top:22px; font-size:12px; color:var(--muted); }}
</style>
</head>
<body>
<main>
{body}
<footer>Unofficial Telegram MCP server. Uses the Telegram API; not affiliated with Telegram.</footer>
</main>
</body>
</html>"#,
        title = esc(title),
    );
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn error(status: StatusCode, title: &str, message: &str) -> Response {
    page(
        status,
        title,
        &format!("<h1>{}</h1><p>{}</p>", esc(title), esc(message)),
    )
}

pub fn login(login_id: &str) -> Response {
    page(
        StatusCode::OK,
        "Log in with Telegram",
        &format!(
            r#"<h1>Log in with Telegram</h1>
<p>Open Telegram on your phone → <b>Settings → Devices → Link Desktop Device</b>, and scan this code.</p>
<div class="qr" id="qr" data-login="{id}"><span class="muted">Preparing a login code…</span></div>
<p class="muted" id="status">The code refreshes by itself. Keep this page open.</p>
<script src="/login/assets/login.js"></script>"#,
            id = esc(login_id),
        ),
    )
}

/// The Cloud password step. `error` is shown inline after a failed attempt.
pub fn password(
    status: StatusCode,
    login_id: &str,
    hint: Option<&str>,
    error: Option<&str>,
) -> Response {
    let hint = hint
        .filter(|h| !h.is_empty())
        .map(|h| format!(r#"<p class="muted">Hint: <b>{}</b></p>"#, esc(h)))
        .unwrap_or_default();
    let error = error
        .map(|e| format!(r#"<p class="error" role="alert">{}</p>"#, esc(e)))
        .unwrap_or_default();
    page(
        status,
        "Two-step verification",
        &format!(
            r#"<h1>Two-step verification</h1>
<p>This account has a cloud password. Enter it to finish logging in.</p>
{error}<form method="post" action="/login/{id}/password">
  <label for="password">Cloud password</label>
  <input id="password" name="password" type="password" autocomplete="current-password" maxlength="1024" required autofocus>
  {hint}<p class="muted">It is checked with Telegram once and never stored.</p>
  <div class="row">
    <button type="submit" class="primary">Continue</button>
  </div>
</form>"#,
            id = esc(login_id),
        ),
    )
}

pub struct ConsentView<'a> {
    pub login_id: &'a str,
    /// Self-declared at registration: anyone can call themselves "Claude".
    pub client_name: &'a str,
    /// Where the authorization code goes, the one fact the server can vouch for.
    pub redirect_target: &'a str,
    /// The code goes to a loopback host, which names no one.
    pub redirect_is_loopback: bool,
    pub account_name: &'a str,
    pub account_username: Option<&'a str>,
    /// The boxes checked when the page opens: what the client asked for, of
    /// what the screen offers ([`CONSENT_OFFERS`]), but never `send`; or, when
    /// the page comes back from its own form, what was ticked there.
    pub checked: Scopes,
    /// The Send allow-list part, shown while `send` is ticked.
    pub send: SendView<'a>,
}

/// The Consent screen's Send allow-list: the chats it may send to.
#[derive(Default)]
pub struct SendView<'a> {
    /// What the screen offers, once loaded for this Login flow; until then a
    /// button loads it.
    pub choices: Option<Choices<'a>>,
    /// What is ticked.
    pub picked: Picked,
    /// The name last searched for, shown back in the search box.
    pub query: &'a str,
    /// What the last search found, said under the search box.
    pub notice: Option<&'a str>,
    /// Why Allow was refused, or why the chats couldn't be loaded or searched.
    pub error: Option<&'a str>,
}

/// What a Scope's box on the Consent screen says it allows.
fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Read => "<b>Read</b> chats and messages, and download their media. Always included.",
        Scope::Draft => {
            "<b>Write Drafts</b>: put text in a chat's message field on all your devices, \
             replacing a draft that is there. Only you can send it."
        }
        Scope::Send => {
            "<b>Send messages</b> as you, react and mark chats as read, only in the chats \
             you choose below."
        }
        Scope::Organize => "<b>Organize</b> your chats.",
    }
}

/// One box per Scope: `read` checked and fixed, the offered ones as `checked`
/// says. Checking a write Scope hides the read-only wording (CSS only); the
/// page as served already matches the boxes as checked.
fn scope_boxes(checked: Scopes) -> String {
    let mut boxes = format!(
        r#"<label class="scope"><input type="checkbox" checked disabled> <span>{}</span></label>"#,
        scope_label(Scope::Read)
    );
    for &scope in CONSENT_OFFERS {
        boxes.push_str(&format!(
            r#"
    <label class="scope"><input type="checkbox" class="{class}" name="scope" value="{value}"{checked}> <span>{label}</span></label>"#,
            class = if scope == Scope::Send { "write send" } else { "write" },
            value = scope.as_str(),
            checked = if checked.contains(scope) { " checked" } else { "" },
            label = scope_label(scope),
        ));
    }
    boxes
}

/// The redirect host leads, in the heading; the client name follows, muted and
/// marked unverified. Names lose hidden characters (a right-to-left override
/// could reverse the sentence) before they are escaped.
fn consent_body(view: &ConsentView<'_>) -> String {
    let recipient = if view.redirect_is_loopback {
        "an app on this computer (localhost)".to_string()
    } else {
        esc(view.redirect_target)
    };
    let username = view
        .account_username
        .map(|u| format!(" (@{})", esc(&sanitise(u))))
        .unwrap_or_default();
    let client_name = sanitise(view.client_name);
    let client = match client_name.trim() {
        "" => "The app gave no name.".to_string(),
        name => format!("The app calls itself “{}” (not verified).", esc(name)),
    };
    // Shown as the boxes are checked when the page opens; CSS keeps them in step.
    let hidden = |shown: bool| if shown { "" } else { " hidden" };
    let sends = view.checked.contains(Scope::Send);
    let read_only = hidden(!view.checked.writes());
    let writes = hidden(view.checked.writes() && !sends);
    let send_hidden = hidden(sends);
    format!(
        r#"<h1 class="ask">Allow <b>{recipient}</b> <span class="read-only"{read_only}>read-only </span>access to all chats of <b>{account}</b>{username}?</h1>
<p class="muted">{client}</p>
<form method="post" action="/login/{id}/consent">
  <fieldset class="scopes">
    <legend class="muted">It may:</legend>
    {boxes}
  </fieldset>
  {send_section}
  <p class="muted read-only"{read_only}>It can only read: it can't send, edit or delete anything.</p>
  <p class="muted writes"{writes}>It can't send, edit or delete messages.</p>
  <p class="muted sends"{send_hidden}>It can't send anywhere else, and can't edit or delete messages.</p>
  <p class="muted"><b>To disconnect later</b>, end the <b>Unofficial MCP Server</b> session in Telegram → Settings → Devices. Removing or disconnecting this server in the app alone does not end that session.</p>
  <p class="muted">If you didn't just connect an app to this Telegram account, deny.</p>
  <div class="row">
    <button type="submit" name="decision" value="deny">Deny</button>
    <button type="submit" name="decision" value="allow" class="primary">Allow</button>
  </div>
</form>"#,
        account = esc(&sanitise(view.account_name)),
        id = esc(view.login_id),
        boxes = scope_boxes(view.checked),
        send_section = send_section(view.login_id, &view.send, send_hidden),
    )
}

/// The chats it may send to: loaded on request (a form round-trip, no
/// script), then Chat folders, recent chats and a search by name, each a box.
/// Its buttons come before Allow, so Enter in the search box searches.
fn send_section(login_id: &str, send: &SendView<'_>, hidden: &str) -> String {
    let action = format!("/login/{}/consent/chats", esc(login_id));
    let error = send
        .error
        .map(|e| format!(r#"<p class="error" role="alert">{}</p>"#, esc(e)))
        .unwrap_or_default();
    let Some(choices) = send.choices else {
        return format!(
            r#"<section class="send"{hidden}>
    <h2>Chats it may send to</h2>
    {error}<p class="muted">Choose from your chat folders, your recent chats, or find a chat by name. Only chats you already have.</p>
    <div class="row"><button type="submit" class="pick" formaction="{action}" formmethod="post">Choose chats</button></div>
  </section>"#
        );
    };
    let folders = if choices.folders.is_empty() {
        r#"<p class="muted">No chat folders that are a list of chats.</p>"#.to_string()
    } else {
        choices
            .folders
            .iter()
            .map(|folder| folder_box(folder, send.picked.folders.contains(&folder.id)))
            .collect()
    };
    let recent: String = choices
        .recent
        .iter()
        .map(|chat| chat_box(chat, &send.picked))
        .collect();
    let found: String = choices
        .found
        .iter()
        .map(|chat| chat_box(chat, &send.picked))
        .collect();
    let notice = send
        .notice
        .map(|n| format!(r#"<p class="muted" role="status">{}</p>"#, esc(n)))
        .unwrap_or_default();
    format!(
        r#"<section class="send"{hidden}>
    <h2>Chats it may send to</h2>
    {error}<p class="muted">Only these chats. To change them later, connect again.</p>
    <label class="chat"><input type="checkbox" name="auto_send" value="on"{auto_send}> <span>Send to these chats <b>without asking me</b> each time</span></label>
    <p class="muted">Unticked, each message waits until you confirm it in this browser.</p>
    <fieldset class="pick"><legend>Chat folders</legend>
      {folders}
    </fieldset>
    <fieldset class="pick"><legend>Recent chats</legend>
      <div class="chats">{recent}</div>
    </fieldset>
    <fieldset class="pick"><legend>Find a chat by name</legend>
      <div class="search"><input type="search" name="q" maxlength="{max_query}" value="{query}" aria-label="Chat name"><button type="submit" formaction="{action}" formmethod="post">Search</button></div>
      {notice}<div class="chats">{found}</div>
    </fieldset>
  </section>"#,
        auto_send = if send.picked.auto_send {
            " checked"
        } else {
            ""
        },
        max_query = MAX_QUERY_CHARS,
        query = esc(send.query),
    )
}

fn folder_box(folder: &FolderChoice, checked: bool) -> String {
    let emoji = folder
        .emoji
        .as_deref()
        .map(|e| format!("{} ", esc(&sanitise(e))))
        .unwrap_or_default();
    format!(
        r#"<label class="chat"><input type="checkbox" name="folder" value="{id}"{checked}> <span>{emoji}{title} <span class="muted">folder, its chats as they are now</span></span></label>"#,
        id = folder.id,
        checked = if checked { " checked" } else { "" },
        title = esc(&sanitise(&folder.title)),
    )
}

fn chat_box(chat: &ChatChoice, picked: &Picked) -> String {
    let (title, kind) = if chat.is_self {
        ("Saved Messages".to_string(), "yours")
    } else {
        (esc(&sanitise(&chat.title)), kind_label(chat.kind))
    };
    format!(
        r#"<label class="chat"><input type="checkbox" name="chat" value="{id}"{checked}> <span>{title} <span class="muted">{kind}</span></span></label>"#,
        id = esc(&chat.id.to_string()),
        checked = if picked.has_chat(chat.id.chat()) {
            " checked"
        } else {
            ""
        },
    )
}

fn kind_label(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Private => "person",
        ChatKind::Bot => "bot",
        ChatKind::Group | ChatKind::Supergroup => "group",
        ChatKind::Channel => "channel",
    }
}

pub fn consent(status: StatusCode, view: &ConsentView<'_>) -> Response {
    page(status, "Allow access", &consent_body(view))
}

/// Polls the login status and swaps the QR; moves on to Consent when logged in.
pub const LOGIN_JS: &str = r#""use strict";
(function () {
  const box = document.getElementById("qr");
  const status = document.getElementById("status");
  const id = box.dataset.login;
  let shown = null;
  async function tick() {
    let res;
    try {
      res = await fetch("/login/" + encodeURIComponent(id) + "/status", { cache: "no-store" });
    } catch (_) {
      status.textContent = "Connection lost, retrying…";
      return setTimeout(tick, 3000);
    }
    let s = null;
    try { s = await res.json(); } catch (_) {}
    if (!res.ok || !s) {
      status.textContent = (s && s.message) || "This login expired. Start again from your app.";
      box.replaceChildren();
      return;
    }
    if (s.state === "logged_in") {
      status.textContent = "Logged in. One more step…";
      window.location.assign(s.next);
      return;
    }
    if (s.state === "password") {
      status.textContent = "Scanned. This account has a cloud password…";
      window.location.assign(s.next);
      return;
    }
    if (s.state === "waiting" && s.qr_url !== shown) {
      shown = s.qr_url;
      box.replaceChildren();
      if (s.qr_svg) {
        const img = document.createElement("img");
        img.alt = "Telegram login QR code";
        img.src = "data:image/svg+xml;base64," + s.qr_svg;
        box.appendChild(img);
      } else {
        const a = document.createElement("a");
        a.href = s.qr_url;
        a.textContent = "Open in Telegram";
        box.appendChild(a);
      }
    }
    setTimeout(tick, 1500);
  }
  tick();
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(client_name: &'a str, target: &'a str, loopback: bool) -> ConsentView<'a> {
        ConsentView {
            login_id: "L",
            client_name,
            redirect_target: target,
            redirect_is_loopback: loopback,
            account_name: "Alice",
            account_username: Some("alice"),
            checked: Scopes::READ_ONLY,
            send: SendView::default(),
        }
    }

    /// `html` without the elements served `hidden`: what the user reads first.
    fn shown(html: &str) -> String {
        let mut out = html.to_string();
        while let Some(at) = out.find(" hidden>") {
            let open = out[..at].rfind('<').expect("an opening tag");
            let tag: String = out[open + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let close = format!("</{tag}>");
            let end = out[at..].find(&close).expect("a closing tag") + at + close.len();
            out.replace_range(open..end, "");
        }
        out
    }

    /// Tags gone, whitespace collapsed: the sentences as read.
    fn text(html: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn drafts() -> Scopes {
        [Scope::Draft].into_iter().collect()
    }

    fn heading(html: &str) -> &str {
        let start = html.find("<h1").expect("an h1");
        let end = html[start..].find("</h1>").expect("a closed h1") + start;
        &html[start..end]
    }

    /// The text of the element with `class="muted"` that contains `needle`.
    fn muted_containing<'h>(html: &'h str, needle: &str) -> Option<&'h str> {
        html.split(r#"<p class="muted">"#)
            .skip(1)
            .map(|rest| &rest[..rest.find("</p>").unwrap_or(rest.len())])
            .find(|p| p.contains(needle))
    }

    #[test]
    fn the_heading_names_the_redirect_host_and_not_the_client() {
        let html = consent_body(&view("Claude", "claude.ai", false));
        let h1 = heading(&html);
        assert!(h1.contains("<b>claude.ai</b>"), "{h1}");
        assert!(text(h1).contains("read-only access to all chats"), "{h1}");
        assert!(h1.contains("<b>Alice</b> (@alice)"), "{h1}");
        assert!(!h1.contains("Claude"), "{h1}");
        assert!(html.find("claude.ai").unwrap() < html.find("Claude").unwrap());
    }

    #[test]
    fn the_client_name_is_muted_and_marked_unverified() {
        let html = consent_body(&view("Claude", "evil.example", false));
        let line = muted_containing(&html, "Claude").expect("client name in a muted line");
        assert!(line.contains("calls itself"), "{line}");
        assert!(line.contains("(not verified)"), "{line}");
        assert_eq!(html.matches("Claude").count(), 1, "{html}");
    }

    #[test]
    fn a_loopback_redirect_is_an_app_on_this_computer() {
        let html = consent_body(&view("Claude Code", "127.0.0.1", true));
        let h1 = heading(&html);
        assert!(
            h1.contains("<b>an app on this computer (localhost)</b>"),
            "{h1}"
        );
        assert!(!h1.contains("127.0.0.1"), "{h1}");

        let html = consent_body(&view(
            "Cursor",
            "cursor://anysphere.cursor-retrieval",
            false,
        ));
        assert!(
            heading(&html).contains("<b>cursor://anysphere.cursor-retrieval</b>"),
            "{html}"
        );
        assert!(!html.contains("this computer"), "{html}");
    }

    #[test]
    fn everything_interpolated_is_escaped() {
        let html = consent_body(&ConsentView {
            login_id: "L\"><x",
            client_name: "<script>alert(1)</script>",
            redirect_target: "a<b>.example",
            redirect_is_loopback: false,
            account_name: "<i>Mallory</i>",
            account_username: Some("m&m"),
            checked: Scopes::READ_ONLY,
            send: SendView::default(),
        });
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(html.contains("a&lt;b&gt;.example"), "{html}");
        assert!(html.contains("&lt;i&gt;Mallory&lt;/i&gt;"), "{html}");
        assert!(html.contains("(@m&amp;m)"), "{html}");
        assert!(html.contains("/login/L&quot;&gt;&lt;x/consent"), "{html}");
    }

    #[test]
    fn hidden_characters_are_stripped_from_names() {
        // A right-to-left override could reverse the rest of the sentence visually.
        let html = consent_body(&ConsentView {
            login_id: "L",
            client_name: "Cla\u{200B}ude\u{202E}edoC",
            redirect_target: "claude.ai",
            redirect_is_loopback: false,
            account_name: "Al\u{202E}ice\u{2066}",
            account_username: Some("ali\u{200F}ce"),
            checked: Scopes::READ_ONLY,
            send: SendView::default(),
        });
        assert!(html.contains("Claude"), "{html}");
        assert!(html.contains("edoC"), "{html}");
        assert!(html.contains("<b>Alice</b> (@alice)"), "{html}");
        assert!(
            !html.chars().any(crate::tg::sanitise::is_format),
            "hidden characters left in {html:?}"
        );
    }

    #[test]
    fn a_name_that_is_only_hidden_characters_reads_as_no_name() {
        let html = consent_body(&view("\u{202E}\u{200B}", "claude.ai", false));
        assert!(!html.contains("“”"), "{html}");
        assert!(muted_containing(&html, "gave no name").is_some(), "{html}");
    }

    #[test]
    fn a_read_only_request_reads_as_read_only() {
        let html = consent_body(&view("Claude", "claude.ai", false));
        let page = text(&shown(&html));
        assert!(
            page.starts_with("Allow claude.ai read-only access to all chats of Alice (@alice)?"),
            "{page}"
        );
        assert!(
            page.contains("Read chats and messages, and download their media. Always included."),
            "{page}"
        );
        assert!(
            page.contains(
                "Write Drafts: put text in a chat's message field on all your devices, \
                 replacing a draft that is there. Only you can send it."
            ),
            "{page}"
        );
        assert!(
            page.contains("It can only read: it can't send, edit or delete anything."),
            "{page}"
        );
        assert!(
            !page.contains("can't send, edit or delete messages"),
            "{page}"
        );
        assert!(
            html.contains(r#"<input type="checkbox" class="write" name="scope" value="draft">"#),
            "the Drafts box starts unchecked: {html}"
        );
    }

    #[test]
    fn a_draft_request_drops_the_read_only_wording_and_checks_its_box() {
        let html = consent_body(&ConsentView {
            checked: drafts(),
            ..view("Claude", "claude.ai", false)
        });
        let page = text(&shown(&html));
        assert!(
            page.starts_with("Allow claude.ai access to all chats of Alice (@alice)?"),
            "{page}"
        );
        assert!(!page.contains("read-only"), "{page}");
        assert!(!page.contains("can only read"), "{page}");
        assert!(
            page.contains("It can't send, edit or delete messages."),
            "{page}"
        );
        assert!(
            html.contains(
                r#"<input type="checkbox" class="write" name="scope" value="draft" checked>"#
            ),
            "{html}"
        );
        // Both wordings are in the page, for the CSS to swap when a box changes.
        assert!(text(&html).contains("read-only access"), "{html}");
    }

    #[test]
    fn read_is_fixed_and_only_offered_scopes_get_a_box() {
        let all: Scopes = Scope::ALL.into_iter().collect();
        let html = consent_body(&ConsentView {
            checked: all,
            ..view("Claude", "claude.ai", false)
        });
        assert!(
            html.contains(r#"<input type="checkbox" checked disabled>"#),
            "{html}"
        );
        assert_eq!(
            html.matches(r#"name="scope""#).count(),
            CONSENT_OFFERS.len()
        );
        for scope in Scope::ALL {
            assert_eq!(
                html.contains(&format!(r#"value="{scope}""#)),
                CONSENT_OFFERS.contains(&scope),
                "{scope}: {html}"
            );
        }
        assert!(!html.contains(r#"value="read""#), "{html}");
    }

    fn sending() -> Scopes {
        [Scope::Send].into_iter().collect()
    }

    fn choice(id: &str, title: &str, kind: ChatKind) -> ChatChoice {
        ChatChoice {
            id: id.parse().unwrap(),
            title: title.into(),
            kind,
            is_self: false,
        }
    }

    #[test]
    fn the_send_part_shows_only_with_send_ticked() {
        let html = consent_body(&view("Claude", "claude.ai", false));
        let page = text(&shown(&html));
        assert!(!page.contains("Chats it may send to"), "{page}");
        assert!(html.contains(r#"<section class="send" hidden>"#), "{html}");
        assert!(
            html.contains(r#"class="write send" name="scope" value="send">"#),
            "the Send box starts unticked: {html}"
        );

        let html = consent_body(&ConsentView {
            checked: sending(),
            ..view("Claude", "claude.ai", false)
        });
        let page = text(&shown(&html));
        assert!(
            page.starts_with("Allow claude.ai access to all chats of Alice (@alice)?"),
            "{page}"
        );
        assert!(page.contains("Chats it may send to"), "{page}");
        assert!(
            page.contains("Send messages as you, react and mark chats as read, only in the chats you choose below."),
            "{page}"
        );
        assert!(
            page.contains("It can't send anywhere else, and can't edit or delete messages."),
            "{page}"
        );
        assert!(
            !page.contains("It can't send, edit or delete messages."),
            "{page}"
        );
        assert!(!page.contains("can only read"), "{page}");
    }

    #[test]
    fn until_chats_are_loaded_a_button_loads_them_before_allow() {
        let html = consent_body(&ConsentView {
            checked: sending(),
            ..view("Claude", "claude.ai", false)
        });
        let button = r#"<button type="submit" class="pick" formaction="/login/L/consent/chats" formmethod="post">Choose chats</button>"#;
        assert!(html.contains(button), "{html}");
        // Enter submits the first button: never Allow.
        assert!(html.find(button).unwrap() < html.find(r#"value="allow""#).unwrap());
        assert!(!html.contains(r#"name="chat""#), "{html}");
        assert!(!html.contains(r#"name="auto_send""#), "{html}");
    }

    #[test]
    fn loaded_chats_are_boxes_ticked_as_posted() {
        let folders = [FolderChoice {
            id: 3,
            title: "Work".into(),
            emoji: Some("💼".into()),
        }];
        let recent = [
            ChatChoice {
                is_self: true,
                ..choice("user:42~AAAAAAAAACo", "Alice", ChatKind::Private)
            },
            choice("user:7~AAAAAAAAAAc", "Bob <b>", ChatKind::Private),
            choice("chat:8", "Family", ChatKind::Group),
        ];
        let found = [choice(
            "channel:9~AAAAAAAAAAk",
            "Team\u{202E}",
            ChatKind::Channel,
        )];
        let html = consent_body(&ConsentView {
            checked: sending(),
            send: SendView {
                choices: Some(Choices {
                    folders: &folders,
                    recent: &recent,
                    found: &found,
                }),
                picked: Picked {
                    folders: vec![3],
                    chats: vec!["channel:9~AAAAAAAAAAk".parse().unwrap()],
                    auto_send: false,
                },
                query: "Te\"am",
                notice: Some("Found 1 of your chats."),
                error: None,
            },
            ..view("Claude", "claude.ai", false)
        });
        assert!(
            html.contains(r#"<input type="checkbox" name="auto_send" value="on"> "#),
            "auto-send is off unless ticked: {html}"
        );
        assert!(
            html.contains(r#"name="folder" value="3" checked>"#),
            "{html}"
        );
        assert!(html.contains("💼 Work"), "{html}");
        assert!(
            html.contains(r#"name="chat" value="user:7~AAAAAAAAAAc"> <span>Bob &lt;b&gt; <span class="muted">person</span>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"value="user:42~AAAAAAAAACo"> <span>Saved Messages"#),
            "{html}"
        );
        assert!(
            html.contains(r#"value="chat:8"> <span>Family <span class="muted">group</span>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="chat" value="channel:9~AAAAAAAAAAk" checked> <span>Team <span class="muted">channel</span>"#),
            "{html}"
        );
        assert!(html.contains(r#"value="Te&quot;am""#), "{html}");
        assert!(html.contains("Found 1 of your chats."), "{html}");
        let search = r#"formaction="/login/L/consent/chats" formmethod="post">Search</button>"#;
        assert!(html.contains(search), "{html}");
        assert!(html.find(search).unwrap() < html.find(r#"value="allow""#).unwrap());
        assert!(!html.contains("Choose chats</button>"), "{html}");

        let ticked = consent_body(&ConsentView {
            checked: sending(),
            send: SendView {
                choices: Some(Choices {
                    folders: &[],
                    recent: &recent,
                    found: &[],
                }),
                picked: Picked {
                    auto_send: true,
                    ..Picked::default()
                },
                error: Some("Choose at least one chat"),
                ..SendView::default()
            },
            ..view("Claude", "claude.ai", false)
        });
        assert!(
            ticked.contains(r#"name="auto_send" value="on" checked>"#),
            "{ticked}"
        );
        assert!(
            ticked.contains("No chat folders that are a list of chats."),
            "{ticked}"
        );
        assert!(
            ticked.contains(r#"<p class="error" role="alert">Choose at least one chat</p>"#),
            "{ticked}"
        );
    }
}
