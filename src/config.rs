//! Configuration from environment variables. Every variable is documented in `.env.example`.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use axum::http::Uri;

use crate::tg::grammers::AppCredentials;

#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Public base URL clients reach us at (OAuth metadata and Host validation derive from it).
    pub public_url: Uri,
    /// Serve a fixed fake Account instead of Telegram. Development only.
    pub fake_telegram: bool,
    /// SQLite database file of the Session vault.
    pub db_path: PathBuf,
    /// Our Telegram app (my.telegram.org). Required unless `fake_telegram`.
    pub telegram_app: Option<AppCredentials>,
    /// How long an unused Live client stays connected.
    pub live_client_idle_ttl: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`Config::from_env`], with an injectable variable source for tests.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let bind = get("TG_MCP_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .context("TG_MCP_BIND must be host:port")?;

        let public_url: Uri = get("TG_MCP_PUBLIC_URL")
            .unwrap_or_else(|| "http://localhost:8080".into())
            .parse()
            .context("TG_MCP_PUBLIC_URL must be a URL")?;
        if public_url.scheme().is_none() || public_url.authority().is_none() {
            bail!("TG_MCP_PUBLIC_URL must be absolute, e.g. https://tg-mcp.example.com");
        }

        let fake_telegram = matches!(get("TG_MCP_FAKE_TELEGRAM").as_deref(), Some("1" | "true"));
        let db_path = get("TG_MCP_DB_PATH")
            .unwrap_or_else(|| "tg-mcp.db".into())
            .into();

        let telegram_app = match (get("TG_MCP_API_ID"), get("TG_MCP_API_HASH")) {
            (Some(id), Some(hash)) if !id.is_empty() && !hash.is_empty() => Some(AppCredentials {
                api_id: id
                    .trim()
                    .parse()
                    .context("TG_MCP_API_ID must be a number")?,
                api_hash: hash.trim().to_string(),
            }),
            _ if fake_telegram => None,
            _ => bail!(
                "TG_MCP_API_ID and TG_MCP_API_HASH are required (or TG_MCP_FAKE_TELEGRAM=1 for development)"
            ),
        };
        let live_client_idle_ttl = Duration::from_secs(
            get("TG_MCP_LIVE_CLIENT_IDLE_SECS")
                .map(|v| v.parse())
                .transpose()
                .context("TG_MCP_LIVE_CLIENT_IDLE_SECS must be a number of seconds")?
                .unwrap_or(600),
        );

        Ok(Self {
            bind,
            public_url,
            fake_telegram,
            db_path,
            telegram_app,
            live_client_idle_ttl,
        })
    }

    /// `scheme://host[:port]` of the public URL: the OAuth issuer and resource base.
    pub fn base_url(&self) -> String {
        let scheme = self.public_url.scheme_str().unwrap_or("https");
        let authority = self
            .public_url
            .authority()
            .map(|a| a.as_str())
            .unwrap_or("localhost");
        format!("{scheme}://{authority}")
    }

    /// Host header values the MCP endpoint accepts (DNS-rebinding protection).
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
            .into_iter()
            .map(String::from)
            .collect();
        if let Some(authority) = self.public_url.authority() {
            hosts.push(authority.host().to_string());
            hosts.push(authority.as_str().to_string());
        }
        hosts.dedup();
        hosts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn telegram_credentials_are_required_unless_faked() {
        assert!(Config::from_lookup(env(&[])).is_err());
        let config = Config::from_lookup(env(&[
            ("TG_MCP_API_ID", "12345"),
            ("TG_MCP_API_HASH", "abcdef"),
        ]))
        .unwrap();
        assert_eq!(config.telegram_app.unwrap().api_id, 12345);
        assert!(
            Config::from_lookup(env(&[("TG_MCP_API_ID", "x"), ("TG_MCP_API_HASH", "y")])).is_err()
        );
    }

    #[test]
    fn defaults_are_local() {
        let config = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1")])).unwrap();
        assert_eq!(config.bind.to_string(), "0.0.0.0:8080");
        assert_eq!(config.public_url.to_string(), "http://localhost:8080/");
        assert!(config.fake_telegram);
        assert_eq!(config.db_path, PathBuf::from("tg-mcp.db"));
    }

    #[test]
    fn public_host_is_allowed() {
        let config = Config::from_lookup(env(&[
            ("TG_MCP_PUBLIC_URL", "https://tg.example.com"),
            ("TG_MCP_FAKE_TELEGRAM", "1"),
        ]))
        .unwrap();
        assert!(
            config
                .allowed_hosts()
                .contains(&"tg.example.com".to_string())
        );
    }

    #[test]
    fn relative_public_url_is_rejected() {
        assert!(
            Config::from_lookup(env(&[
                ("TG_MCP_PUBLIC_URL", "/mcp"),
                ("TG_MCP_FAKE_TELEGRAM", "1")
            ]))
            .is_err()
        );
    }
}
