//! Configuration from environment variables. Every variable is documented in `.env.example`.

use std::{fmt, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use axum::http::Uri;

use crate::{
    auth::rate_limit::{OAuthLimits, Rate},
    limits::LimitsConfig,
    tg::grammers::AppCredentials,
};

#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Public base URL clients reach us at (OAuth metadata and Host validation derive from it).
    pub public_url: Uri,
    /// Where [`Config::public_url`] came from, for the startup log.
    pub public_url_source: PublicUrlSource,
    /// Serve a fixed fake Account instead of Telegram. Development only.
    pub fake_telegram: bool,
    /// SQLite database file of the Session vault.
    pub db_path: PathBuf,
    /// Our Telegram app (my.telegram.org). Required unless `fake_telegram`.
    pub telegram_app: Option<AppCredentials>,
    /// How long an unused Live client stays connected.
    pub live_client_idle_ttl: Duration,
    /// Per-Account request budget and FLOOD_WAIT handling.
    pub limits: LimitsConfig,
    /// Per-client limits on the anonymous OAuth endpoints and on Login flows.
    pub oauth_limits: OAuthLimits,
    /// Experimental, off by default (#58 prototype): get_media takes mode
    /// `document` and lists it in its schema and description.
    pub experimental_documents: bool,
}

/// Where the public URL came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicUrlSource {
    /// `TG_MCP_PUBLIC_URL`.
    Explicit,
    /// A hosting platform's own variable.
    Platform(Platform),
    /// `http://localhost:8080`.
    Default,
}

impl fmt::Display for PublicUrlSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Explicit => f.write_str("TG_MCP_PUBLIC_URL"),
            Self::Platform(platform) => write!(f, "{} ({})", platform.name(), platform.variable()),
            Self::Default => {
                f.write_str("default (TG_MCP_PUBLIC_URL unset, no hosting platform detected)")
            }
        }
    }
}

/// Hosting platforms whose public domain is detected from the variable they set
/// in every running service. A detected domain is always served over HTTPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// <https://docs.railway.com/reference/variables>: `RAILWAY_PUBLIC_DOMAIN`,
    /// "of the form `example.up.railway.app`".
    Railway,
    /// <https://render.com/docs/environment-variables>: `RENDER_EXTERNAL_HOSTNAME`,
    /// "such as `myapp.onrender.com`" (empty for services that aren't web services).
    Render,
    /// <https://docs.fly.io/machines/runtime-environment>: `FLY_APP_NAME`; an app
    /// "is automatically given a `fly.dev` subdomain, based on the app's name"
    /// (<https://docs.fly.io/networking/custom-domain/>).
    Fly,
    /// <https://www.koyeb.com/docs/build-and-deploy/environment-variables>:
    /// `KOYEB_PUBLIC_DOMAIN`, the "default public Service URL" as a bare domain.
    Koyeb,
}

impl Platform {
    /// In detection order.
    const ALL: [Self; 4] = [Self::Railway, Self::Render, Self::Fly, Self::Koyeb];

