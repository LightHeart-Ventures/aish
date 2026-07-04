//! HMAC-SHA256 signature verification for webhook authentication.
//!
//! GitHub, Slack and most webhook producers sign the raw request body with a
//! shared secret and send the hex digest in a header (e.g. `X-Hub-Signature-256:
//! sha256=<hex>`). We recompute and compare in constant time.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a webhook signature using HMAC-SHA256.
///
/// Accepts signatures with or without the `sha256=` prefix.
pub fn verify_signature(payload: &[u8], signature: &str, secret: &str) -> crate::error::Result<()> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| crate::error::BrokerError::InvalidSignature)?;
    mac.update(payload);

    let computed = hex::encode(mac.finalize().into_bytes());
    let provided = signature.strip_prefix("sha256=").unwrap_or(signature);

    if constant_time_compare(provided.as_bytes(), computed.as_bytes()) {
        Ok(())
    } else {
        Err(crate::error::BrokerError::InvalidSignature)
    }
}

/// Compute the HMAC-SHA256 signature of `payload` under `secret`, returned as a
/// lowercase hex digest (no `sha256=` prefix). Handy for clients that need to
/// sign outbound bodies and for tests.
pub fn sign(payload: &[u8], secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any size");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time comparison to prevent timing attacks.
fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, payload: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn valid_with_prefix() {
        let secret = "my-secret";
        let payload = b"test payload";
        let sig = format!("sha256={}", sign(secret, payload));
        assert!(verify_signature(payload, &sig, secret).is_ok());
    }

    #[test]
    fn valid_without_prefix() {
        let secret = "my-secret";
        let payload = b"test payload";
        let sig = sign(secret, payload);
        assert!(verify_signature(payload, &sig, secret).is_ok());
    }

    #[test]
    fn invalid_signature_rejected() {
        assert!(verify_signature(b"test payload", "sha256=deadbeef", "my-secret").is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let payload = b"{\"action\":\"opened\"}";
        let sig = format!("sha256={}", sign("correct", payload));
        assert!(verify_signature(payload, &sig, "wrong").is_err());
    }

    #[test]
    fn tamper_detection() {
        let secret = "s3cr3t";
        let sig = format!("sha256={}", sign(secret, b"original body"));
        assert!(verify_signature(b"tampered body", &sig, secret).is_err());
    }
}
