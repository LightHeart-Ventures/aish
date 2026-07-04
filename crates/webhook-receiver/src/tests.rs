#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Webhook;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_webhook_storage() {
        let db = Database::memory().await.expect("Failed to create in-memory DB");
        
        let webhook = Webhook {
            id: "test-123".to_string(),
            source: "github".to_string(),
            event: "push".to_string(),
            received_at: chrono::Utc::now(),
            signature_valid: true,
            payload: serde_json::json!({"ref": "refs/heads/main"}),
        };
        
        db.store_webhook(&webhook)
            .await
            .expect("Failed to store webhook");
        
        let stored = db.get_webhook("test-123")
            .await
            .expect("Failed to retrieve webhook")
            .expect("Webhook not found");
        
        assert_eq!(stored.id, webhook.id);
        assert_eq!(stored.source, webhook.source);
        assert_eq!(stored.event, webhook.event);
    }

    #[tokio::test]
    async fn test_list_webhooks() {
        let db = Database::memory().await.expect("Failed to create in-memory DB");
        
        // Insert test webhooks
        for i in 0..5 {
            let webhook = Webhook {
                id: format!("test-{}", i),
                source: "github".to_string(),
                event: format!("event-{}", i),
                received_at: chrono::Utc::now(),
                signature_valid: i % 2 == 0,
                payload: serde_json::json!({"index": i}),
            };
            db.store_webhook(&webhook)
                .await
                .expect(&format!("Failed to store webhook {}", i));
        }
        
        let webhooks = db.list_webhooks("github", Some(3))
            .await
            .expect("Failed to list webhooks");
        
        assert_eq!(webhooks.len(), 3);
    }

    #[test]
    fn test_signature_validation_valid() {
        let secret = "test-secret";
        let body = r#"{"event":"push","data":{}}"#;
        
        let signature = crate::signing::generate_signature(secret, body);
        let valid = crate::signing::verify_signature(secret, body, &signature);
        
        assert!(valid, "Signature verification failed for valid signature");
    }

    #[test]
    fn test_signature_validation_invalid() {
        let secret = "test-secret";
        let body = r#"{"event":"push","data":{}}"#;
        let wrong_signature = "invalid-signature";
        
        let valid = crate::signing::verify_signature(secret, body, wrong_signature);
        
        assert!(!valid, "Signature verification passed for invalid signature");
    }
}
