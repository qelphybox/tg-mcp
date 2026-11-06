use std::sync::Arc;

use anyhow::{Context, bail};
use tg_mcp::{config::Config, tg::FakeTelegram};
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
