//! Server-rendered pages of the Login flow. No template engine: a handful of
//! pages, every interpolated value goes through [`esc`].

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::tg::sanitise::sanitise;

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
    format!(
        r#"<h1 class="ask">Allow <b>{recipient}</b> read-only access to all chats of <b>{account}</b>{username}?</h1>
<p class="muted">{client}</p>
<p class="muted">It can read chats and messages. It can't send, edit or delete anything.</p>
<p class="muted"><b>To disconnect later</b>, end the <b>Unofficial MCP Server</b> session in Telegram → Settings → Devices. Removing or disconnecting this server in the app alone does not end that session.</p>
<p class="muted">If you didn't just connect an app to this Telegram account, deny.</p>
<form method="post" action="/login/{id}/consent">
  <div class="row">
    <button type="submit" name="decision" value="deny">Deny</button>
    <button type="submit" name="decision" value="allow" class="primary">Allow</button>
  </div>
</form>"#,
        account = esc(&sanitise(view.account_name)),
        id = esc(view.login_id),
    )
}

pub fn consent(view: &ConsentView<'_>) -> Response {
    page(StatusCode::OK, "Allow access", &consent_body(view))
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
        }
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
        assert!(h1.contains("read-only access to all chats"), "{h1}");
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
}
