//! Database initialization, schema, and the persistent webhook store.
//!
//! SQLite is the source of truth for queued webhooks so the broker survives
//! restarts. rusqlite calls are synchronous; they are fast enough for the
//! broker's throughput profile and mirror how the parent `aish` crate uses
//! SQLite. Handlers call these helpers directly.

use r2d2_sqlite::SqliteConnectionManager;
use tracing::info;

use crate::error::{BrokerError, Result};
use crate::queue::Webhook;

pub type DbPool = r2d2::Pool<SqliteConnectionManager>;

/// A registered aish client row.
#[derive(Clone, Debug)]
pub struct ClientRow {
    pub client_id: String,
    pub tenant_id: String,
    pub plugin_id: String,
    pub session_token: String,
}

/// Initialize the SQLite database: open a pool, apply pragmas, run migrations.
pub fn init(db_path: &str) -> Result<DbPool> {
    info!("Initializing database at {}", db_path);

    let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
    });
    let pool = r2d2::Pool::builder()
        .max_size(8)
        .build(manager)
        .map_err(|e| BrokerError::Database(e.to_string()))?;

    migrate(&pool)?;
    info!("Database schema ready");
    Ok(pool)
}

/// Apply the schema (idempotent — safe to run on every startup).
pub fn migrate(pool: &DbPool) -> Result<()> {
    let conn = pool.get()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clients (
            client_id      TEXT PRIMARY KEY,
            tenant_id      TEXT NOT NULL,
            plugin_id      TEXT NOT NULL,
            session_id     TEXT NOT NULL,
            session_token  TEXT NOT NULL UNIQUE,
            transport      TEXT NOT NULL DEFAULT 'websocket',
            secret         TEXT,
            created_at     TEXT NOT NULL,
            last_seen_at   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_clients_tenant_plugin
            ON clients (tenant_id, plugin_id);

        CREATE TABLE IF NOT EXISTS webhooks (
            id             TEXT PRIMARY KEY,
            tenant_id      TEXT NOT NULL,
            plugin_id      TEXT NOT NULL,
            event_type     TEXT NOT NULL,
            payload        TEXT NOT NULL,
            received_at    TEXT NOT NULL,
            ttl_expires_at TEXT NOT NULL,
            delivered      INTEGER NOT NULL DEFAULT 0,
            delivered_at   TEXT,
            delivered_to   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_webhooks_pending
            ON webhooks (tenant_id, plugin_id, delivered, received_at);
        CREATE INDEX IF NOT EXISTS idx_webhooks_ttl
            ON webhooks (ttl_expires_at);

        CREATE TABLE IF NOT EXISTS audit_log (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          TEXT NOT NULL,
            event       TEXT NOT NULL,
            tenant_id   TEXT,
            plugin_id   TEXT,
            detail      TEXT
        );",
    )?;
    Ok(())
}

/// Register (or re-register) a client. Returns the generated identifiers.
///
/// Idempotent on (tenant_id, plugin_id, session_id): re-registering the same
/// logical client rotates its token but keeps a single row.
pub fn register_client(
    pool: &DbPool,
    tenant_id: &str,
    plugin_id: &str,
    session_id: &str,
    transport: &str,
    secret: Option<&str>,
) -> Result<ClientRow> {
    let conn = pool.get()?;
    let client_id = format!("client_{}", uuid::Uuid::new_v4().simple());
    let session_token = format!("st_{}", uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now().to_rfc3339();

    // Drop any prior row for this logical client, then insert fresh.
    conn.execute(
        "DELETE FROM clients WHERE tenant_id = ?1 AND plugin_id = ?2 AND session_id = ?3",
        rusqlite::params![tenant_id, plugin_id, session_id],
    )?;
    conn.execute(
        "INSERT INTO clients
            (client_id, tenant_id, plugin_id, session_id, session_token, transport, secret, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        rusqlite::params![
            client_id,
            tenant_id,
            plugin_id,
            session_id,
            session_token,
            transport,
            secret,
            now,
        ],
    )?;

    audit(pool, "client_registered", Some(tenant_id), Some(plugin_id), None);

    Ok(ClientRow {
        client_id,
        tenant_id: tenant_id.to_string(),
        plugin_id: plugin_id.to_string(),
        session_token,
    })
}

/// Look up the shared secret for a (tenant, plugin), if any client registered one.
pub fn get_secret(pool: &DbPool, tenant_id: &str, plugin_id: &str) -> Result<Option<String>> {
    let conn = pool.get()?;
    let secret: Option<String> = conn
        .query_row(
            "SELECT secret FROM clients
             WHERE tenant_id = ?1 AND plugin_id = ?2 AND secret IS NOT NULL
             ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![tenant_id, plugin_id],
            |row| row.get(0),
        )
        .ok();
    Ok(secret)
}

/// Return true if any client is registered for this (tenant, plugin).
pub fn tenant_plugin_exists(pool: &DbPool, tenant_id: &str, plugin_id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM clients WHERE tenant_id = ?1 AND plugin_id = ?2",
        rusqlite::params![tenant_id, plugin_id],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Validate a session token, returning the owning client row.
pub fn validate_session(pool: &DbPool, session_token: &str) -> Result<Option<ClientRow>> {
    let conn = pool.get()?;
    let row = conn
        .query_row(
            "SELECT client_id, tenant_id, plugin_id, session_token
             FROM clients WHERE session_token = ?1",
            rusqlite::params![session_token],
            |row| {
                Ok(ClientRow {
                    client_id: row.get(0)?,
                    tenant_id: row.get(1)?,
                    plugin_id: row.get(2)?,
                    session_token: row.get(3)?,
                })
            },
        )
        .ok();
    Ok(row)
}

/// Persist a webhook. Enforces `max_queue_size` per (tenant, plugin) by dropping
/// the oldest undelivered rows. Returns `QueueFull` only if the cap is 0.
pub fn insert_webhook(pool: &DbPool, wh: &Webhook, ttl_secs: u64, max_queue: usize) -> Result<()> {
    if max_queue == 0 {
        return Err(BrokerError::QueueFull);
    }
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;

    let ttl_expires = (wh.received_at + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339();
    let payload_str = serde_json::to_string(&wh.payload)
        .map_err(|e| BrokerError::InvalidJson(e.to_string()))?;

    tx.execute(
        "INSERT INTO webhooks
            (id, tenant_id, plugin_id, event_type, payload, received_at, ttl_expires_at, delivered)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
        rusqlite::params![
            wh.id,
            wh.tenant_id,
            wh.plugin_id,
            wh.event_type,
            payload_str,
            wh.received_at.to_rfc3339(),
            ttl_expires,
        ],
    )?;

    // Bound the queue: drop oldest undelivered beyond the cap (FIFO overflow).
    tx.execute(
        "DELETE FROM webhooks WHERE id IN (
            SELECT id FROM webhooks
            WHERE tenant_id = ?1 AND plugin_id = ?2 AND delivered = 0
            ORDER BY received_at DESC
            LIMIT -1 OFFSET ?3
        )",
        rusqlite::params![wh.tenant_id, wh.plugin_id, max_queue as i64],
    )?;

    tx.commit()?;
    Ok(())
}

/// Count undelivered webhooks for a (tenant, plugin).
pub fn count_pending(pool: &DbPool, tenant_id: &str, plugin_id: &str) -> Result<i64> {
    let conn = pool.get()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhooks
         WHERE tenant_id = ?1 AND plugin_id = ?2 AND delivered = 0",
        rusqlite::params![tenant_id, plugin_id],
        |row| row.get(0),
    )?;
    Ok(n)
}

/// Total undelivered webhooks across all tenants (for `/health`).
pub fn total_pending(pool: &DbPool) -> Result<i64> {
    let conn = pool.get()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhooks WHERE delivered = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(n)
}

/// Fetch up to `limit` undelivered webhooks for a (tenant, plugin), oldest first.
pub fn fetch_pending(
    pool: &DbPool,
    tenant_id: &str,
    plugin_id: &str,
    limit: usize,
) -> Result<Vec<Webhook>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, tenant_id, plugin_id, event_type, payload, received_at
         FROM webhooks
         WHERE tenant_id = ?1 AND plugin_id = ?2 AND delivered = 0
         ORDER BY received_at ASC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![tenant_id, plugin_id, limit as i64],
        |row| {
            let payload_str: String = row.get(4)?;
            let received_str: String = row.get(5)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                payload_str,
                received_str,
            ))
        },
    )?;

    let mut out = Vec::new();
    for r in rows {
        let (id, tenant, plugin, event_type, payload_str, received_str) = r?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        let received_at = chrono::DateTime::parse_from_rfc3339(&received_str)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        out.push(Webhook {
            id,
            tenant_id: tenant,
            plugin_id: plugin,
            event_type,
            payload,
            received_at,
        });
    }
    Ok(out)
}

