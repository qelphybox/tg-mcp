//! Pending sends end to end: the store on its own, then the page as a
//! browser sees it, on a real server.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use reqwest::header;

use super::*;
use crate::{
    allow_list::SendAllowList,
    auth::{
        clients::ClientRegistry,
        rate_limit::{OAuthLimits, Rate},
    },
    db,
    links::MediaLinks,
    scope::{Scope, Scopes},
    sessions::{TokenTtl, Vault},
    tg::{
        Account, Connector, FakeConnector, FakeLogin, FakeTelegram,
        chats::{ChatRef, ChatSummary},
        fake::{FIRST_SENT_ID, FakeData, FakeTopic, fake_access_hash},
        markdown::markdown,
        messages::RawHistoryMessage,
    },
};

const ME: i64 = 42;
const ALICE: ChatId = ChatId::User(101);
/// A forum: General and Releases (10).
const FORUM: ChatId = ChatId::Channel(505);
/// One of the Account's chats, not in the Send allow-list.
const BOB: ChatId = ChatId::User(102);
const TEXT: &str = "**Hi** there, see [the notes](https://example.com/n)";

fn summary(id: ChatId, title: &str, kind: ChatKind) -> ChatSummary {
    ChatSummary {
        id: id.to_string(),
        title: title.into(),
        kind,
        username: None,
        is_self: false,
        unread_count: 0,
        pinned: false,
        muted: false,
        marked_unread: false,
        last_message: None,
    }
}

fn raw(id: i32, topic: Option<i32>, text: &str) -> RawHistoryMessage {
    RawHistoryMessage {
        id,
        date: 1_793_923_200 + id,
        from: None,
        out: false,
        text: text.into(),
        media: None,
        reply_to: None,
        reply_elsewhere: None,
        forwarded: false,
        forwarded_from: None,
        edited: false,
        reactions: Vec::new(),
        pinned: false,
        topic,
    }
}

fn data() -> FakeData {
    FakeData::new(Account {
        id: ME,
        first_name: "Me".into(),
        last_name: None,
        username: None,
    })
    .with_chats(vec![
        summary(ALICE, "Alice\u{202E}", ChatKind::Private),
        summary(BOB, "Bob", ChatKind::Private),
        summary(FORUM, "Dev forum", ChatKind::Supergroup),
    ])
    .with_messages(ALICE, vec![raw(1, None, "lunch?")])
    .with_messages(
        FORUM,
        vec![
            raw(5, None, "hello"),
            raw(10, Some(10), "Releases"),
            raw(12, Some(10), "v2 is <out>"),
        ],
    )
    .with_topics(
        FORUM,
        vec![FakeTopic::new(10, "Releases"), FakeTopic::new(1, "General")],
    )
}

fn to(chat: ChatId) -> ChatRef {
    ChatRef::new(chat, fake_access_hash(chat))
}

fn message(chat: ChatId, reply_to: Option<i32>, topic: Option<i32>) -> NewMessage {
    NewMessage::new(to(chat), markdown(TEXT), reply_to, topic)
}

fn sending() -> Scopes {
    [Scope::Send].into_iter().collect()
}

fn list(auto_send: bool) -> SendAllowList {
    SendAllowList::new([ALICE, FORUM], auto_send).unwrap()
}

/// A caller of Grant `g` without a database: the store needs none.
fn caller_without_db(telegram: Arc<dyn Telegram>) -> Caller {
    Caller {
        grant_id: GrantId::from("g".to_string()),
        scopes: sending(),
        send_allow_list: Some(Arc::new(list(false).with_confirm_cookie([1; 32]))),
        telegram,
    }
}

fn store_and_caller() -> (PendingSends, Caller, Arc<FakeTelegram>) {
    let fake = Arc::new(FakeTelegram::new(data()));
    (
        PendingSends::new("https://tg.example.com/"),
        caller_without_db(fake.clone()),
        fake,
    )
}

fn id_of(url: &str) -> &str {
    url.rsplit_once('/').unwrap().1
}

