//! The Real world: the Test account and the Peer on real Telegram, and the
//! server under test started here (`cargo run`, or a Docker image for release
//! acceptance), always with the Acceptance app, or already running elsewhere
//! (`--target`, e.g. production).

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
    invariants::{LogSource, Observed},
    telegram::{Role, TgClient},
    world::{Server, World},
};

/// The server under test's files, when the harness started it.
#[derive(Debug, Clone)]
struct ServerFiles {
    /// Everything the server printed (stdout and stderr).
    log: PathBuf,
    /// Its SQLite database.
    db: PathBuf,
}

/// Functional runs raise these (ADR-0009); the limits scenario uses the defaults.
const RAISED_LIMITS: &[(&str, &str)] = &[
    ("TG_MCP_ACCOUNT_WRITES_PER_10_MIN", "1000"),
    ("TG_MCP_CHAT_SENDS_PER_MINUTE", "100"),
    ("TG_MCP_CONSENT_CHATS_PER_MINUTE", "1000"),
    ("TG_MCP_CONSENT_CHATS_BURST", "1000"),
];

/// How long the first `cargo run` (or a container) may take to start.
const START_TIMEOUT: Duration = Duration::from_secs(900);

/// Where the server under test comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUnderTest {
    /// `cargo run` from the working copy: the quick loop while building a feature.
    CargoRun,
    /// A Docker image: a published one (`ghcr.io/qelphybox/tg-mcp:<sha>`, the
    /// release candidate) or [`LOCAL_IMAGE`], built from the working copy.
    Image(String),
    /// A server already running at this URL (e.g. production, for the smoke).
    Target(String),
}

/// `--image local`: build the working copy's image first.
pub const LOCAL_IMAGE: &str = "local";
/// The tag `--image local` builds.
const LOCAL_TAG: &str = "tg-mcp:accept";

impl ServerUnderTest {
    /// How the report names it.
    fn label(&self) -> String {
        match self {
            ServerUnderTest::CargoRun => "real world, cargo run".into(),
            ServerUnderTest::Image(i) if i == LOCAL_IMAGE => {
                format!("real world, image {LOCAL_TAG} built here")
            }
            ServerUnderTest::Image(i) => format!("real world, image {i}"),
            ServerUnderTest::Target(url) => format!("real world, {url}"),
        }
    }
}

