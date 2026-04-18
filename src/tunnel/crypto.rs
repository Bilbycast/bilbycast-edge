// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! End-to-end tunnel encryption using ChaCha20-Poly1305 (AEAD).
//!
//! Both edges share a 32-byte symmetric key. All tunnel data payloads are
//! encrypted before sending through the relay. The relay sees only ciphertext
//! and cannot read or tamper with the traffic.
//!
//! Wire format per encrypted message:
//! ```text
//! [12-byte nonce][ciphertext + 16-byte auth tag]
//! ```
//!
//! Overhead: 28 bytes per operation (12 nonce + 16 tag).
//! For a 1316-byte SRT packet, this is ~2% overhead.

use anyhow::{Context, Result};
use ring::aead;
use ring::rand::{SecureRandom, SystemRandom};

/// AEAD nonce size (96 bits).
const NONCE_LEN: usize = 12;

/// AEAD tag size (128 bits).
const TAG_LEN: usize = 16;

/// Total overhead per encrypted message (used in tests).
#[cfg(test)]
const ENCRYPTION_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

/// Symmetric cipher for end-to-end tunnel encryption.
///
/// Thread-safe — can be shared via `Arc<TunnelCipher>` across forwarder tasks.
pub struct TunnelCipher {
    sealing_key: aead::LessSafeKey,
    opening_key: aead::LessSafeKey,
    rng: SystemRandom,
}

impl TunnelCipher {
    /// Create a new cipher from a hex-encoded 32-byte key.
    pub fn new(key_hex: &str) -> Result<Self> {
        let key_bytes = hex_decode(key_hex)
            .context("invalid tunnel_encryption_key: must be valid hex")?;
        if key_bytes.len() != 32 {
            anyhow::bail!(
                "tunnel_encryption_key must be 32 bytes (64 hex chars), got {} bytes",
                key_bytes.len()
            );
        }

        let unbound_key = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key_bytes)
            .map_err(|_| anyhow::anyhow!("failed to create AEAD key"))?;
        let sealing_key = aead::LessSafeKey::new(unbound_key);

        let unbound_key2 = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key_bytes)
            .map_err(|_| anyhow::anyhow!("failed to create AEAD key"))?;
        let opening_key = aead::LessSafeKey::new(unbound_key2);

        Ok(Self {
            sealing_key,
            opening_key,
            rng: SystemRandom::new(),
        })
    }

    /// Encrypt a plaintext payload.
    ///
    /// Returns `[12-byte nonce][ciphertext + 16-byte auth tag]`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // Generate random nonce
        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("failed to generate random nonce"))?;

        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

        // Encrypt in place: start with plaintext, tag is appended
        let mut in_out = plaintext.to_vec();
        self.sealing_key
            .seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut in_out)
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;

        // Prepend nonce
        let mut result = Vec::with_capacity(NONCE_LEN + in_out.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&in_out);
        Ok(result)
    }

    /// Decrypt a ciphertext payload.
    ///
    /// Input format: `[12-byte nonce][ciphertext + 16-byte auth tag]`.
    /// Returns the decrypted plaintext.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < NONCE_LEN + TAG_LEN {
            anyhow::bail!(
                "ciphertext too short: {} bytes (minimum {})",
                ciphertext.len(),
                NONCE_LEN + TAG_LEN
            );
        }

        let nonce_bytes: [u8; NONCE_LEN] = ciphertext[..NONCE_LEN]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid nonce"))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

        let mut in_out = ciphertext[NONCE_LEN..].to_vec();
        let plaintext = self
            .opening_key
            .open_in_place(nonce, aead::Aad::empty(), &mut in_out)
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong key or tampered data)"))?;

        Ok(plaintext.to_vec())
    }
}

/// Decode hex string to bytes.
fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        anyhow::bail!("hex string must have even length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("invalid hex at position {i}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn test_roundtrip() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        let plaintext = b"Hello, encrypted tunnel!";
        let encrypted = cipher.encrypt(plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn test_empty_payload() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        let encrypted = cipher.encrypt(b"").unwrap();
        assert_eq!(encrypted.len(), NONCE_LEN + TAG_LEN);
        let decrypted = cipher.decrypt(&encrypted).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_large_payload() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        let plaintext = vec![0xABu8; 65536];
        let encrypted = cipher.encrypt(&plaintext).unwrap();
        assert_eq!(encrypted.len(), plaintext.len() + ENCRYPTION_OVERHEAD);
        let decrypted = cipher.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let cipher1 = TunnelCipher::new(TEST_KEY).unwrap();
        let cipher2 = TunnelCipher::new(
            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
        )
        .unwrap();

        let encrypted = cipher1.encrypt(b"secret data").unwrap();
        assert!(cipher2.decrypt(&encrypted).is_err());
    }

    #[test]
    fn test_tampered_data_fails() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        let mut encrypted = cipher.encrypt(b"important data").unwrap();
        // Flip a byte in the ciphertext
        if let Some(byte) = encrypted.last_mut() {
            *byte ^= 0xFF;
        }
        assert!(cipher.decrypt(&encrypted).is_err());
    }

    #[test]
    fn test_too_short_ciphertext() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        assert!(cipher.decrypt(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_different_nonces() {
        let cipher = TunnelCipher::new(TEST_KEY).unwrap();
        let plaintext = b"same data";
        let e1 = cipher.encrypt(plaintext).unwrap();
        let e2 = cipher.encrypt(plaintext).unwrap();
        // Different random nonces → different ciphertext
        assert_ne!(e1, e2);
        // But both decrypt to the same plaintext
        assert_eq!(cipher.decrypt(&e1).unwrap(), plaintext);
        assert_eq!(cipher.decrypt(&e2).unwrap(), plaintext);
    }

    #[test]
    fn test_invalid_key_hex() {
        assert!(TunnelCipher::new("not-valid-hex").is_err());
        assert!(TunnelCipher::new("0123").is_err()); // too short
    }
}