#[tokio::test]
async fn a_pending_send_is_a_256_bit_secret_under_the_public_url_and_not_kept() {
    let (store, caller, _) = store_and_caller();
    let a = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    let b = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    assert!(
        a.url.starts_with("https://tg.example.com/confirm/"),
        "{}",
        a.url
    );
    assert_eq!(URL_SAFE_NO_PAD.decode(id_of(&a.url)).unwrap().len(), 32);
    assert_ne!(a.url, b.url);
    let entries = store.entries();
    assert!(!entries.contains_key(id_of(&a.url).as_bytes()));
    assert!(entries.contains_key(&key(id_of(&a.url))));
    assert_eq!(entries.len(), 2);
    assert!(a.is_open());
}

#[tokio::test]
async fn a_request_checks_the_chat_and_where_the_text_goes_before_anything_waits() {
    let (store, caller, _) = store_and_caller();
    let refused = |result: Result<Requested, RequestError>| match result {
        Err(err) => err.to_string(),
        Ok(_) => panic!("expected a refusal"),
    };
    let msg = refused(store.request(&caller, message(BOB, None, None)).await);
    assert!(
        msg.contains("not one of the chats this connection may send to"),
        "{msg}"
    );
    let msg = refused(store.request(&caller, message(FORUM, None, None)).await);
    assert!(msg.contains("is a forum"), "{msg}");
    let msg = refused(
        store
            .request(&caller, message(ALICE, Some(999), None))
            .await,
    );
    assert!(msg.contains("999"), "{msg}");
    let msg = refused(store.request(&caller, message(ALICE, None, Some(3))).await);
    assert!(msg.contains("not a forum"), "{msg}");
    let blank = NewMessage::new(to(ALICE), markdown("   "), None, None);
    let msg = refused(store.request(&caller, blank).await);
    assert!(msg.contains("needs text"), "{msg}");
    // Without `send`, nothing at all.
    let reader = Caller {
        scopes: Scopes::READ_ONLY,
        ..caller.clone()
    };
    let msg = refused(store.request(&reader, message(ALICE, None, None)).await);
    assert!(msg.contains("`send` Scope"), "{msg}");
    assert!(store.is_empty());

    // A reply in a forum Topic finds its Topic from the message.
    store
        .request(&caller, message(FORUM, Some(12), None))
        .await
        .unwrap();
    let shown = store.entries().values().next().unwrap().shown.clone();
    assert_eq!(shown.topic, Some((10, Some("Releases".into()))));
    let reply = shown.reply.unwrap();
    assert_eq!((reply.id, reply.text.as_str()), (12, "v2 is <out>"));
    assert_eq!(shown.chat_title, "Dev forum");
}

