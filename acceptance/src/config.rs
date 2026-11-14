//! The Real world's configuration: the Acceptance app, the Test account and
//! the Peer, their logins and the Cloud password (ADR-0009).
//!
//! Each `TG_ACCEPT_*` value comes from the environment (CI: the GitHub
//! environment `acceptance`), else from a local file outside the repository,
//! readable only by its owner. Values are never printed.

use std::{collections::BTreeMap, fmt, fs, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

pub const API_ID: &str = "TG_ACCEPT_API_ID";
pub const API_HASH: &str = "TG_ACCEPT_API_HASH";
pub const ACCOUNT_ID: &str = "TG_ACCEPT_ACCOUNT_ID";
pub const PEER_ID: &str = "TG_ACCEPT_PEER_ID";
pub const CLOUD_PASSWORD: &str = "TG_ACCEPT_CLOUD_PASSWORD";
pub const DRIVER_SESSION: &str = "TG_ACCEPT_DRIVER_SESSION";
pub const PEER_SESSION: &str = "TG_ACCEPT_PEER_SESSION";

/// Every key, in the order the setup wizard asks for them.
pub const KEYS: &[&str] = &[
    API_ID,
    API_HASH,
    ACCOUNT_ID,
    PEER_ID,
    CLOUD_PASSWORD,
    DRIVER_SESSION,
    PEER_SESSION,
];

/// The local file: `$TG_ACCEPT_FILE`, else `$XDG_CONFIG_HOME/tg-mcp-accept/env`,
/// else `~/.config/tg-mcp-accept/env`.
pub fn file_path() -> Result<PathBuf> {
    if let Some(path) = non_empty_env("TG_ACCEPT_FILE") {
        return Ok(PathBuf::from(path));
    }
    let base = match non_empty_env("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(non_empty_env("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("tg-mcp-accept").join("env"))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `KEY=VALUE` lines; `#` starts a comment line.
fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn read_file() -> Result<BTreeMap<String, String>> {
    let path = file_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A value: the environment's, else the local file's.
pub fn get(key: &str) -> Result<Option<Zeroizing<String>>> {
    if let Some(v) = non_empty_env(key) {
        return Ok(Some(Zeroizing::new(v)));
    }
    Ok(read_file()?
        .remove(key)
        .filter(|v| !v.is_empty())
        .map(Zeroizing::new))
}

/// Writes `key` into the local file, creating it (and its directory) readable
/// only by the owner.
pub fn set_in_file(key: &str, value: &str) -> Result<PathBuf> {
    let path = file_path()?;
    let mut values = read_file()?;
    values.insert(key.to_string(), value.to_string());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut text = Zeroizing::new(String::from(
        "# tg-mcp acceptance (ADR-0009): the Test account's and the Peer's logins.\n\
         # Secrets: keep this file to yourself (mode 600), never commit it.\n",
    ));
    for (k, v) in &values {
        text.push_str(&format!("{k}={v}\n"));
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(text.as_bytes())?;
    Ok(path)
}

/// The Acceptance app.
#[derive(Clone)]
pub struct App {
    pub api_id: i32,
    pub api_hash: Zeroizing<String>,
}

impl App {
    pub fn load() -> Result<Self> {
        let api_id = required(API_ID)?;
        let api_id = api_id
            .parse()
            .with_context(|| format!("{API_ID} must be a number"))?;
        Ok(Self {
            api_id,
            api_hash: required(API_HASH)?,
        })
    }
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("api_id", &self.api_id)
            .finish_non_exhaustive()
    }
}

/// Everything the Real world needs.
pub struct AcceptConfig {
    pub app: App,
    pub test_account: i64,
    pub peer_account: i64,
    pub cloud_password: Zeroizing<String>,
    pub driver_session: Zeroizing<Vec<u8>>,
    pub peer_session: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for AcceptConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptConfig")
            .field("app", &self.app)
            .field("test_account", &self.test_account)
            .field("peer_account", &self.peer_account)
            .finish_non_exhaustive()
    }
}

impl AcceptConfig {
    pub fn load() -> Result<Self> {
        let id = |key| -> Result<i64> {
            required(key)?
                .parse()
                .with_context(|| format!("{key} must be a Telegram user id"))
        };
        let session = |key| -> Result<Zeroizing<Vec<u8>>> {
            let b64 = required(key)?;
            STANDARD
                .decode(b64.as_bytes())
                .map(Zeroizing::new)
                .with_context(|| format!("{key} is not base64"))
        };
        Ok(Self {
            app: App::load()?,
            test_account: id(ACCOUNT_ID)?,
            peer_account: id(PEER_ID)?,
            cloud_password: required(CLOUD_PASSWORD)?,
            driver_session: session(DRIVER_SESSION)?,
            peer_session: session(PEER_SESSION)?,
        })
    }
}

fn required(key: &str) -> Result<Zeroizing<String>> {
    match get(key)? {
        Some(v) => Ok(v),
        None => bail!(
            "{key} is not set (environment, or {}); scripts/accept-setup.sh sets everything up",
            file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_key_value_lines() {
        let values = parse("# comment\n\nA=1\n B = two \nbroken line\nC=x=y\n");
        assert_eq!(values.get("A").map(String::as_str), Some("1"));
        assert_eq!(values.get("B").map(String::as_str), Some("two"));
        assert_eq!(values.get("C").map(String::as_str), Some("x=y"));
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn debug_shows_no_secret() {
        let config = AcceptConfig {
            app: App {
                api_id: 1,
                api_hash: Zeroizing::new("hash-secret".into()),
            },
            test_account: 2,
            peer_account: 3,
            cloud_password: Zeroizing::new("pw-secret".into()),
            driver_session: Zeroizing::new(b"session-secret".to_vec()),
            peer_session: Zeroizing::new(b"session-secret".to_vec()),
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("test_account: 2"), "{shown}");
    }
}