    fn name(self) -> &'static str {
        match self {
            Self::Railway => "Railway",
            Self::Render => "Render",
            Self::Fly => "Fly.io",
            Self::Koyeb => "Koyeb",
        }
    }

    fn variable(self) -> &'static str {
        match self {
            Self::Railway => "RAILWAY_PUBLIC_DOMAIN",
            Self::Render => "RENDER_EXTERNAL_HOSTNAME",
            Self::Fly => "FLY_APP_NAME",
            Self::Koyeb => "KOYEB_PUBLIC_DOMAIN",
        }
    }

    /// The public host, from the platform variable's value.
    fn host(self, value: &str) -> String {
        match self {
            Self::Fly => format!("{value}.fly.dev"),
            Self::Railway | Self::Render | Self::Koyeb => value.to_string(),
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`Config::from_env`], with an injectable variable source for tests.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        // Set but empty is unset, for the variables a platform or a compose file fills in.
        let get_set = |key: &str| get(key).filter(|v| !v.trim().is_empty());

        let bind = match (get("TG_MCP_BIND"), get_set("PORT")) {
            (Some(bind), _) => bind.parse().context("TG_MCP_BIND must be host:port")?,
            (None, Some(port)) => {
                let port: u16 =
                    port.trim()
                        .parse()
                        .ok()
                        .filter(|&p| p != 0)
                        .with_context(|| {
                            format!("PORT must be a port number (1-65535), not {port:?}")
                        })?;
                SocketAddr::from(([0, 0, 0, 0], port))
            }
            (None, None) => SocketAddr::from(([0, 0, 0, 0], 8080)),
        };

        let detected = Platform::ALL
            .into_iter()
            .find_map(|platform| get_set(platform.variable()).map(|value| (platform, value)));
        let (public_url, public_url_source): (Uri, _) = match (get("TG_MCP_PUBLIC_URL"), detected) {
            (Some(url), _) => (
                url.parse().context("TG_MCP_PUBLIC_URL must be a URL")?,
                PublicUrlSource::Explicit,
            ),
            (None, Some((platform, value))) => {
                let host = platform.host(value.trim());
                let url: Uri = format!("https://{host}")
                    .parse()
                    .ok()
                    .filter(|url: &Uri| {
                        url.path() == "/" && host.parse::<http::uri::Authority>().is_ok()
                    })
                    .with_context(|| {
                        format!(
                            "{} must be a domain name, not {value:?}",
                            platform.variable()
                        )
                    })?;
                (url, PublicUrlSource::Platform(platform))
            }
            (None, None) => (
                Uri::from_static("http://localhost:8080"),
                PublicUrlSource::Default,
            ),
        };
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

        let number = |key: &str, default: u32| -> anyhow::Result<u32> {
            get(key)
                .map(|v| v.trim().parse::<u32>())
                .transpose()
                .with_context(|| format!("{key} must be a whole number"))?
                .map_or(Ok(default), |v| {
                    if v == 0 {
                        bail!("{key} must be at least 1")
                    } else {
                        Ok(v)
                    }
                })
        };
        let defaults = LimitsConfig::default();
        let limits = LimitsConfig {
            per_minute: number("TG_MCP_ACCOUNT_REQUESTS_PER_MINUTE", defaults.per_minute)?,
            burst: number("TG_MCP_ACCOUNT_BURST", defaults.burst)?,
            max_wait: Duration::from_secs(
                number("TG_MCP_MAX_WAIT_SECS", defaults.max_wait.as_secs() as u32)?.into(),
            ),
            writes_per_10_min: number(
                "TG_MCP_ACCOUNT_WRITES_PER_10_MIN",
                defaults.writes_per_10_min,
            )?,
            sends_per_chat_per_minute: number(
                "TG_MCP_CHAT_SENDS_PER_MINUTE",
                defaults.sends_per_chat_per_minute,
            )?,
        };

        let defaults = OAuthLimits::default();
        let rate = |name: &str, default: Rate| -> anyhow::Result<Rate> {
            Ok(Rate {
                per_minute: number(&format!("TG_MCP_{name}_PER_MINUTE"), default.per_minute)?,
                burst: number(&format!("TG_MCP_{name}_BURST"), default.burst)?,
            })
        };
        let oauth_limits = OAuthLimits {
            trust_forwarded_for: matches!(
                get("TG_MCP_TRUST_FORWARDED_FOR").as_deref(),
                Some("1" | "true")
            ),
            authorize: rate("AUTHORIZE", defaults.authorize)?,
            register: rate("REGISTER", defaults.register)?,
            token: rate("TOKEN", defaults.token)?,
            media: rate("MEDIA", defaults.media)?,
            consent_chats: rate("CONSENT_CHATS", defaults.consent_chats)?,
            max_pending_logins: number(
                "TG_MCP_MAX_PENDING_LOGINS",
                defaults.max_pending_logins as u32,
            )? as usize,
            max_pending_logins_per_client: number(
                "TG_MCP_MAX_PENDING_LOGINS_PER_IP",
                defaults.max_pending_logins_per_client as u32,
            )? as usize,
        };

        let experimental_documents = matches!(
            get("TG_MCP_EXPERIMENTAL_DOCUMENTS").as_deref(),
            Some("1" | "true")
        );

        Ok(Self {
            bind,
            public_url,
            public_url_source,
            fake_telegram,
            db_path,
            telegram_app,
            live_client_idle_ttl,
            limits,
            oauth_limits,
            experimental_documents,
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
    fn limits_come_from_env_and_reject_zero() {
        let config = Config::from_lookup(env(&[
            ("TG_MCP_FAKE_TELEGRAM", "1"),
            ("TG_MCP_ACCOUNT_REQUESTS_PER_MINUTE", "30"),
        ]))
        .unwrap();
        assert_eq!(config.limits.per_minute, 30);
        assert_eq!(config.limits.burst, LimitsConfig::default().burst);
        assert!(
            Config::from_lookup(env(&[
                ("TG_MCP_FAKE_TELEGRAM", "1"),
                ("TG_MCP_ACCOUNT_BURST", "0")
            ]))
            .is_err()
        );
    }

    #[test]
    fn write_limits_default_to_adr_0005_and_come_from_env() {
        let limits = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1")]))
            .unwrap()
            .limits;
        assert_eq!(
            (limits.writes_per_10_min, limits.sends_per_chat_per_minute),
            (20, 5)
        );
        let limits = Config::from_lookup(env(&[
            ("TG_MCP_FAKE_TELEGRAM", "1"),
            ("TG_MCP_ACCOUNT_WRITES_PER_10_MIN", "40"),
            ("TG_MCP_CHAT_SENDS_PER_MINUTE", "2"),
        ]))
        .unwrap()
        .limits;
        assert_eq!(
            (limits.writes_per_10_min, limits.sends_per_chat_per_minute),
            (40, 2)
        );
        for key in [
            "TG_MCP_ACCOUNT_WRITES_PER_10_MIN",
            "TG_MCP_CHAT_SENDS_PER_MINUTE",
        ] {
            for bad in ["0", "-1", "five"] {
                assert!(
                    Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1"), (key, bad)])).is_err(),
                    "{key}={bad}"
                );
            }
        }
    }

    #[test]
    fn oauth_limits_default_to_the_brief_and_come_from_env() {
        let config = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1")])).unwrap();
        let limits = config.oauth_limits;
        assert!(!limits.trust_forwarded_for);
        assert_eq!(
            (limits.authorize.per_minute, limits.authorize.burst),
            (10, 5)
        );
        assert_eq!((limits.register.per_minute, limits.register.burst), (5, 5));
        assert_eq!((limits.token.per_minute, limits.token.burst), (30, 10));
        assert_eq!((limits.media.per_minute, limits.media.burst), (30, 10));
        assert_eq!(limits.max_pending_logins, 50);
        assert_eq!(limits.max_pending_logins_per_client, 3);

        let config = Config::from_lookup(env(&[
            ("TG_MCP_FAKE_TELEGRAM", "1"),
            ("TG_MCP_TRUST_FORWARDED_FOR", "1"),
            ("TG_MCP_AUTHORIZE_PER_MINUTE", "20"),
            ("TG_MCP_TOKEN_BURST", "3"),
            ("TG_MCP_MEDIA_PER_MINUTE", "12"),
            ("TG_MCP_MAX_PENDING_LOGINS", "7"),
            ("TG_MCP_MAX_PENDING_LOGINS_PER_IP", "2"),
        ]))
        .unwrap();
        let limits = config.oauth_limits;
        assert!(limits.trust_forwarded_for);
        assert_eq!(limits.authorize.per_minute, 20);
        assert_eq!(limits.token.burst, 3);
        assert_eq!(limits.media.per_minute, 12);
        assert_eq!(limits.max_pending_logins, 7);
        assert_eq!(limits.max_pending_logins_per_client, 2);
        assert!(
            Config::from_lookup(env(&[
                ("TG_MCP_FAKE_TELEGRAM", "1"),
                ("TG_MCP_MAX_PENDING_LOGINS_PER_IP", "0")
            ]))
            .is_err()
        );
    }

    #[test]
    fn defaults_are_local() {
        let config = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1")])).unwrap();
        assert_eq!(config.bind.to_string(), "0.0.0.0:8080");
        assert_eq!(config.public_url.to_string(), "http://localhost:8080/");
        assert!(config.fake_telegram);
        assert_eq!(config.db_path, PathBuf::from("tg-mcp.db"));
        assert!(!config.experimental_documents);
    }

    #[test]
    fn experimental_documents_are_off_unless_set() {
        let on = |value: &str| {
            Config::from_lookup(env(&[
                ("TG_MCP_FAKE_TELEGRAM", "1"),
                ("TG_MCP_EXPERIMENTAL_DOCUMENTS", value),
            ]))
            .unwrap()
            .experimental_documents
        };
        assert!(on("1"));
        assert!(on("true"));
        assert!(!on("0"));
        assert!(!on(""));
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

    #[test]
    fn bind_comes_from_tg_mcp_bind_then_port_then_the_default() {
        let bind = |pairs: &[(&str, &str)]| {
            let mut all = vec![("TG_MCP_FAKE_TELEGRAM", "1")];
            all.extend_from_slice(pairs);
            Config::from_lookup(env(&all)).unwrap().bind.to_string()
        };
        assert_eq!(bind(&[]), "0.0.0.0:8080");
        assert_eq!(bind(&[("PORT", "9000")]), "0.0.0.0:9000");
        assert_eq!(bind(&[("PORT", "")]), "0.0.0.0:8080");
        assert_eq!(
            bind(&[("TG_MCP_BIND", "127.0.0.1:7000"), ("PORT", "9000")]),
            "127.0.0.1:7000"
        );
    }

    #[test]
    fn a_port_that_is_not_a_port_number_is_rejected() {
        for bad in ["http", "0", "65536", "-1", "80 80"] {
            let err = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1"), ("PORT", bad)]))
                .unwrap_err();
            assert!(format!("{err:#}").contains("PORT"), "PORT={bad}: {err:#}");
        }
    }

    #[test]
    fn the_public_url_is_detected_on_each_platform() {
        let cases = [
            (
                "RAILWAY_PUBLIC_DOMAIN",
                "tg-mcp-production.up.railway.app",
                "https://tg-mcp-production.up.railway.app",
                Platform::Railway,
            ),
            (
                "RENDER_EXTERNAL_HOSTNAME",
                "tg-mcp.onrender.com",
                "https://tg-mcp.onrender.com",
                Platform::Render,
            ),
            (
                "FLY_APP_NAME",
                "tg-mcp-demo",
                "https://tg-mcp-demo.fly.dev",
                Platform::Fly,
            ),
            (
                "KOYEB_PUBLIC_DOMAIN",
                "tg-mcp-me.koyeb.app",
                "https://tg-mcp-me.koyeb.app",
                Platform::Koyeb,
            ),
        ];
        for (key, value, url, platform) in cases {
            let config =
                Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1"), (key, value)])).unwrap();
            assert_eq!(config.base_url(), url, "{key}");
            assert_eq!(
                config.public_url_source,
                PublicUrlSource::Platform(platform),
                "{key}"
            );
            let host = url.trim_start_matches("https://").to_string();
            assert!(config.allowed_hosts().contains(&host), "{key}");

            let config = Config::from_lookup(env(&[
                ("TG_MCP_FAKE_TELEGRAM", "1"),
                ("TG_MCP_PUBLIC_URL", "https://tg.example.com"),
                (key, value),
            ]))
            .unwrap();
            assert_eq!(config.base_url(), "https://tg.example.com", "{key}");
            assert_eq!(config.public_url_source, PublicUrlSource::Explicit);
            assert!(!config.allowed_hosts().contains(&host), "{key}");

            let config =
                Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1"), (key, "")])).unwrap();
            assert_eq!(config.public_url_source, PublicUrlSource::Default, "{key}");
        }
    }

    #[test]
    fn without_any_the_public_url_is_localhost() {
        let config = Config::from_lookup(env(&[("TG_MCP_FAKE_TELEGRAM", "1")])).unwrap();
        assert_eq!(config.base_url(), "http://localhost:8080");
        assert_eq!(config.public_url_source, PublicUrlSource::Default);
        assert_eq!(
            PublicUrlSource::Default.to_string(),
            "default (TG_MCP_PUBLIC_URL unset, no hosting platform detected)"
        );
        assert_eq!(PublicUrlSource::Explicit.to_string(), "TG_MCP_PUBLIC_URL");
        assert_eq!(
            PublicUrlSource::Platform(Platform::Fly).to_string(),
            "Fly.io (FLY_APP_NAME)"
        );
    }

    #[test]
    fn a_platform_domain_that_is_not_a_host_is_rejected() {
        let err = Config::from_lookup(env(&[
            ("TG_MCP_FAKE_TELEGRAM", "1"),
            ("RAILWAY_PUBLIC_DOMAIN", "https://x.up.railway.app/path"),
        ]))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RAILWAY_PUBLIC_DOMAIN"),
            "{err:#}"
        );
    }
}