#[tokio::test]
async fn a_grant_holds_a_bounded_number_of_pending_sends() {
    let (store, caller, _) = store_and_caller();
    for _ in 0..MAX_PENDING_PER_GRANT {
        store
            .request(&caller, message(ALICE, None, None))
            .await
            .unwrap();
    }
    assert!(matches!(
        store.request(&caller, message(ALICE, None, None)).await,
        Err(RequestError::TooMany)
    ));
    // Another Grant still may.
    let other = Caller {
        grant_id: GrantId::from("h".to_string()),
        ..caller
    };
    store
        .request(&other, message(ALICE, None, None))
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_pending_send_waits_ten_minutes_then_is_no_longer_valid() {
    let (store, caller, fake) = store_and_caller();
    let requested = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    let id = id_of(&requested.url).to_string();
    assert_eq!(
        store.entry_expires(&requested.url),
        Some(requested.expires),
        "its deadline"
    );
    let before = Arc::strong_count(&fake);
    tokio::time::advance(PENDING_TTL - Duration::from_secs(1)).await;
    assert!(store.view(&id).is_some());
    assert_eq!(store.sweep(), 0);
    let waited = tokio::spawn({
        let store = Arc::new(store);
        let waiting = store.clone();
        async move { (waiting.wait(requested).await, store) }
    });
    tokio::time::advance(Duration::from_secs(2)).await;
    let (outcome, store) = waited.await.unwrap();
    assert_eq!(outcome, Outcome::Expired);
    assert!(store.view(&id).is_none());
    assert!(store.is_empty());
    assert_eq!(
        Arc::strong_count(&fake),
        before - 1,
        "its hold on the Live client is gone"
    );

    // The sweep drops one nobody waits for.
    let requested = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    tokio::time::advance(PENDING_TTL).await;
    assert_eq!(store.sweep(), 1);
    assert_eq!(store.wait(requested).await, Outcome::Expired);
}

#[tokio::test]
async fn the_client_declining_cancels_and_revoking_ends_them() {
    let (store, caller, _) = store_and_caller();
    let requested = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    assert!(store.cancel(&requested));
    assert!(!store.cancel(&requested), "decided once");
    assert_eq!(store.wait(requested).await, Outcome::Cancelled);

    let requested = store
        .request(&caller, message(ALICE, None, None))
        .await
        .unwrap();
    assert_eq!(store.revoke_grant(&caller.grant_id), 1);
    match store.wait(requested).await {
        Outcome::Refused(refusal) => assert!(refusal.message.contains("disconnected")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn refusals_say_what_happened_and_when_to_retry() {
    let chat = refusal(
        &TgError::WritesLimited {
            seconds: 42,
            limit: WriteLimit::Chat,
        },
        Decision::Send,
    );
    assert_eq!(chat.retry_after_s, Some(42));
    assert!(chat.message.contains("Send cap"), "{}", chat.message);
    assert!(
        chat.message.contains("try again in 42 s"),
        "{}",
        chat.message
    );
    assert!(chat.message.contains("not sent"), "{}", chat.message);
    let budget = refusal(
        &TgError::WritesLimited {
            seconds: 300,
            limit: WriteLimit::Account,
        },
        Decision::Draft,
    );
    assert!(
        budget.message.contains("writes budget"),
        "{}",
        budget.message
    );
    assert!(
        budget.message.contains("Draft was not saved"),
        "{}",
        budget.message
    );
    let again = refusal(&crate::tg::send::already_sent(), Decision::Send);
    assert!(
        again.message.starts_with("It was already sent"),
        "{}",
        again.message
    );
    let flood = refusal(&TgError::PeerFlood, Decision::Send);
    assert_eq!(flood.retry_after_s, None);
    assert!(flood.message.contains("don't retry"), "{}", flood.message);
}

// ── The page, on a real server ──────────────────────────────────────────────

struct Page {
    auth: Arc<AuthState>,
    base: String,
    http: reqwest::Client,
    caller: Caller,
    /// `name=value` of the Grant's Confirmation cookie, as the Consent
    /// browser sends it.
    cookie: String,
    connector: FakeConnector,
    _stop: tokio_util::sync::DropGuard,
    _dir: tempfile::TempDir,
}

fn relaxed() -> OAuthLimits {
    OAuthLimits {
        confirm: Rate {
            per_minute: 10_000,
            burst: 10_000,
        },
        ..OAuthLimits::default()
    }
}

async fn page_server(limits: OAuthLimits) -> Page {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("t.db")).await.unwrap();
    let vault = Arc::new(Vault::new(pool.clone(), TokenTtl::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let connector = FakeConnector::default();
    let auth = Arc::new(AuthState::new(
        base.clone(),
        vault.clone(),
        ClientRegistry::new(pool),
        Arc::new(FakeLogin::default()),
        Arc::new(connector.clone()),
        Arc::new(MediaLinks::new(&base)),
        limits,
    ));
    let client = auth
        .clients
        .register(
            Some("Claude\u{202E}"),
            &["https://claude.ai/api/mcp/auth_callback".to_string()],
        )
        .await
        .unwrap();
    let issued_cookie = cookie::issue(vault.ttl().refresh, false);
    let cookie = issued_cookie
        .set_cookie
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let session = serde_json::to_vec(&data()).unwrap();
    let issued = vault
        .create_grant_with_send(
            &client.id,
            ME,
            &session,
            sending(),
            Some(&list(false).with_confirm_cookie(issued_cookie.verifier)),
        )
        .await
        .unwrap();
    let grant = vault.open(issued.access.expose()).await.unwrap();
    let caller = Caller {
        grant_id: grant.grant_id.clone(),
        scopes: grant.scopes,
        send_allow_list: grant.send_allow_list.clone().map(Arc::new),
        telegram: connector.connect(grant).await.unwrap(),
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let app = router(auth.clone());
    let serving = stop.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(serving.cancelled_owned())
        .await
        .unwrap();
    });
    Page {
        auth,
        base,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        caller,
        cookie,
        connector,
        _stop: stop.drop_guard(),
        _dir: dir,
    }
}

struct Got {
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    html: String,
}

impl Page {
    async fn pending(&self, message: NewMessage) -> (String, Requested) {
        let requested = self
            .auth
            .confirmations
            .request(&self.caller, message)
            .await
            .unwrap();
        let path = format!("/confirm/{}", id_of(&requested.url));
        assert_eq!(requested.url, format!("{}{path}", self.base));
        (path, requested)
    }

    async fn got(response: reqwest::Response) -> Got {
        Got {
            status: response.status(),
            headers: response.headers().clone(),
            html: response.text().await.unwrap(),
        }
    }

    async fn get(&self, path: &str, cookie: Option<&str>, site: Option<&str>) -> Got {
        let mut request = self.http.get(format!("{}{path}", self.base));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(site) = site {
            request = request.header("sec-fetch-site", site);
        }
        Self::got(request.send().await.unwrap()).await
    }

    async fn post(&self, path: &str, pairs: &[(&str, &str)], cookie: Option<&str>) -> Got {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        let mut request = self
            .http
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        Self::got(request.send().await.unwrap()).await
    }

    /// Press `decision` on the page as served to `cookie`'s browser.
    async fn press(&self, path: &str, decision: &str, cookie: Option<&str>) -> Got {
        let page = self.get(path, cookie, None).await;
        assert_eq!(page.status, 200, "{}", page.html);
        let csrf = csrf_of(&page.html);
        self.post(path, &[("csrf", &csrf), ("decision", decision)], cookie)
            .await
    }

    fn sent(&self) -> Vec<crate::tg::fake::FakeSentMessage> {
        self.connector.sent().of(ME)
    }
}

fn csrf_of(html: &str) -> String {
    let start = html.find(r#"name="csrf" value=""#).expect("a form") + 19;
    html[start..].split('"').next().unwrap().to_string()
}

fn no_longer_valid(got: &Got) {
    assert_eq!(got.status, 410, "{}", got.html);
    assert!(got.html.contains("no longer valid"), "{}", got.html);
}

#[tokio::test]
async fn without_the_cookie_the_page_only_makes_a_draft_and_the_shell_cannot_send() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, Some(1), None)).await;

    // The model's shell: it has the URL, not the cookie.
    let shown = page.get(&path, None, None).await;
    assert_eq!(shown.status, 200);
    assert!(shown.html.contains(r#"value="draft""#), "{}", shown.html);
    assert!(!shown.html.contains(r#"value="send""#), "{}", shown.html);
    assert!(!shown.html.contains(r#"value="cancel""#), "{}", shown.html);
    assert!(
        shown
            .html
            .contains("only send this message in the browser where you connected")
    );
    let csrf = csrf_of(&shown.html);
    for decision in ["send", "cancel"] {
        let refused = page
            .post(&path, &[("csrf", &csrf), ("decision", decision)], None)
            .await;
        assert_eq!(refused.status, 403, "{decision}: {}", refused.html);
        assert!(
            refused.html.contains(page::WRONG_BROWSER),
            "{}",
            refused.html
        );
        // Another Grant's cookie, or a made-up one, is no better.
        let forged = page
            .post(
                &path,
                &[("csrf", &csrf), ("decision", decision)],
                Some("tgm_confirm_x=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            )
            .await;
        assert_eq!(forged.status, 403);
    }
    assert!(page.sent().is_empty(), "nothing was sent");
    assert!(requested.is_open());

    // On a phone without the cookie, "Make it a Draft" works.
    let drafted = page.press(&path, "draft", None).await;
    assert_eq!(drafted.status, 200, "{}", drafted.html);
    assert!(
        drafted.html.contains("Saved as a Draft"),
        "{}",
        drafted.html
    );
    let drafts = page.connector.drafts().of(ME);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].chat, ALICE);
    assert_eq!(drafts[0].reply_to, Some(1));
    assert_eq!(drafts[0].text, markdown(TEXT), "with its formatting");
    assert!(page.sent().is_empty());
    assert!(matches!(
        page.auth.confirmations.wait(requested).await,
        Outcome::Drafted(saved) if saved.chat == ALICE
    ));

    // Decided once.
    no_longer_valid(&page.get(&path, Some(&page.cookie), None).await);
    no_longer_valid(
        &page
            .post(
                &path,
                &[("csrf", &csrf), ("decision", "send")],
                Some(&page.cookie),
            )
            .await,
    );
    assert!(page.sent().is_empty());
}

#[tokio::test]
async fn in_the_consent_browser_send_sends_once() {
    let logs = Logs::capture();
    let page = page_server(relaxed()).await;
    let msg = message(ALICE, Some(1), None);
    let random_id = msg.random_id;
    let (path, requested) = page.pending(msg).await;

    let shown = page.get(&path, Some(&page.cookie), None).await;
    assert_eq!(shown.status, 200, "{}", shown.html);
    for part in [
        r#"value="send""#,
        r#"value="cancel""#,
        r#"value="draft""#,
        "<b>claude.ai</b> asks to send a message as you",
        "calls itself “Claude” (not verified)",
        "Alice <span class=\"muted\">person</span>",
        "<b>Hi</b> there, see <u>the notes</u> <span class=\"url\">(https://example.com/n)</span>",
        "Replying to Alice (message 1):</span> lunch?",
    ] {
        assert!(shown.html.contains(part), "{part}: {}", shown.html);
    }
    assert!(
        !shown.html.contains('\u{202E}'),
        "hidden characters are gone"
    );
    // The cookie lives on while it is used.
    let renewed = shown.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(
        renewed.starts_with(&format!("{};", page.cookie)),
        "{renewed}"
    );
    assert!(renewed.contains("Max-Age=2592000"), "{renewed}");

    let sent = page.press(&path, "send", Some(&page.cookie)).await;
    assert_eq!(sent.status, 200, "{}", sent.html);
    assert!(sent.html.contains("The message was sent"), "{}", sent.html);
    let messages = page.sent();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        (
            messages[0].chat,
            messages[0].reply_to,
            messages[0].random_id
        ),
        (ALICE, Some(1), random_id)
    );
    assert_eq!(messages[0].text, markdown(TEXT));
    match page.auth.confirmations.wait(requested).await {
        Outcome::Sent(sent) => assert_eq!((sent.chat, sent.id), (ALICE, FIRST_SENT_ID)),
        other => panic!("{other:?}"),
    }

    // A double click, or a second tab: nothing more.
    no_longer_valid(&page.get(&path, Some(&page.cookie), None).await);
    let csrf = csrf_of(&shown.html);
    no_longer_valid(
        &page
            .post(
                &path,
                &[("csrf", &csrf), ("decision", "send")],
                Some(&page.cookie),
            )
            .await,
    );
    assert_eq!(page.sent().len(), 1);

    // Ids and sizes only: never the URL's id, the text, the CSRF token or
    // the cookie.
    let logs = logs.text();
    assert!(
        logs.contains("pending send confirmed: message sent"),
        "{logs}"
    );
    assert!(
        logs.contains(&format!("message_id={FIRST_SENT_ID}")),
        "{logs}"
    );
    let (cookie_name, cookie_value) = page.cookie.split_once('=').unwrap();
    for secret in [
        id_of(&format!("{}{path}", page.base)),
        "Hi",
        "the notes",
        "example.com",
        "lunch",
        &csrf,
        cookie_name,
        cookie_value,
    ] {
        assert!(!logs.contains(secret), "{secret} in the logs: {logs}");
    }
}

#[tokio::test]
async fn cancel_in_the_consent_browser_sends_nothing() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, None, None)).await;
    let cancelled = page.press(&path, "cancel", Some(&page.cookie)).await;
    assert_eq!(cancelled.status, 200);
    assert!(
        cancelled.html.contains("Nothing was sent"),
        "{}",
        cancelled.html
    );
    assert_eq!(
        page.auth.confirmations.wait(requested).await,
        Outcome::Cancelled
    );
    assert!(page.sent().is_empty());
    assert!(page.connector.drafts().of(ME).is_empty());
    no_longer_valid(&page.get(&path, Some(&page.cookie), None).await);
}

#[tokio::test]
async fn csrf_is_enforced_and_the_page_runs_no_script_and_is_never_framed() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, None, None)).await;
    let shown = page.get(&path, Some(&page.cookie), None).await;
    assert_eq!(
        shown.headers[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'self'; \
         frame-ancestors 'none'"
    );
    assert_eq!(shown.headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(shown.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(shown.headers[header::REFERRER_POLICY], "no-referrer");
    assert!(!shown.html.contains("<script"), "{}", shown.html);

    let wrong = page
        .post(
            &path,
            &[("csrf", "not-the-token"), ("decision", "send")],
            Some(&page.cookie),
        )
        .await;
    assert_eq!(wrong.status, 403, "{}", wrong.html);
    let missing = page
        .post(&path, &[("decision", "send")], Some(&page.cookie))
        .await;
    assert_eq!(missing.status, 400);
    let unknown = page
        .post(
            &path,
            &[("csrf", &csrf_of(&shown.html)), ("decision", "forward")],
            Some(&page.cookie),
        )
        .await;
    assert_eq!(unknown.status, 400);
    assert!(page.sent().is_empty());
    assert!(requested.is_open(), "still waiting for a real decision");
    assert_eq!(page.get(&path, Some(&page.cookie), None).await.status, 200);
}

#[tokio::test]
async fn unknown_ids_are_no_longer_valid() {
    let page = page_server(relaxed()).await;
    no_longer_valid(&page.get("/confirm/garbage", None, None).await);
    no_longer_valid(
        &page
            .get(
                "/confirm/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                Some(&page.cookie),
                None,
            )
            .await,
    );
    no_longer_valid(
        &page
            .post(
                "/confirm/garbage",
                &[("csrf", "x"), ("decision", "send")],
                Some(&page.cookie),
            )
            .await,
    );
}

#[tokio::test]
async fn a_send_cap_refusal_at_confirmation_is_said_on_the_page_and_to_the_tool() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, None, None)).await;
    page.connector.sent().refuse_next(TgError::WritesLimited {
        seconds: 42,
        limit: WriteLimit::Chat,
    });
    let refused = page.press(&path, "send", Some(&page.cookie)).await;
    assert_eq!(refused.status, 200, "{}", refused.html);
    assert!(refused.html.contains("Not sent"), "{}", refused.html);
    assert!(
        refused.html.contains("try again in 42 s"),
        "{}",
        refused.html
    );
    match page.auth.confirmations.wait(requested).await {
        Outcome::Refused(refusal) => assert_eq!(refusal.retry_after_s, Some(42)),
        other => panic!("{other:?}"),
    }
    assert!(page.sent().is_empty());
    // Decided: the model asks again later, the user confirms again then.
    no_longer_valid(&page.get(&path, Some(&page.cookie), None).await);
}

#[tokio::test]
async fn when_telegram_does_not_answer_the_same_message_can_be_sent_again_once() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, None, None)).await;
    page.connector
        .sent()
        .refuse_next(TgError::Upstream("connection lost".into()));
    let lost = page.press(&path, "send", Some(&page.cookie)).await;
    assert_eq!(lost.status, 502, "{}", lost.html);
    assert!(
        lost.html
            .contains(&crate::auth::pages::esc(page::TRY_AGAIN)),
        "{}",
        lost.html
    );
    assert!(
        lost.html.contains(r#"value="send""#),
        "the buttons are back"
    );
    assert!(requested.is_open());

    let sent = page.press(&path, "send", Some(&page.cookie)).await;
    assert!(sent.html.contains("The message was sent"), "{}", sent.html);
    assert_eq!(page.sent().len(), 1);
    assert!(matches!(
        page.auth.confirmations.wait(requested).await,
        Outcome::Sent(_)
    ));
}

