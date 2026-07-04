use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http};

#[derive(Parser, Debug)]
#[command(
    name = "aish-webhook-broker",
    about = "Self-hosted webhook broker for aish plugin system",
    long_about = "Routes webhooks from external services (GitHub, Slack, etc) to connected aish clients via WebSocket or long-poll"
)]
struct Cli {
    /// Listen address (e.g., 0.0.0.0:8080)
    #[arg(short, long, env = "BROKER_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,

    /// SQLite database path
    #[arg(short, long, env = "BROKER_DB", default_value = "/var/lib/aish-broker.db")]
    db: String,

    /// Maximum queue size (messages per tenant_id+plugin_id)
    #[arg(long, env = "BROKER_MAX_QUEUE_SIZE", default_value = "1000")]
    max_queue_size: usize,

    /// WebSocket heartbeat interval (seconds)
    #[arg(long, env = "BROKER_WS_HEARTBEAT_SECS", default_value = "30")]
    ws_heartbeat_secs: u64,

    /// Long-poll timeout (seconds)
    #[arg(long, env = "BROKER_POLL_TIMEOUT_SECS", default_value = "60")]
    poll_timeout_secs: u64,

    /// Message TTL (seconds, default 7 days)
    #[arg(long, env = "BROKER_MSG_TTL_SECS", default_value = "604800")]
    msg_ttl_secs: u64,

    /// Log level
    #[arg(long, env = "BROKER_LOG_LEVEL", default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log_level))
        .with_target(true)
        .init();

    info!("aish-webhook-broker starting up");
    info!(
        listen = %cli.listen,
        db_path = %cli.db,
        max_queue_size = cli.max_queue_size,
        "Configuration loaded"
    );

    let addr: SocketAddr = cli.listen.parse()?;

    // Initialize database (synchronous rusqlite/r2d2 setup).
    let db = db::init(&cli.db)?;

    let config = BrokerConfig {
        db: db.clone(),
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size: cli.max_queue_size,
        ws_heartbeat_secs: cli.ws_heartbeat_secs,
        poll_timeout_secs: cli.poll_timeout_secs,
        msg_ttl_secs: cli.msg_ttl_secs,
    };

    // Background TTL sweep: purge expired webhooks hourly.
    {
        let sweep_db = db.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                ticker.tick().await;
                match db::ttl_cleanup(&sweep_db) {
                    Ok(n) if n > 0 => info!("TTL cleanup removed {} expired webhook(s)", n),
                    Ok(_) => {}
                    Err(e) => warn!("TTL cleanup failed: {}", e),
                }
            }
        });
    }

    let app = http::router(config);
    info!("HTTP router initialized");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Server listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shut down cleanly");
    Ok(())
}

/// Wait for SIGINT / SIGTERM for graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("Shutdown signal received");
}
