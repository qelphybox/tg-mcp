//! The Login flow, headless: what a browser and the user's phone do, from
//! `/register` to an MCP session's access Token.

use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    forms::Form,
    world::{Driver, World},
};

/// Where the authorization code would go. Nothing listens there: the harness
/// reads the code from the redirect itself.
pub const REDIRECT: &str = "http://127.0.0.1:43123/callback";

/// The Tokens of a finished Login flow.
#[derive(Clone)]
pub struct Grant {
    pub client_id: String,
    pub access_token: String,
    /// The Scopes the token response names, e.g. `read draft`.
    pub scope: String,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Logs in with `scopes` (besides `read`, which is always granted) ticked on
/// Consent, the Driver accepting the QR.
pub async fn login(world: &World, scopes: &[&str]) -> Result<Grant> {
    let mut flow = LoginFlow::start(world, scopes).await?;
    flow.wait_logged_in(world).await?;
    let html = flow.consent_page().await?;
    let code = flow.allow(&html, scopes).await?;
    flow.token(&code).await
}

/// One Login flow in progress, step by step.
pub struct LoginFlow {
    http: reqwest::Client,
    base: String,
    client_id: String,
    verifier: String,
    state: String,
    /// `tgm_login=…`, as the browser would send it.
    cookie: String,
    /// `/login/<id>`.
    pub login_path: String,
    /// `/login/<id>/consent`, once the QR login finished.
    consent_path: Option<String>,
}

impl LoginFlow {
    /// `/register`, then `/authorize` asking for `scopes`.
    pub async fn start(world: &World, scopes: &[&str]) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let base = world.server.base.clone();
        let res = http
            .post(format!("{base}/register"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                json!({
                    "client_name": "tg-mcp acceptance",
                    "redirect_uris": [REDIRECT],
                })
                .to_string(),
            )
            .send()
            .await
            .context("/register")?;
        let (status, body) = json_body(res).await?;
        ensure!(status == StatusCode::CREATED, "/register: {status} {body}");
        let client_id = body["client_id"]
            .as_str()
            .context("/register gave no client_id")?
            .to_string();

        let verifier = URL_SAFE_NO_PAD.encode(random::<32>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = URL_SAFE_NO_PAD.encode(random::<12>());
        let scope = std::iter::once("read")
            .chain(scopes.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        let query = form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("resource", &format!("{base}/mcp")),
            ("scope", &scope),
        ]);
        let res = http
            .get(format!("{base}/authorize?{query}"))
            .send()
            .await
            .context("/authorize")?;
        let status = res.status();
        ensure!(
            status == StatusCode::SEE_OTHER,
            "/authorize answered {status}: {}",
            res.text().await.unwrap_or_default()
        );
        let login_path = location(&res)?;
        ensure!(
            login_path.starts_with("/login/"),
            "/authorize redirected to {login_path}, not to a login page"
        );
        let cookie = set_cookie(&res, "tgm_login")?;
        Ok(Self {
            http,
            base,
            client_id,
            verifier,
            state,
            cookie,
            login_path,
            consent_path: None,
        })
    }

    /// Polls the login page until the QR login finished, handing each new QR
    /// to the Driver. The server only moves a login on while it is polled, so
    /// polling goes on after the Driver accepted.
    pub async fn wait_logged_in(&mut self, world: &World) -> Result<()> {
        let started = Instant::now();
        let mut accepted: Option<String> = None;
        loop {
            let res = self
                .http
                .get(format!("{}{}/status", self.base, self.login_path))
                .header(header::COOKIE, &self.cookie)
                .send()
                .await
                .context("login status")?;
            let (status, body) = json_body(res).await?;
            ensure!(status == StatusCode::OK, "login status: {status} {body}");
            match body["state"].as_str() {
                Some("waiting") => {
                    let qr_url = body["qr_url"]
                        .as_str()
                        .context("login status: waiting without a qr_url")?;
                    if accepted.as_deref() != Some(qr_url) {
                        accept(world.driver.as_ref(), qr_url).await?;
                        accepted = Some(qr_url.to_string());
                    }
                }
                Some("password") => {
                    bail!("the login asks for a Cloud password; this world has none configured")
                }
                Some("logged_in") => {
                    let next = body["next"]
                        .as_str()
                        .context("login status: logged_in without next")?;
                    self.consent_path = Some(next.to_string());
                    return Ok(());
                }
                Some("error") => bail!("the login failed: {}", body["message"]),
                _ => bail!("login status: unexpected answer {body}"),
            }
            if started.elapsed() > world.login_timeout {
                bail!(
                    "the QR login was still waiting after {:?}: the Driver's acceptance didn't move it on",
                    world.login_timeout
                );
            }
            tokio::time::sleep(world.poll_every).await;
        }
    }