#[tokio::test]
async fn a_link_from_another_site_loads_the_page_again_from_here() {
    let page = page_server(relaxed()).await;
    let (path, _requested) = page.pending(message(ALICE, None, None)).await;
    // Followed from claude.ai: the browser leaves the SameSite=Strict cookie out.
    let bounce = page.get(&path, None, Some("cross-site")).await;
    assert_eq!(bounce.status, 200);
    assert!(
        bounce
            .html
            .contains(r#"<meta http-equiv="refresh" content="0">"#),
        "{}",
        bounce.html
    );
    assert!(!bounce.html.contains("csrf"), "{}", bounce.html);
    // Loaded again from here, the cookie comes along.
    let here = page
        .get(&path, Some(&page.cookie), Some("same-origin"))
        .await;
    assert!(here.html.contains(r#"value="send""#), "{}", here.html);
    // Another browser loading it from here gets the Draft page, no loop.
    let elsewhere = page.get(&path, None, Some("same-origin")).await;
    assert!(
        elsewhere.html.contains(r#"value="draft""#),
        "{}",
        elsewhere.html
    );
    assert!(!elsewhere.html.contains("http-equiv"), "{}", elsewhere.html);
}

#[tokio::test]
async fn a_forum_reply_shows_its_topic_and_the_message_replied_to() {
    let page = page_server(relaxed()).await;
    let (path, _requested) = page.pending(message(FORUM, Some(12), None)).await;
    let shown = page.get(&path, Some(&page.cookie), None).await;
    for part in [
        "Dev forum <span class=\"muted\">group</span>",
        "<dt>Topic</dt><dd>Releases</dd>",
        "(message 12):</span> v2 is &lt;out&gt;",
    ] {
        assert!(shown.html.contains(part), "{part}: {}", shown.html);
    }
    let sent = page.press(&path, "send", Some(&page.cookie)).await;
    assert!(sent.html.contains("The message was sent"), "{}", sent.html);
    let messages = page.sent();
    assert_eq!(
        (messages[0].chat, messages[0].topic, messages[0].reply_to),
        (FORUM, Some(10), Some(12))
    );
}

#[tokio::test]
async fn a_grant_that_is_gone_takes_its_pending_sends_with_it() {
    let page = page_server(relaxed()).await;
    let (path, requested) = page.pending(message(ALICE, None, None)).await;
    page.auth
        .vault
        .delete_grant(&page.caller.grant_id)
        .await
        .unwrap();
    no_longer_valid(&page.get(&path, Some(&page.cookie), None).await);
    assert!(page.auth.confirmations.is_empty());
    assert!(matches!(
        page.auth.confirmations.wait(requested).await,
        Outcome::Refused(_)
    ));
    assert!(page.sent().is_empty());
}

#[tokio::test]
async fn the_page_is_rate_limited_per_client_ip() {
    let page = page_server(OAuthLimits {
        confirm: Rate {
            per_minute: 1,
            burst: 2,
        },
        ..OAuthLimits::default()
    })
    .await;
    for _ in 0..2 {
        no_longer_valid(&page.get("/confirm/guess", None, None).await);
    }
    let limited = page.get("/confirm/guess", None, None).await;
    assert_eq!(limited.status, 429);
    assert!(limited.headers.contains_key(header::RETRY_AFTER));
    let limited = page
        .post(
            "/confirm/guess",
            &[("csrf", "x"), ("decision", "draft")],
            None,
        )
        .await;
    assert_eq!(limited.status, 429, "posts share the bucket");
}

/// What the server logs while a test runs (this thread only).
struct Logs {
    sink: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> Self {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = Sink(sink.clone());
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_env_filter("info,tg_mcp=trace")
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        Self {
            sink,
            _guard: guard,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.sink.lock().unwrap()).into_owned()
    }
}
