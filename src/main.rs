use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, bail};

use tg_mcp::{
    Deps,
    auth::clients::ClientRegistry,
    config::Config,
    db,
    limits::Limits,
    sessions::{TokenTtl, Vault},
    tg::{
        self, Connector, FakeConnector, FakeLogin, Login,
        grammers::{GrammersConnector, GrammersLogin},
    },
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rmcp logs JSON-RPC payloads (= message content) at debug/trace. Cap it whatever RUST_LOG says.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("rmcp=info".parse()?);
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = Config::from_env()?;

    let pool = db::open(&config.db_path)
        .await
        .with_context(|| format!("opening the database at {}", config.db_path.display()))?;
    tracing::info!(db = %config.db_path.display(), "database ready");
    let vault = Arc::new(Vault::new(pool.clone(), TokenTtl::default()));
    tokio::spawn(purge_expired_tokens(vault.clone()));
    tokio::spawn(drop_consumed_keys(vault.clone()));

    let (login, connector): (Arc<dyn Login>, Arc<dyn Connector>) = match (
        config.fake_telegram,
        config.telegram_app.clone(),
    ) {
        (true, _) => {
            tracing::warn!(
                "Telegram is FAKED: every login is the same fake Account (TG_MCP_FAKE_TELEGRAM=1)"
            );
            (
                Arc::new(FakeLogin::default()),
                Arc::new(FakeConnector::default()),
            )
        }
        (false, Some(app)) => {
            tracing::info!(api_id = app.api_id, "using Telegram app");
            (
                Arc::new(GrammersLogin::new(app.clone())),
                GrammersConnector::new(
                    app,
                    vault.clone(),
                    Arc::new(Limits::new(config.limits)),
                    config.live_client_idle_ttl,
                ),
            )
        }
        (false, None) => bail!("Telegram app credentials are missing"),
    };
    let deps = Deps {
        vault,
        clients: ClientRegistry::new(pool),
        login,
        connector: connector.clone(),
    };
    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(&config, deps, shutdown.clone());

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(bind = %config.bind, public_url = %config.public_url, "listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown.cancel();
    })
    .await?;

    // The last `stats` line, before the pool it counts is drained.
    tg_mcp::stats::finish(Duration::from_secs(5)).await;
    // Every connection is closed by now, so no request can open a Live client
    // while the pool is drained.
    tg::shut_down(connector.as_ref(), tg::SHUTDOWN_TIMEOUT).await;
    Ok(())
}

async fn purge_expired_tokens(vault: Arc<Vault>) {
    let mut every = tokio::time::interval(Duration::from_secs(60 * 60));
    loop {
        every.tick().await;
        match vault.purge_expired().await {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "expired tokens purged"),
            Err(err) => tracing::warn!(error = %err, "purging expired tokens failed"),
        }
    }
}

/// Every minute: delete the Wrapped keys of refresh Tokens past their grace
/// window (ADR-0003), so they leave the database file within minutes.
async fn drop_consumed_keys(vault: Arc<Vault>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    loop {
        every.tick().await;
        match vault.drop_consumed_keys().await {
            Ok(0) => {}
            Ok(dropped) => {
                tracing::info!(dropped, "wrapped keys of consumed refresh tokens deleted")
            }
            Err(err) => {
                tracing::warn!(error = %err, "deleting consumed refresh tokens' wrapped keys failed")
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}