/// Starts the Real world with its server under test.
pub async fn start(config: &AcceptConfig, sut: &ServerUnderTest) -> Result<Arc<World>> {
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
    let account_names = driver.names().await?;
    let mut keep: Vec<Box<dyn std::any::Any + Send + Sync>> = Vec::new();
    let (server, files) = match sut {
        ServerUnderTest::Target(url) => (
            Server {
                base: url.trim_end_matches('/').to_string(),
            },
            None,
        ),
        ServerUnderTest::CargoRun => {
            let started = start_server(config).await?;
            let files = started.files.clone();
            let server = Server {
                base: started.base.clone(),
            };
            keep.push(Box::new(started));
            (server, Some(files))
        }
        ServerUnderTest::Image(image) => {
            let started = start_container(config, image).await?;
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
        label: sut.label(),
        server,
        driver,
        peer,
        test_account: config.test_account,
        peer_account: config.peer_account,
        cloud_password: Some(Zeroizing::new(config.cloud_password.to_string())),
        poll_every: Duration::from_millis(1500),
        login_timeout: Duration::from_secs(90),
        lease: crate::lease::Terms::REAL,
        observed: files.map(|f| Observed {
            log: LogSource::File(f.log),
            db: f.db,
        }),
        account_names,
        _keep: keep,
    };
    Ok(Arc::new(world))
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
    let port = free_port()?;
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
    wait_healthy(&base, &files.log, || Ok(child.try_wait()?.is_none())).await?;
    Ok(StartedServer {
        base,
        files,
        _child: child,
        _dir: dir,
    })
}

/// A free port on 127.0.0.1.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Waits until `base` answers `/healthz`, or `alive` says the server died.
async fn wait_healthy(
    base: &str,
    log: &Path,
    mut alive: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let http = reqwest::Client::new();
    let started = Instant::now();
    loop {
        if !alive()? {
            bail!(
                "the server stopped before answering /healthz; its output is in {}",
                log.display()
            );
        }
        if let Ok(res) = http.get(format!("{base}/healthz")).send().await
            && res.status().is_success()
        {
            return Ok(());
        }
        if started.elapsed() > START_TIMEOUT {
            bail!("the server didn't answer /healthz within {START_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── The server as a container (release acceptance) ─────────────────────────

/// The `docker run` arguments: the port on 127.0.0.1 only, `/data` on the
/// run's directory, and each variable by name only (`-e NAME`): docker reads
/// its value from its own environment, so no secret shows in the process list.
/// No Built-in app is involved: the Acceptance app is passed explicitly, as
/// production passes its own.
fn docker_run_args(name: &str, image: &str, port: u16, data: &Path, env: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        name.into(),
        "--publish".into(),
        format!("127.0.0.1:{port}:8080"),
        "--volume".into(),
        format!("{}:/data", data.display()),
    ];
    for key in env {
        args.push("--env".into());
        args.push((*key).into());
    }
    args.push(image.into());
    args
}

/// The variables the container gets, with their values (set on the docker
/// process, passed by name).
fn container_env(config: &AcceptConfig, base: &str) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("TG_MCP_PUBLIC_URL", base.to_string()),
        ("TG_MCP_API_ID", config.app.api_id.to_string()),
        ("TG_MCP_API_HASH", config.app.api_hash.to_string()),
    ];
    env.extend(RAISED_LIMITS.iter().map(|(k, v)| (*k, v.to_string())));
    env
}

fn docker() -> std::process::Command {
    std::process::Command::new("docker")
}

/// Runs `docker` with `args`; fails with its own error output.
fn docker_ok(args: &[&str], what: &str) -> Result<()> {
    let out = docker()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("{what}: is Docker installed?"))?;
    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The container the harness started; removed when dropped.
struct StartedContainer {
    base: String,
    files: ServerFiles,
    name: String,
    _logs: tokio::process::Child,
    _dir: tempfile::TempDir,
}

impl Drop for StartedContainer {
    fn drop(&mut self) {
        let _ = docker()
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn start_container(config: &AcceptConfig, image: &str) -> Result<StartedContainer> {
    docker_ok(
        &["version", "--format", "{{.Server.Version}}"],
        "reaching Docker (start it, or run without --image)",
    )?;
    let image = if image == LOCAL_IMAGE {
        eprintln!("building {LOCAL_TAG} from the working copy (a few minutes the first time)…");
        let status = docker()
            .current_dir(workspace())
            .args(["build", "--tag", LOCAL_TAG, "."])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .context("docker build")?;
        if !status.success() {
            bail!("docker build of the working copy failed ({status})");
        }
        LOCAL_TAG.to_string()
    } else {
        docker_ok(
            &["pull", "--quiet", image],
            &format!(
                "pulling {image} (is it published? a private package needs `docker login ghcr.io`)"
            ),
        )?;
        image.to_string()
    };

    let port = free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("data");
    std::fs::create_dir(&data)?;
    #[cfg(unix)]
    {
        // The image runs as an unprivileged user that must write the database.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o777))?;
    }
    let files = ServerFiles {
        log: dir.path().join("server.log"),
        db: data.join("tg-mcp.db"),
    };
    let name = format!("tgmcp-accept-{}", crate::run::new_run_id());
    let env = container_env(config, &base);
    let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
    let args = docker_run_args(&name, &image, port, &data, &keys);
    let out = docker()
        .args(&args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .output()
        .context("docker run")?;
    if !out.status.success() {
        bail!(
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let log = std::fs::File::create(&files.log)?;
    let logs = tokio::process::Command::new("docker")
        .args(["logs", "--follow", &name])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .context("docker logs")?;
    let started = StartedContainer {
        base: base.clone(),
        files,
        name: name.clone(),
        _logs: logs,
        _dir: dir,
    };
    wait_healthy(&base, &started.files.log, || {
        let out = docker()
            .args(["inspect", "--format", "{{.State.Running}}", &name])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "true")
    })
    .await?;
    Ok(started)
}

/// `cargo accept fixtures`: creates and repairs the fixtures on the Test
/// account, under the lease, after the same checks a run makes on the Driver
/// and the Peer. Returns what it did.
pub async fn fixtures(config: &AcceptConfig) -> Result<Vec<String>> {
    let driver = Arc::new(TgClient::open(
        &config.app,
        &config.driver_session,
        Role::Driver,
    )?);
    let peer = TgClient::open(&config.app, &config.peer_session, Role::Peer)?;
    let (d, p) = (driver.me().await?, peer.me().await?);
    if d != config.test_account || p != config.peer_account || d == p {
        bail!(
            "interlock: the Driver is account {d} and the Peer {p}, configured: Test account {}, Peer {}; nothing was written",
            config.test_account,
            config.peer_account
        );
    }
    let run_id = crate::run::new_run_id();
    let held = crate::lease::take(driver.clone(), &run_id, crate::lease::Terms::REAL).await?;
    let done = crate::fixtures::ensure(&driver, &peer).await;
    let released = held.release().await;
    let done = done?;
    released?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_listens_on_localhost_only_and_secrets_go_by_name() {
        let args = docker_run_args(
            "tgmcp-accept-x",
            "ghcr.io/qelphybox/tg-mcp:abc",
            18080,
            Path::new("/tmp/run/data"),
            &["TG_MCP_PUBLIC_URL", "TG_MCP_API_ID", "TG_MCP_API_HASH"],
        );
        let line = args.join(" ");
        assert!(line.contains("--publish 127.0.0.1:18080:8080"), "{line}");
        assert!(line.contains("--volume /tmp/run/data:/data"), "{line}");
        assert!(line.contains("--env TG_MCP_API_HASH"), "{line}");
        assert!(!line.contains('='), "no value on the command line: {line}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("ghcr.io/qelphybox/tg-mcp:abc")
        );
        assert!(!line.contains("BUILTIN"), "{line}");
    }

    #[test]
    fn the_container_gets_the_acceptance_app_and_raised_limits() {
        let config = AcceptConfig {
            app: crate::config::App {
                api_id: 123,
                api_hash: Zeroizing::new("h".into()),
            },
            test_account: 1,
            peer_account: 2,
            cloud_password: Zeroizing::new("p".into()),
            driver_session: Zeroizing::new(Vec::new()),
            peer_session: Zeroizing::new(Vec::new()),
        };
        let env = container_env(&config, "http://127.0.0.1:1");
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("TG_MCP_API_ID"), Some("123"));
        assert_eq!(get("TG_MCP_PUBLIC_URL"), Some("http://127.0.0.1:1"));
        assert_eq!(get("TG_MCP_ACCOUNT_WRITES_PER_10_MIN"), Some("1000"));
        assert_eq!(get("TG_MCP_FAKE_TELEGRAM"), None);
    }
}
