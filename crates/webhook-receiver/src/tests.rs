//! Tests for the webhook receiver's storage and signature-verification logic.
//!
//! This crate keeps its persistence and HTTP handling inline in `main.rs`
//! rather than behind separate `models`/`signing`/`db` modules, so these
//! tests exercise the real schema/queries used by the handlers (via an
//! in-memory SQLite pool) and the real `verify_signature` function directly,
//! instead of assuming an API surface that doesn't exist.
use super::*;

async fn memory_pool() -> SqlitePool {
    let pool = SqlitePool::connect(":memory:")
        .await
        .expect("failed to create in-memory sqlite pool");
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
    .await
    .expect("failed to create webhooks table");
    pool
}

async fn insert_webhook(
    pool: &SqlitePool,
    id: &str,
    source: &str,
    event: &str,
    payload: &str,
    signature_valid: bool,
) {
    sqlx::query(
        r#"
        INSERT INTO webhooks (id, source, event, payload, received_at, signature_valid)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(id)
    .bind(source)
    .bind(event)
    .bind(payload)
    .bind(Utc::now().to_rfc3339())
    .bind(signature_valid)
    .execute(pool)
    .await
    .expect("failed to insert webhook");
}

#[tokio::test]
async fn test_webhook_storage_and_retrieval() {
    let pool = memory_pool().await;
    insert_webhook(
        &pool,
        "test-123",
        "github",
        "push",
        r#"{"ref":"refs/heads/main"}"#,
        true,
    )
    .await;

    let row = sqlx::query_as::<_, WebhookRecord>(
        r#"
        SELECT id, source, event, payload, received_at, signature_valid
        FROM webhooks
        WHERE id = ? AND source = ?
        "#,
    )
    .bind("test-123")
    .bind("github")
    .fetch_optional(&pool)
    .await
    .expect("query failed")
    .expect("webhook not found");

    assert_eq!(row.id, "test-123");
    assert_eq!(row.source, "github");
    assert_eq!(row.event, "push");
    assert!(row.signature_valid);
}

#[tokio::test]
async fn test_list_webhooks_respects_source_and_limit() {
    let pool = memory_pool().await;
    for i in 0..5 {
        insert_webhook(
            &pool,
            &format!("test-{i}"),
            "github",
            &format!("event-{i}"),
            "{}",
            i % 2 == 0,
        )
        .await;
    }
    // A webhook from a different source should never show up in the
    // "github" listing below.
    insert_webhook(&pool, "other-1", "gitlab", "push", "{}", true).await;

    let rows = sqlx::query_as::<_, WebhookRecord>(
        r#"
        SELECT id, source, event, payload, received_at, signature_valid
        FROM webhooks
        WHERE source = ?
        ORDER BY received_at DESC
        LIMIT 3
        "#,
    )
    .bind("github")
    .fetch_all(&pool)
    .await
    .expect("query failed");

    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.source == "github"));
}

#[test]
fn test_signature_validation_valid() {
    let secret = "test-secret";
    let body = r#"{"event":"push","data":{}}"#;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(body.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    assert!(
        verify_signature(secret, body, &signature),
        "Signature verification failed for valid signature"
    );
}

#[test]
fn test_signature_validation_invalid() {
    let secret = "test-secret";
    let body = r#"{"event":"push","data":{}}"#;

    assert!(
        !verify_signature(secret, body, "invalid-signature"),
        "Signature verification passed for a malformed signature"
    );
}

#[test]
fn test_signature_validation_wrong_secret() {
    let secret = "test-secret";
    let wrong_secret = "wrong-secret";
    let body = r#"{"event":"push","data":{}}"#;

    let mut mac =
        HmacSha256::new_from_slice(wrong_secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(body.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    assert!(
        !verify_signature(secret, body, &signature),
        "Signature verification passed for a signature made with the wrong secret"
    );
}
