use std::sync::Arc;

use anyhow::{Context, bail};
use std::time::Duration;

use tg_mcp::{
    config::Config,
    sessions::{TokenTtl, Vault},
    tg::FakeTelegram,
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
    if !config.fake_telegram {
        bail!("real Telegram login is not implemented yet; set TG_MCP_FAKE_TELEGRAM=1");
    }
    tracing::warn!("serving a FAKE Telegram account (TG_MCP_FAKE_TELEGRAM=1)");

    let vault = Arc::new(
        Vault::open_path(&config.db_path, TokenTtl::default())
            .await
            .with_context(|| format!("opening the vault at {}", config.db_path.display()))?,
    );
    tracing::info!(db = %config.db_path.display(), "vault ready");
    tokio::spawn(purge_expired_tokens(vault.clone()));

    let shutdown = CancellationToken::new();
    let app = tg_mcp::app(&config, Arc::new(FakeTelegram::default()), shutdown.clone());

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(bind = %config.bind, public_url = %config.public_url, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await?;
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
