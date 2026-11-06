//! OAuth clients: dynamic registration (RFC 7591), public clients only.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::SqlitePool;
use url::{Host, Url};

const MAX_REDIRECT_URIS: usize = 10;
const MAX_URI_LEN: usize = 2000;
const MAX_NAME_CHARS: usize = 100;

/// Schemes that must never be a redirect target, even as "private-use" app schemes.
const FORBIDDEN_SCHEMES: &[&str] = &[
    "javascript",
    "data",
    "file",
    "vbscript",
    "about",
    "blob",
    "ws",
    "wss",
    "ftp",
];

#[derive(Debug, Clone)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
}

impl Client {
    /// RFC 8252 §7.3: a loopback redirect matches on any port; everything else exactly.
    pub fn accepts_redirect(&self, requested: &str) -> bool {
        self.redirect_uris.iter().any(|registered| {
            if registered == requested {
                return true;
            }
            match (Url::parse(registered), Url::parse(requested)) {
                (Ok(mut registered), Ok(mut requested))
                    if is_loopback_http(&registered) && is_loopback_http(&requested) =>
                {
                    let _ = registered.set_port(None);
                    let _ = requested.set_port(None);
                    registered == requested
                }
                _ => false,
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("{0}")]
    InvalidRedirectUri(String),
    #[error("{0}")]
    InvalidMetadata(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

pub struct ClientRegistry {
    pool: SqlitePool,
}

impl ClientRegistry {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn register(
        &self,
        name: Option<&str>,
        redirect_uris: &[String],
    ) -> Result<Client, RegisterError> {
        if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(RegisterError::InvalidRedirectUri(format!(
                "between 1 and {MAX_REDIRECT_URIS} redirect_uris are required"
            )));
        }
        for uri in redirect_uris {
            check_redirect_uri(uri).map_err(RegisterError::InvalidRedirectUri)?;
        }
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("MCP client");
        if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(RegisterError::InvalidMetadata(format!(
                "client_name must be at most {MAX_NAME_CHARS} printable characters"
            )));
        }

        let client = Client {
            id: format!(
                "tgmc_{}",
                URL_SAFE_NO_PAD.encode(crate::sessions::random_bytes::<16>())
            ),
            name: name.to_string(),
            redirect_uris: redirect_uris.to_vec(),
        };
        sqlx::query(
            "INSERT INTO oauth_clients (id, name, redirect_uris, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&client.id)
        .bind(&client.name)
        .bind(serde_json::to_string(&client.redirect_uris).expect("strings serialize"))
        .bind(unix_now())
        .execute(&self.pool)
        .await?;
        tracing::info!(client_id = %client.id, "oauth client registered");
        Ok(client)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Client>, sqlx::Error> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT name, redirect_uris FROM oauth_clients WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(name, uris)| Client {
            id: id.to_string(),
            name,
            redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
        }))
    }
}

/// Allowed: `https://…`, `http://` on a loopback host, and private-use app schemes
/// (`cursor://…`). Never fragments, never script-ish schemes.
fn check_redirect_uri(uri: &str) -> Result<(), String> {
    if uri.len() > MAX_URI_LEN {
        return Err("redirect_uri is too long".into());
    }
    let url = Url::parse(uri).map_err(|_| format!("redirect_uri is not an absolute URI: {uri}"))?;
    if url.fragment().is_some() {
        return Err("redirect_uri must not contain a fragment".into());
    }
    match url.scheme() {
        "https" if url.host().is_some() => Ok(()),
        "http" if is_loopback_http(&url) => Ok(()),
        "http" | "https" => Err(format!(
            "redirect_uri must be https, or http on localhost/127.0.0.1/[::1]: {uri}"
        )),
        scheme if FORBIDDEN_SCHEMES.contains(&scheme) => {
            Err(format!("redirect_uri scheme '{scheme}' is not allowed"))
        }
        _ => Ok(()),
    }
}

fn is_loopback_http(url: &Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_policy() {
        for ok in [
            "http://localhost:33418/callback",
            "http://127.0.0.1:5555/cb",
            "http://[::1]:8080/cb",
            "https://claude.ai/api/mcp/auth_callback",
            "cursor://anysphere.cursor-retrieval/oauth/callback",
        ] {
            assert!(check_redirect_uri(ok).is_ok(), "rejected {ok}");
        }
        for bad in [
            "http://evil.example/cb",
            "http://localhost.evil.example/cb",
            "https://claude.ai/cb#frag",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "/relative/callback",
            "not a uri",
        ] {
            assert!(check_redirect_uri(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn loopback_matches_on_any_port_others_exactly() {
        let client = Client {
            id: "c".into(),
            name: "n".into(),
            redirect_uris: vec![
                "http://127.0.0.1:1234/cb".into(),
                "https://app.example/cb".into(),
            ],
        };
        assert!(client.accepts_redirect("http://127.0.0.1:9999/cb"));
        assert!(!client.accepts_redirect("http://127.0.0.1:9999/other"));
        assert!(client.accepts_redirect("https://app.example/cb"));
        assert!(!client.accepts_redirect("https://app.example:8443/cb"));
        assert!(!client.accepts_redirect("https://app.example/cb?x=1"));
    }
}
