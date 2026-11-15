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
    blocked::{self, Blocked},
    forms::Form,
    world::{Party, Who, World},
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
    pub refresh_token: Option<String>,
    /// Whether the Login flow went through the Cloud password step.
    pub password_typed: bool,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Logs `who` in with `scopes` (besides `read`, which is always granted)
/// ticked on Consent: `who`'s own login accepts the QR, and the Test account
/// types its Cloud password when asked.
pub async fn login(world: &World, who: Who, scopes: &[&str]) -> Result<Grant> {
    login_sending(world, who, scopes, &[]).await
}

/// [`login`] with a Send allow-list: `send_to` names its Chats by the start
/// of their id (`user:123`), picked from the Consent screen's recent Chats.
pub async fn login_sending(
    world: &World,
    who: Who,
    scopes: &[&str],
    send_to: &[&str],
) -> Result<Grant> {
    let mut flow = LoginFlow::start(world, scopes).await?;
    flow.wait_logged_in(world, who).await?;
    ensure!(
        who != Who::TestAccount || world.cloud_password.is_none() || flow.password_typed,
        "the login never asked for the Test account's Cloud password: is two-step verification still on?"
    );
    let html = flow.consent_page().await?;
    let code = if send_to.is_empty() {
        flow.allow(&html, scopes).await?
    } else {
        flow.allow_sending(&html, scopes, send_to).await?
    };
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
    /// Whether the login page asked for the Cloud password (and got it).
    pub password_typed: bool,
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
            password_typed: false,
        })
    }

    /// Polls the login page until the QR login finished, handing each new QR
    /// to `who`'s login (the Driver for the Test account). The server only
    /// moves a login on while it is polled, so polling goes on after the QR
    /// was accepted. Types the Cloud password when the page asks for it.
    pub async fn wait_logged_in(&mut self, world: &World, who: Who) -> Result<()> {
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
                        accept(world.party(who), qr_url).await?;
                        accepted = Some(qr_url.to_string());
                    }
                }
                Some("password") => {
                    let next = body["next"]
                        .as_str()
                        .context("login status: password without next")?
                        .to_string();
                    let password = match (who, &world.cloud_password) {
                        (Who::TestAccount, Some(p)) => p,
                        (Who::TestAccount, None) => bail!(
                            "the login asks for the Test account's Cloud password, and none is configured"
                        ),
                        (Who::Peer, _) => bail!(
                            "the login asks for the Peer's Cloud password: the Peer should have none"
                        ),
                    };
                    self.submit_password(&next, password).await?;
                    self.password_typed = true;
                }
                Some("logged_in") => {
                    let next = body["next"]
                        .as_str()
                        .context("login status: logged_in without next")?;
                    self.consent_path = Some(next.to_string());
                    return Ok(());
                }
                Some("error") => {
                    let message = body["message"].as_str().unwrap_or_default();
                    if let Some(blocked) = blocked::server_message(message) {
                        return Err(blocked.into());
                    }
                    bail!("the login failed: {message}")
                }
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
        let form = Form::find(consent_html, self.consent()?)?;
        let pairs = form.submit(("decision", "allow"), "scope", scopes)?;
        self.post_consent(pairs).await
    }

    /// Ticks `scopes` (with `send`), loads the Send allow-list's pickers,
    /// ticks the Chats whose ids start with `send_to`, and presses Allow.
    pub async fn allow_sending(
        &self,
        consent_html: &str,
        scopes: &[&str],
        send_to: &[&str],
    ) -> Result<String> {
        ensure!(
            scopes.contains(&"send"),
            "a Send allow-list needs the send Scope"
        );
        let path = self.consent()?;
        let form = Form::find(consent_html, path)?;
        let pick = form
            .controls
            .iter()
            .find_map(|c| c.formaction.clone())
            .context("the Consent form has no button that loads the chats")?;
        let fields = form.fields(&[("scope", scopes)])?;
        let fields: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let res = self
            .http
            .post(format!("{}{pick}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &self.cookie)
            .body(form_body(&fields))
            .send()
            .await
            .context("loading Consent's chats")?;
        let status = res.status();
        let html = res.text().await?;
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Blocked::new(format!("the server says: {}", page_alert(&html))).into());
        }
        ensure!(
            status == StatusCode::OK,
            "loading Consent's chats: {status}"
        );
        let form = Form::find(&html, path)?;
        let offered = form.checkboxes("chat");
        let mut chats = Vec::new();
        for want in send_to {
            let found = offered
                .iter()
                .map(|(v, _)| v.as_str())
                .find(|v| *v == *want || v.starts_with(&format!("{want}~")))
                .with_context(|| {
                    format!(
                        "Consent offers no chat {want} to send to (it offers {:?})",
                        offered.iter().map(|(v, _)| v).collect::<Vec<_>>()
                    )
                })?;
            chats.push(found);
        }
        let mut pairs = form.fields(&[("scope", scopes), ("chat", &chats)])?;
        pairs.push(("decision".into(), "allow".into()));
        self.post_consent(pairs).await
    }

    async fn post_consent(&self, pairs: Vec<(String, String)>) -> Result<String> {
        let path = self.consent()?;
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
            refresh_token: body["refresh_token"].as_str().map(str::to_string),
            password_typed: self.password_typed,
        })
    }

    /// Posts the Cloud password; the server answers with a redirect to
    /// Consent, or the page again with what went wrong.
    async fn submit_password(&self, path: &str, password: &str) -> Result<()> {
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &self.cookie)
            .body(form_body(&[("password", password)]))
            .send()
            .await
            .context("Cloud password")?;
        let status = res.status();
        if status == StatusCode::SEE_OTHER {
            return Ok(());
        }
        let page = res.text().await.unwrap_or_default();
        let said = page_alert(&page);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Blocked::new(format!("the server says: {said}")).into());
        }
        bail!("the Cloud password was refused ({status}): {said}")
    }

    fn consent(&self) -> Result<&str> {
        self.consent_path
            .as_deref()
            .context("the QR login hasn't finished")
    }
}

async fn accept(party: &dyn Party, qr_url: &str) -> Result<()> {
    party
        .accept_login(qr_url)
        .await
        .context("couldn't accept the QR login")
}

/// The text of a page's `role="alert"` paragraph (where our pages say what
/// went wrong), or a note that there is none.
fn page_alert(html: &str) -> String {
    html.split("role=\"alert\">")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "(no message on the page)".into())
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