/// Mark a webhook delivered/acked. Returns true if a row was updated.
pub fn mark_delivered(
    pool: &DbPool,
    tenant_id: &str,
    plugin_id: &str,
    webhook_id: &str,
    delivered_to: Option<&str>,
) -> Result<bool> {
    let conn = pool.get()?;
    let now = chrono::Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE webhooks SET delivered = 1, delivered_at = ?1, delivered_to = ?2
         WHERE id = ?3 AND tenant_id = ?4 AND plugin_id = ?5 AND delivered = 0",
        rusqlite::params![now, delivered_to, webhook_id, tenant_id, plugin_id],
    )?;
    Ok(n > 0)
}

/// Delete expired webhooks (TTL cleanup). Returns the number removed.
pub fn ttl_cleanup(pool: &DbPool) -> Result<usize> {
    let conn = pool.get()?;
    let now = chrono::Utc::now().to_rfc3339();
    let n = conn.execute(
        "DELETE FROM webhooks WHERE ttl_expires_at < ?1",
        rusqlite::params![now],
    )?;
    Ok(n)
}

/// Best-effort audit trail write (never fails the caller).
pub fn audit(
    pool: &DbPool,
    event: &str,
    tenant_id: Option<&str>,
    plugin_id: Option<&str>,
    detail: Option<&str>,
) {
    if let Ok(conn) = pool.get() {
        let now = chrono::Utc::now().to_rfc3339();
        let _ = conn.execute(
            "INSERT INTO audit_log (ts, event, tenant_id, plugin_id, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![now, event, tenant_id, plugin_id, detail],
        );
    }
}
