//! The Real world: the Test account and the Peer on real Telegram, and the
//! server under test either started here as a process (with the Acceptance
//! app) or already running elsewhere (`--target`, e.g. production).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use crate::{
    config::AcceptConfig,
    telegram::{Role, TgClient},
    world::{Server, World},
};

/// The server under test's files, when the harness started it.
#[derive(Debug, Clone)]
pub struct ServerFiles {
    /// Everything the server printed (stdout and stderr).
    pub log: PathBuf,
    /// Its SQLite database.
    pub db: PathBuf,
}

/// Functional runs raise these (ADR-0009); the limits scenario uses the defaults.
const RAISED_LIMITS: &[(&str, &str)] = &[
    ("TG_MCP_ACCOUNT_WRITES_PER_10_MIN", "1000"),
    ("TG_MCP_CHAT_SENDS_PER_MINUTE", "100"),
    ("TG_MCP_CONSENT_CHATS_PER_MINUTE", "1000"),
    ("TG_MCP_CONSENT_CHATS_BURST", "1000"),
];

/// How long the first `cargo run` may take to build and start the server.
const START_TIMEOUT: Duration = Duration::from_secs(900);

/// Starts the Real world. With `target`, the server is that URL; otherwise
/// the harness builds and starts it on a free local port.
pub async fn start(
    config: &AcceptConfig,
    target: Option<&str>,
) -> Result<(Arc<World>, Option<ServerFiles>)> {
    let driver = Arc::new(TgClient::open(
        &config.app,
        &config.driver_session,
        Role::Driver,
    )?);
    let peer = Arc::new(TgClient::open(
        &config.app,
        &config.peer_session,
        Role::Peer,
    )?);
    let mut keep: Vec<Box<dyn std::any::Any + Send + Sync>> = Vec::new();
    let (server, files) = match target {
        Some(url) => (
            Server {
                base: url.trim_end_matches('/').to_string(),
            },
            None,
        ),
        None => {
            let started = start_server(config).await?;
            let files = started.files.clone();
            let server = Server {
                base: started.base.clone(),
            };
            keep.push(Box::new(started));
            (server, Some(files))
        }
    };
    let world = World {
        name: "real",
        server,
        driver,
        peer,
        test_account: config.test_account,
        peer_account: config.peer_account,
        cloud_password: Some(Zeroizing::new(config.cloud_password.to_string())),
        poll_every: Duration::from_millis(1500),
        login_timeout: Duration::from_secs(90),
        lease: crate::lease::Terms::REAL,
        _keep: keep,
    };
    Ok((Arc::new(world), files))
}

/// The server process the harness started; killed when dropped.
struct StartedServer {
    base: String,
    files: ServerFiles,
    _child: tokio::process::Child,
    _dir: tempfile::TempDir,
}

/// The workspace root, where `cargo run` finds the server.
fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the acceptance crate lives in the workspace")
}

async fn start_server(config: &AcceptConfig) -> Result<StartedServer> {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };
    let base = format!("http://127.0.0.1:{port}");
    let dir = tempfile::tempdir()?;
    let files = ServerFiles {
        log: dir.path().join("server.log"),
        db: dir.path().join("tg-mcp.db"),
    };
    let log = std::fs::File::create(&files.log)?;
    let mut command =
        tokio::process::Command::new(std::env::var("CARGO").unwrap_or("cargo".into()));
    command
        .current_dir(workspace())
        .args(["run", "--quiet", "--package", "tg-mcp", "--bin", "tg-mcp"])
        .env("TG_MCP_BIND", format!("127.0.0.1:{port}"))
        .env("TG_MCP_PUBLIC_URL", &base)
        .env("TG_MCP_DB_PATH", &files.db)
        .env("TG_MCP_API_ID", config.app.api_id.to_string())
        .env("TG_MCP_API_HASH", config.app.api_hash.as_str())
        .env_remove("TG_MCP_FAKE_TELEGRAM")
        .env_remove("PORT")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    for (key, value) in RAISED_LIMITS {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .context("starting the server with cargo run")?;
    let http = reqwest::Client::new();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "the server exited ({status}) before answering /healthz; its output is in {}",
                files.log.display()
            );
        }
        if let Ok(res) = http.get(format!("{base}/healthz")).send().await
            && res.status().is_success()
        {
            break;
        }
        if started.elapsed() > START_TIMEOUT {
            bail!("the server didn't answer /healthz within {START_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(StartedServer {
        base,
        files,
        _child: child,
        _dir: dir,
    })
}