    /// The Consent page's HTML.
    pub async fn consent_page(&self) -> Result<String> {
        let path = self.consent()?;
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .context("Consent page")?;
        let status = res.status();
        let html = res.text().await?;
        ensure!(status == StatusCode::OK, "Consent page: {status}");
        Ok(html)
    }

    /// Ticks exactly `scopes` on the Consent form, presses Allow and returns
    /// the authorization code.
    pub async fn allow(&self, consent_html: &str, scopes: &[&str]) -> Result<String> {
        let path = self.consent()?;
        let form = Form::find(consent_html, path)?;
        let pairs = form.submit(("decision", "allow"), "scope", scopes)?;
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &self.cookie)
            .body(form_body(&pairs))
            .send()
            .await
            .context("Consent")?;
        let status = res.status();
        ensure!(
            status == StatusCode::SEE_OTHER,
            "Consent answered {status}: {}",
            res.text().await.unwrap_or_default()
        );
        let redirect = Url::parse(&location(&res)?)?;
        ensure!(
            redirect.as_str().starts_with(REDIRECT),
            "Consent redirected to {redirect}"
        );
        let get = |key: &str| {
            redirect
                .query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(error) = get("error") {
            bail!("Consent ended in {error}");
        }
        ensure!(
            get("state").as_deref() == Some(self.state.as_str()),
            "the redirect's state is not ours"
        );
        get("code").context("no code in the redirect")
    }

    /// `/token`: the code for Tokens.
    pub async fn token(&self, code: &str) -> Result<Grant> {
        let res = self
            .http
            .post(format!("{}/token", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &self.client_id),
                ("redirect_uri", REDIRECT),
                ("code_verifier", &self.verifier),
                ("resource", &format!("{}/mcp", self.base)),
            ]))
            .send()
            .await
            .context("/token")?;
        let (status, body) = json_body(res).await?;
        // The body holds Tokens: never print it.
        ensure!(
            status == StatusCode::OK,
            "/token answered {status}: {}",
            body["error"]
        );
        Ok(Grant {
            client_id: self.client_id.clone(),
            access_token: body["access_token"]
                .as_str()
                .context("/token gave no access_token")?
                .to_string(),
            scope: body["scope"].as_str().unwrap_or_default().to_string(),
        })
    }

    fn consent(&self) -> Result<&str> {
        self.consent_path
            .as_deref()
            .context("the QR login hasn't finished")
    }
}

async fn accept(driver: &dyn Driver, qr_url: &str) -> Result<()> {
    driver
        .accept_login(qr_url)
        .await
        .context("the Driver couldn't accept the QR login")
}

async fn json_body(res: reqwest::Response) -> Result<(StatusCode, Value)> {
    let status = res.status();
    let text = res.text().await?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    ))
}

fn location(res: &reqwest::Response) -> Result<String> {
    Ok(res
        .headers()
        .get(header::LOCATION)
        .context("no Location")?
        .to_str()?
        .to_string())
}

/// `name=value` of the `Set-Cookie` that sets `name`.
fn set_cookie(res: &reqwest::Response, name: &str) -> Result<String> {
    res.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|pair| pair.starts_with(&format!("{name}=")))
        .map(str::to_string)
        .with_context(|| format!("no {name} cookie"))
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    form(pairs)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS has randomness");
    b
}
