use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::str::FromStr;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

#[cfg(test)]
mod tests;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    secret: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct WebhookPayload {
    #[serde(default)]
    id: Option<String>,
    event: String,
    timestamp: Option<i64>,
    data: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, sqlx::FromRow)]
struct WebhookRecord {
    id: String,
    source: String,
    event: String,
    payload: String,
    received_at: String,
    signature_valid: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
}

#[derive(Serialize)]
struct ListResponse {
    count: i64,
    webhooks: Vec<WebhookRecord>,
}

// Health check endpoint
async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// Receive webhook from a source
async fn receive_webhook(
    State(state): State<Arc<AppState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Verify signature if provided
    let signature_valid = if let Some(sig_header) = headers.get("X-Webhook-Signature") {
        let sig = sig_header
            .to_str()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid signature header".to_string()))?;
        verify_signature(&state.secret, &body, sig)
    } else {
        warn!("Webhook received from {} without signature", source);
        false
    };

    // Parse payload
    let payload: WebhookPayload = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)))?;

    let webhook_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now().to_rfc3339();

    // Store in database
    sqlx::query(
        r#"
        INSERT INTO webhooks (id, source, event, payload, received_at, signature_valid)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&webhook_id)
    .bind(&source)
    .bind(&payload.event)
    .bind(&body)
    .bind(&timestamp)
    .bind(signature_valid)
    .execute(&state.db)
    .await
    .map_err(|e| {
        error!("Failed to insert webhook: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to store webhook".to_string())
    })?;

    info!(
        webhook_id = %webhook_id,
        source = %source,
        event = %payload.event,
        signature_valid = signature_valid,
        "Webhook received and stored"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": webhook_id,
            "received": timestamp,
            "signature_valid": signature_valid,
        })),
    ))
}

// List webhooks for a source
async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    Path(source): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let rows = sqlx::query_as::<_, WebhookRecord>(
        r#"
        SELECT id, source, event, payload, received_at, signature_valid
        FROM webhooks
        WHERE source = ?
        ORDER BY received_at DESC
        LIMIT 100
        "#,
    )
    .bind(&source)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        error!("Failed to fetch webhooks: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch webhooks".to_string())
    })?;

    let count = rows.len() as i64;
    Ok(Json(ListResponse {
        count,
        webhooks: rows,
    }))
}

// Get a specific webhook
async fn get_webhook(
    State(state): State<Arc<AppState>>,
    Path((source, webhook_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let row = sqlx::query_as::<_, WebhookRecord>(
        r#"
        SELECT id, source, event, payload, received_at, signature_valid
        FROM webhooks
        WHERE id = ? AND source = ?
        "#,
    )
    .bind(&webhook_id)
    .bind(&source)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        error!("Failed to fetch webhook: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch webhook".to_string())
    })?
    .ok_or((StatusCode::NOT_FOUND, "Webhook not found".to_string()))?;

    Ok(Json(row))
}

// Verify HMAC signature
fn verify_signature(secret: &str, body: &str, signature: &str) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(body.as_bytes());

    let expected = mac.finalize();
    let _expected_hex = hex::encode(expected.into_bytes());

    // Constant-time comparison
    match hex::decode(signature) {
        Ok(sig_bytes) => {
            if sig_bytes.len() != 32 {
                return false;
            }
            match HmacSha256::new_from_slice(secret.as_bytes()) {
                Ok(mut mac2) => {
                    mac2.update(body.as_bytes());
                    mac2.verify_slice(&sig_bytes).is_ok()
                }
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("webhook_receiver=info".parse()?),
        )
        .init();

    // Load config
    dotenv::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite:webhooks.db".to_string());
    let secret = std::env::var("WEBHOOK_SECRET")
        .unwrap_or_else(|_| "dev-secret-change-in-production".to_string());
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()?;

    info!("Starting webhook receiver on port {}", port);

    // Create database pool and migrate
    // create_if_missing so a fresh Fly volume (empty /data) bootstraps the DB
    // file instead of erroring with "unable to open database file".
    let connect_options = SqliteConnectOptions::from_str(&database_url)?
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(connect_options).await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS webhooks (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            event TEXT NOT NULL,
            payload TEXT NOT NULL,
            received_at TEXT NOT NULL,
            signature_valid BOOLEAN NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_source ON webhooks(source)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_received_at ON webhooks(received_at DESC)",
    )
    .execute(&pool)
    .await?;

    let state = Arc::new(AppState {
        db: pool,
        secret,
    });

    // Build router
    let app = Router::new()
        .route("/health", get(health))
        .route("/webhooks/:source", post(receive_webhook))
        .route("/webhooks/:source", get(list_webhooks))
        .route("/webhooks/:source/:id", get(get_webhook))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    // Start server
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("Listening on port {}", port);

    axum::serve(listener, app).await?;

    Ok(())
}
