// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! HMAC-SHA256 token generation for tunnel authentication.
//!
//! Matches the auth scheme in `bilbycast-relay/src/auth.rs`.
//!
//! - **Relay mode**: `generate_token(edge_id, relay_shared_secret)`
//! - **Direct mode**: `generate_token(tunnel_id, per_tunnel_psk)`

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Generate an HMAC-SHA256 signed token.
///
/// Token format: `base64(identity:hmac_hex)` where `hmac_hex = HMAC-SHA256(identity, secret)`.
pub fn generate_token(identity: &str, secret: &str) -> String {
    let sig = compute_hmac(identity, secret);
    let payload = format!("{identity}:{sig}");
    base64::engine::general_purpose::STANDARD.encode(payload.as_bytes())
}

/// Verify a token and extract the identity.
///
/// Returns `Some(identity)` if valid, `None` if invalid.
pub fn verify_token(token: &str, secret: &str) -> Option<String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    let payload = String::from_utf8(decoded).ok()?;
    let (identity, provided_sig) = payload.split_once(':')?;
    let expected_sig = compute_hmac(identity, secret);
    if provided_sig == expected_sig {
        Some(identity.to_string())
    } else {
        None
    }
}

fn compute_hmac(identity: &str, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key can be any length");
    mac.update(identity.as_bytes());
    let result = mac.finalize();
    hex_encode(&result.into_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_verify() {
        let token = generate_token("edge-abc", "my_secret");
        assert_eq!(verify_token(&token, "my_secret"), Some("edge-abc".into()));
    }

    #[test]
    fn test_wrong_secret() {
        let token = generate_token("edge-1", "correct");
        assert_eq!(verify_token(&token, "wrong"), None);
    }
}
