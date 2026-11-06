//! A real server on a random port plus a scripted "browser + MCP client".
#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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
    sessions::{TokenTtl, Vault},
    tg::{Account, FakeConnector, FakeLogin, Login, LoginAttempt, TgError},
};
use tokio_util::sync::CancellationToken;
use url::Url;

pub const REDIRECT: &str = "http://127.0.0.1:43123/callback";

pub struct TestServer {
    pub base: String,
    pub http: reqwest::Client,
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
    accounts: Vec<Account>,
    next: AtomicUsize,
}

#[async_trait::async_trait]
impl Login for SequenceLogin {
    async fn start(&self) -> Result<Box<dyn LoginAttempt>, TgError> {
        let i = self.next.fetch_add(1, Ordering::SeqCst) % self.accounts.len();
        FakeLogin::new(self.accounts[i].clone()).start().await
    }
}

pub async fn start(accounts: Vec<Account>) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = Config::from_lookup(|key| match key {
        "TG_MCP_PUBLIC_URL" => Some(base.clone()),
        "TG_MCP_FAKE_TELEGRAM" => Some("1".into()),
        _ => None,
    })
    .unwrap();
    let pool = db::open(&dir.path().join("test.db")).await.unwrap();
    let deps = Deps {
        vault: Arc::new(Vault::new(pool.clone(), TokenTtl::default())),
        clients: ClientRegistry::new(pool),
        login: Arc::new(SequenceLogin {
            accounts,
            next: AtomicUsize::new(0),
        }),
        connector: Arc::new(FakeConnector),
    };
    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(&config, deps, shutdown.clone());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
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

    /// GET /authorize; returns the login page path and the cookie, like a browser would.
    pub async fn start_authorize(
        &self,
        client_id: &str,
        pkce: &Pkce,
        state: &str,
    ) -> (String, String) {
        let path = format!(
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
        );
        let res = self.get(&path, None).await;
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
