// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! End-to-end tunnel encryption using ChaCha20-Poly1305 (AEAD).
//!
//! Both edges share a 32-byte symmetric key. All tunnel data payloads are
//! encrypted before sending through the relay. The relay sees only ciphertext
//! and cannot read the payload or forge one. It **can** replay, drop, reorder,
//! truncate and re-address frames undetectably — see
//! "[What this layer does NOT protect against](#what-this-layer-does-not-protect-against)"
//! below before concluding that nothing further is needed.
//!
//! Wire format per encrypted message:
//! ```text
//! [12-byte nonce][ciphertext + 16-byte auth tag]
//! ```
//!
//! Overhead: 28 bytes per operation (12 nonce + 16 tag).
//! For a 1316-byte SRT packet, this is ~2% overhead.
//!
//! ## Nonce strategy and key-rotation guidance
//!
//! ChaCha20-Poly1305 takes a 96-bit nonce. We generate it from a CSPRNG
//! per-message, which gives a birthday-paradox collision boundary at
//! ~2^48 messages per key. At broadcast bitrates this is decades for a
//! single tunnel, but operators running many tunnels under one key (or
//! running the same key across edge restarts) should rotate the
//! `tunnel_encryption_key` periodically.
//!
//! **Recommended rotation cadence**: the manager's `rotate_secret`
//! mechanism should re-key any tunnel that has carried more than
//! ~2^32 messages (≈ a day of 1 Gbps traffic at 1316-byte payloads),
//! or every 30 days of operation, whichever comes first. The manager
//! already has the rotation primitive — see
//! `bilbycast-manager/CLAUDE.md` "Node secret rotation".
//!
//! A future protocol revision will move to XChaCha20-Poly1305 (192-bit
//! nonce) which eliminates the rotation requirement entirely; that's a
//! wire-format change and is gated on the next bump of
//! `TUNNEL_PROTOCOL_VERSION`.
//!
//! ## What this layer does NOT protect against
//!
//! The AEAD authenticates the payload under a per-message random nonce with an
//! **empty AAD**, and the 16-byte `tunnel_id` that prefixes every datagram
//! ([`super::protocol::encode_udp_datagram`]) rides *outside* that
//! authentication. Two consequences follow. Both are current, deliberate
//! properties rather than oversights, and closing either one properly is a
//! wire change that BOTH edges must take in the same release — one end alone
//! blacks the tunnel out — so both are gated on the next
//! `TUNNEL_PROTOCOL_VERSION` bump alongside the XChaCha20 move above.
//!
//! - **Replay.** There is no sequence number, timestamp or receive window, so a
//!   captured datagram opens successfully however often it is re-injected.
//!
//!   Where the tunnel carries **SRT, RIST or a bond leg**, the payload protocol
//!   dedups by its own sequence number and a replay is absorbed there. That is
//!   the load-bearing mitigation, and a window at this layer would duplicate it
//!   while having to be wide enough to pass genuine reordering — which is
//!   exactly the width a replay fits through.
//!
//!   It does **not** cover two shapes this cipher also protects, and for those
//!   replay resistance is genuinely absent rather than delegated:
//!   - A **TCP tunnel** ([`super::tcp_forwarder`], documented in
//!     [`super`] as camera control / signalling). Nothing dedups at any layer,
//!     and a replayed chunk does not duplicate a packet — it inserts bytes into
//!     a byte stream, re-issuing a control command (PTZ, record start/stop) or
//!     desynchronising a signalling protocol. The relay pipes those bytes, so
//!     it is squarely inside the stated threat model.
//!   - A **generic UDP tunnel** carrying raw MPEG-TS or RTP. Nothing restricts
//!     a UDP tunnel to SRT/RIST — `run_egress` forwards whatever arrives on
//!     `local_addr` — and a replayed 1316-byte datagram is 7 duplicate TS
//!     packets and a continuity-counter fault.
//!
//!   Closing this properly needs a counter on the wire, so it is gated on the
//!   next `TUNNEL_PROTOCOL_VERSION` bump; until then it is residual risk, not
//!   an absent one.
//! - **Cross-tunnel movement.** With `tunnel_id` outside the AAD, a datagram
//!   captured on one tunnel can be re-framed with another tunnel's id. It still
//!   only *opens* on a tunnel that shares the key, and the manager mints a
//!   distinct random `tunnel_encryption_key` per tunnel, so in a
//!   manager-provisioned fleet this is confined to hand-written configs that
//!   reuse one key across tunnels. Independently of the key, every receive path
//!   now drops a datagram whose prefix is not its own tunnel id —
//!   [`super::udp_forwarder::run_egress`] / [`super::udp_forwarder::run_ingress`],
//!   the relayed bond leg, and `udp_relay_client::run_native_direct_listener`
//!   — counted on `UdpForwarderStats::framing_errors`. That is a plaintext
//!   routing check, not authentication: it refuses a *mis-addressed* datagram,
//!   while an attacker who can reach the receive path can trivially write the
//!   correct 16 bytes.
//!
//! Binding `tunnel_id` (and, with a counter on the wire, a receive window) into
//! the AAD is the complete fix; neither can be made unilaterally.

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
    if !hex.len().is_multiple_of(2) {
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
