// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Secrets encryption at rest for `secrets.json`.
//!
//! Derives a machine-specific AES-256-GCM encryption key from:
//! - `/etc/machine-id` on Linux (unique per OS installation, present on all systemd systems)
//! - A randomly generated `.secrets_key` file as fallback (macOS, non-systemd Linux, containers)
//!
//! The key derivation uses HKDF-SHA256 with a bilbycast-specific salt so that
//! even if `/etc/machine-id` is known, the derived key is application-specific.
//!
//! File format: `"v1:" + base64(12-byte-nonce || AES-256-GCM-ciphertext)`
//! The `v1:` prefix allows future format changes without breaking existing files.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use rand::RngExt;
use std::path::Path;

/// Version prefix for encrypted secrets files.
const ENCRYPTED_PREFIX: &str = "v1:";

/// HKDF salt — domain separator so the same machine-id produces different keys
/// for different applications.
const HKDF_SALT: &[u8] = b"bilbycast-edge-secrets-v1";

/// HKDF info context string.
const HKDF_INFO: &[u8] = b"aes-256-gcm-secrets-encryption";

/// Derive a 32-byte encryption key from a seed string using HKDF-SHA256.
fn derive_key(seed: &str) -> [u8; 32] {
    use hmac::Mac;

    type HmacSha256 = hmac::Hmac<sha2::Sha256>;

    // HKDF Extract: PRK = HMAC-SHA256(salt, seed)
    let mut mac = <HmacSha256 as Mac>::new_from_slice(HKDF_SALT)
        .expect("HMAC key can be any length");
    mac.update(seed.as_bytes());
    let prk = mac.finalize().into_bytes();

    // HKDF Expand: OKM = HMAC-SHA256(PRK, info || 0x01) — single block (32 bytes)
    let mut expand_input = Vec::with_capacity(HKDF_INFO.len() + 1);
    expand_input.extend_from_slice(HKDF_INFO);
    expand_input.push(0x01);

    let mut mac = <HmacSha256 as Mac>::new_from_slice(&prk)
        .expect("HMAC key can be any length");
    mac.update(&expand_input);
    let okm = mac.finalize().into_bytes();

    let mut key = [0u8; 32];
    key.copy_from_slice(&okm);
    key
}

/// Get or create the machine-specific seed for key derivation.
///
/// Priority:
/// 1. `/etc/machine-id` — present on all systemd-based Linux (Debian, Ubuntu, RHEL, Fedora, Arch, etc.)
/// 2. Fallback: generate a random key file at `<secrets_dir>/.secrets_key`
///
/// The fallback ensures the edge works on macOS (development), containers without
/// systemd, and other non-standard environments.
pub fn get_machine_seed(secrets_dir: &Path) -> Result<String> {
    // Try /etc/machine-id first (Linux, 32 hex chars + newline)
    if let Ok(machine_id) = std::fs::read_to_string("/etc/machine-id") {
        let trimmed = machine_id.trim();
        if trimmed.len() >= 32 {
            return Ok(trimmed.to_string());
        }
    }

    // Also try /var/lib/dbus/machine-id (older Linux without systemd)
    if let Ok(machine_id) = std::fs::read_to_string("/var/lib/dbus/machine-id") {
        let trimmed = machine_id.trim();
        if trimmed.len() >= 32 {
            return Ok(trimmed.to_string());
        }
    }

    // Fallback: use or create a random key file
    let key_path = secrets_dir.join(".secrets_key");
    if let Ok(existing) = std::fs::read_to_string(&key_path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 {
            return Ok(trimmed.to_string());
        }
    }

    // Generate a new random 32-byte key (hex-encoded)
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let hex_key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    // Write with restrictive permissions
    std::fs::write(&key_path, &hex_key)
        .with_context(|| format!("Failed to write secrets key to {}", key_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::info!(
        "Generated new secrets encryption key at {} (no /etc/machine-id found)",
        key_path.display()
    );

    Ok(hex_key)
}

/// Encrypt a plaintext string using AES-256-GCM with a machine-derived key.
/// Returns the encrypted string with a version prefix.
pub fn encrypt_secrets(plaintext: &str, seed: &str) -> Result<String> {
    let key = derive_key(seed);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| anyhow::anyhow!("Invalid encryption key: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    // Concatenate nonce + ciphertext and base64-encode
    let mut combined = Vec::with_capacity(12 + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&combined);

    Ok(format!("{ENCRYPTED_PREFIX}{encoded}"))
}

/// Decrypt an encrypted secrets string. Returns the plaintext JSON.
/// If the input is not encrypted (no version prefix), returns it as-is
/// for backward compatibility with existing unencrypted secrets.json files.
pub fn decrypt_secrets(data: &str, seed: &str) -> Result<String> {
    // Backward compatibility: if not encrypted, return as-is
    if !data.starts_with(ENCRYPTED_PREFIX) {
        return Ok(data.to_string());
    }

    let encoded = &data[ENCRYPTED_PREFIX.len()..];

    use base64::Engine;
    let combined = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Failed to decode encrypted secrets (invalid base64)")?;

    if combined.len() < 13 {
        anyhow::bail!("Encrypted secrets data too short");
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let key = derive_key(seed);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| anyhow::anyhow!("Invalid encryption key: {e}"))?;

    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!(
            "Failed to decrypt secrets.json — the machine identity may have changed, \
             or the file was copied from another machine. If this is expected, delete \
             secrets.json and re-register with the manager."
        ))?;

    String::from_utf8(plaintext).context("Decrypted secrets contain invalid UTF-8")
}

/// Returns true if the given data appears to be encrypted (has version prefix).
pub fn is_encrypted(data: &str) -> bool {
    data.starts_with(ENCRYPTED_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let seed = "test-machine-id-1234567890abcdef";
        let plaintext = r#"{"manager_node_secret": "super-secret-value"}"#;
        let encrypted = encrypt_secrets(plaintext, seed).unwrap();

        assert!(encrypted.starts_with("v1:"));
        assert_ne!(encrypted, plaintext);

        let decrypted = decrypt_secrets(&encrypted, seed).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_backward_compatibility_unencrypted() {
        let seed = "test-machine-id";
        let plaintext = r#"{"version": 1, "manager_node_secret": "test"}"#;

        // Unencrypted data should pass through unchanged
        let result = decrypt_secrets(plaintext, seed).unwrap();
        assert_eq!(result, plaintext);
    }

    #[test]
    fn test_wrong_seed_fails() {
        let plaintext = "secret data";
        let encrypted = encrypt_secrets(plaintext, "seed-a").unwrap();
        let result = decrypt_secrets(&encrypted, "seed-b");
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_key_deterministic() {
        let k1 = derive_key("same-seed");
        let k2 = derive_key("same-seed");
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_derive_key_different_seeds() {
        let k1 = derive_key("seed-a");
        let k2 = derive_key("seed-b");
        assert_ne!(k1, k2);
    }
}
