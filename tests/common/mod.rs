//! A real server on a random port plus a scripted "browser + MCP client".
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Response, StatusCode, header};
use rmcp::{
    RoleClient, ServiceExt,
    model::ClientConfig,
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tg_mcp::{
    Deps,
    auth::clients::ClientRegistry,
    config::Config,
    db,
    sessions::{PausedClock, TokenTtl, Vault},
    tg::{Account, FakeConnector, FakeLogin, Login, LoginAttempt, TgError, fake::FakeData},
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub const REDIRECT: &str = "http://127.0.0.1:43123/callback";

pub struct TestServer {
    pub base: String,
    pub http: reqwest::Client,
    /// The fake Telegram side every Grant of this server connects to.
    pub connector: FakeConnector,
    /// The server's vault, to look at what a Grant holds.
    pub vault: Arc<Vault>,
    shutdown: CancellationToken,
    _dir: TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub fn account(id: i64, name: &str) -> Account {
    Account {
        id,
        first_name: name.into(),
        last_name: None,
        username: Some(name.to_lowercase()),
    }
}

/// Logs in as each Account in turn: one per started login.
pub struct SequenceLogin {
    accounts: Vec<FakeData>,
    next: AtomicUsize,
}

impl SequenceLogin {
    pub fn new(accounts: Vec<Account>) -> Self {
        Self {
            accounts: accounts.into_iter().map(FakeData::new).collect(),
            next: AtomicUsize::new(0),
        }
    }

    /// Logs in as each [`FakeData`] in turn.
    pub fn with_data(accounts: Vec<FakeData>) -> Self {
        Self {
            accounts,
            next: AtomicUsize::new(0),
        }
    }

    /// How many Telegram logins were started.
    pub fn started(&self) -> usize {
        self.next.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Login for SequenceLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        let i = self.next.fetch_add(1, Ordering::SeqCst) % self.accounts.len();
        FakeLogin::new(self.accounts[i].clone()).start().await
    }
}

pub async fn start(accounts: Vec<Account>) -> TestServer {
    start_with(accounts.into_iter().map(FakeData::new).collect()).await
}

/// Logs in as each [`FakeData`] in turn.
pub async fn start_with(accounts: Vec<FakeData>) -> TestServer {
    start_with_login(Arc::new(SequenceLogin {
        accounts,
        next: AtomicUsize::new(0),
    }))
    .await
}

pub async fn start_with_login(login: Arc<dyn Login>) -> TestServer {
    start_with_env(login, &[]).await
}

/// Every test talks from 127.0.0.1, so the per-IP rate limits of the
/// anonymous endpoints are lifted unless a test sets them (`env` wins over these).
const RELAXED_LIMITS: &[(&str, &str)] = &[
    ("TG_MCP_AUTHORIZE_PER_MINUTE", "10000"),
    ("TG_MCP_AUTHORIZE_BURST", "10000"),
    ("TG_MCP_REGISTER_PER_MINUTE", "10000"),
    ("TG_MCP_REGISTER_BURST", "10000"),
    ("TG_MCP_TOKEN_PER_MINUTE", "10000"),
    ("TG_MCP_TOKEN_BURST", "10000"),
    ("TG_MCP_MEDIA_PER_MINUTE", "10000"),
    ("TG_MCP_MEDIA_BURST", "10000"),
    ("TG_MCP_CONSENT_CHATS_PER_MINUTE", "10000"),
    ("TG_MCP_CONSENT_CHATS_BURST", "10000"),
];

/// A server with extra environment variables on top of the test defaults.
pub async fn start_with_env(login: Arc<dyn Login>, env: &[(&str, &str)]) -> TestServer {
    serve(login, env, None).await
}

/// A server whose vault reads the time from `clock` (Token expiry, the refresh
/// grace window), logging in as each Account in turn.
pub async fn start_with_clock(accounts: Vec<Account>, clock: PausedClock) -> TestServer {
    let login = Arc::new(SequenceLogin::new(accounts));
    serve(login, &[], Some(clock)).await
}

async fn serve(
    login: Arc<dyn Login>,
    env: &[(&str, &str)],
    clock: Option<PausedClock>,
) -> TestServer {
    let connector = FakeConnector::default();
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = Config::from_lookup(
        |key| match key {
            "TG_MCP_PUBLIC_URL" => Some(base.clone()),
            "TG_MCP_FAKE_TELEGRAM" => Some("1".into()),
            _ => env
                .iter()
                .chain(RELAXED_LIMITS)
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string()),
        },
        None,
    )
    .unwrap();
    let pool = db::open(&dir.path().join("test.db")).await.unwrap();
    let mut vault = Vault::new(pool.clone(), TokenTtl::default());
    if let Some(clock) = clock {
        vault = vault.with_clock(clock);
    }
    let vault = Arc::new(vault);
    let deps = Deps {
        vault: vault.clone(),
        clients: ClientRegistry::new(pool),
        login,
        connector: Arc::new(connector.clone()),
    };
    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(&config, deps, shutdown.clone());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
        .unwrap();
    });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    TestServer {
        base,
        http,
        connector,
        vault,
        shutdown,
        _dir: dir,
    }
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(rand_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

fn rand_bytes() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).unwrap();
    b
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

pub async fn json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let body = res.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

pub fn location(res: &Response) -> Url {
    let loc = res.headers()[header::LOCATION].to_str().unwrap();
    Url::parse(loc).unwrap()
}

pub fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn post_json(&self, path: &str, body: Value) -> Response {
        self.http
            .post(self.url(path))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    pub async fn post_form(
        &self,
        path: &str,
        pairs: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> Response {
        let mut req = self
            .http
            .post(self.url(path))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(pairs));
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        req.send().await.unwrap()
    }

    pub async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut req = self.http.get(self.url(path));
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        req.send().await.unwrap()
    }

    pub async fn register(&self) -> String {
        let (status, body) = json(
            self.post_json(
                "/register",
                serde_json::json!({ "client_name": "Test Client", "redirect_uris": [REDIRECT] }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["client_id"].as_str().unwrap().to_string()
    }

    /// A valid `/authorize` request for `client_id`.
    pub fn authorize_path(&self, client_id: &str, pkce: &Pkce, state: &str) -> String {
        format!(
            "/authorize?{}",
            form(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", REDIRECT),
                ("code_challenge", &pkce.challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
                ("resource", &self.url("/mcp")),
            ])
        )
    }

    /// GET /authorize; returns the login page path and the cookie, like a browser would.
    pub async fn start_authorize(
        &self,
        client_id: &str,
        pkce: &Pkce,
        state: &str,
    ) -> (String, String) {
        let res = self
            .get(&self.authorize_path(client_id, pkce, state), None)
            .await;
        assert_eq!(
            res.status(),
            StatusCode::SEE_OTHER,
            "authorize should redirect to the login page"
        );
        let cookie = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let login_path = res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        (login_path, cookie)
    }

    /// Poll the login page's status endpoint until the (fake) QR is scanned.
    pub async fn wait_logged_in(&self, login_path: &str, cookie: &str) -> String {
        for _ in 0..10 {
            let (status, body) = json(
                self.get(&format!("{login_path}/status"), Some(cookie))
                    .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            if body["state"] == "logged_in" {
                return body["next"].as_str().unwrap().to_string();
            }
            assert_eq!(body["state"], "waiting");
        }
        panic!("login never finished");
    }

    /// Full browser part: authorize → login → consent "allow" → code.
    pub async fn authorize(&self, client_id: &str, pkce: &Pkce) -> String {
        let (login_path, cookie) = self.start_authorize(client_id, pkce, "xyz").await;
        let consent_path = self.wait_logged_in(&login_path, &cookie).await;
        let page = self.get(&consent_path, Some(&cookie)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let res = self
            .post_form(&consent_path, &[("decision", "allow")], Some(&cookie))
            .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let redirect = location(&res);
        assert!(redirect.as_str().starts_with(REDIRECT));
        assert_eq!(query(&redirect, "state").as_deref(), Some("xyz"));
        assert_eq!(query(&redirect, "iss").as_deref(), Some(self.base.as_str()));
        query(&redirect, "code").expect("code in redirect")
    }

    pub async fn exchange(&self, client_id: &str, code: &str, verifier: &str) -> Response {
        self.post_form(
            "/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", client_id),
                ("redirect_uri", REDIRECT),
                ("code_verifier", verifier),
                ("resource", &self.url("/mcp")),
            ],
            None,
        )
        .await
    }

    /// Everything a fresh MCP client does to get Tokens. Returns (client_id, token response).
    pub async fn login(&self) -> (String, Value) {
        let client_id = self.register().await;
        let pkce = pkce();
        let code = self.authorize(&client_id, &pkce).await;
        let (status, tokens) = json(self.exchange(&client_id, &code, &pkce.verifier).await).await;
        assert_eq!(status, StatusCode::OK, "{tokens}");
        (client_id, tokens)
    }

    /// The browser part with `scope` on `/authorize` and the Consent form posted
    /// as `consent` (e.g. `[("scope", "draft"), ("decision", "allow")]`). Returns
    /// the Consent page's HTML and the code.
    pub async fn authorize_scoped(
        &self,
        client_id: &str,
        pkce: &Pkce,
        scope: Option<&str>,
        consent: &[(&str, &str)],
    ) -> (String, String) {
        let mut path = self.authorize_path(client_id, pkce, "xyz");
        if let Some(scope) = scope {
            path.push_str(&format!("&{}", form(&[("scope", scope)])));
        }
        let res = self.get(&path, None).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let login_path = res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(login_path.starts_with("/login/"), "{login_path}");
        let consent_path = self.wait_logged_in(&login_path, &cookie).await;
        let page = self.get(&consent_path, Some(&cookie)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let html = page.text().await.unwrap();
        let res = self.post_form(&consent_path, consent, Some(&cookie)).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let redirect = location(&res);
        (html, query(&redirect, "code").expect("code in redirect"))
    }

    /// [`TestServer::login`] with `scope` asked for and `consent` posted.
    /// Returns (client_id, token response).
    pub async fn login_scoped(
        &self,
        scope: Option<&str>,
        consent: &[(&str, &str)],
    ) -> (String, Value) {
        let client_id = self.register().await;
        let pkce = pkce();
        let (_, code) = self
            .authorize_scoped(&client_id, &pkce, scope, consent)
            .await;
        let (status, tokens) = json(self.exchange(&client_id, &code, &pkce.verifier).await).await;
        assert_eq!(status, StatusCode::OK, "{tokens}");
        (client_id, tokens)
    }

    pub async fn mcp(&self, access_token: &str) -> RunningService<RoleClient, ClientConfig> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url("/mcp"))
                .auth_header(access_token),
        );
        ClientConfig::default()
            .serve(transport)
            .await
            .expect("initialize")
    }
}
