//! RTMP handshake implementation (version 3, plain — no HMAC digest).
//!
//! The RTMP handshake is a three-phase exchange:
//!
//! 1. **C0** (1 byte): client sends RTMP version (always `0x03`).
//! 2. **C1** (1536 bytes): client sends `[timestamp: u32, zero: u32, random: 1528 bytes]`.
//! 3. Server replies with **S0** (1 byte, version) + **S1** (1536 bytes) + **S2** (1536 bytes).
//!    S2 is an echo of C1 with the server's timestamp in the first 4 bytes.
//! 4. **C2** (1536 bytes): client echoes S1 with its timestamp, completing the handshake.
//!
//! Total exchange: C0+C1 -> S0+S1+S2 -> C2.
//! After C2, both sides can send RTMP chunk messages.

use anyhow::{Context, Result, bail};
use rand::RngExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Size of C1/S1/C2/S2 payloads in bytes.
const HANDSHAKE_SIZE: usize = 1536;

/// RTMP version we advertise.
const RTMP_VERSION: u8 = 3;

/// Perform the client-side RTMP handshake over the given async stream.
///
/// On success the stream is ready for chunk-level communication.
pub async fn perform_handshake<S>(stream: &mut S) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send,
{
    // ---- Build C0 + C1 ----
    let mut c1 = [0u8; HANDSHAKE_SIZE];
    let timestamp: u32 = 0; // we use 0 as our epoch
    c1[0..4].copy_from_slice(&timestamp.to_be_bytes());
    // Bytes 4..8 must be zero (RTMP spec "version" field, set to 0 for simple handshake).
    c1[4..8].copy_from_slice(&[0u8; 4]);
    // Fill the remaining 1528 bytes with random data.
    rand::rng().fill(&mut c1[8..]);

    // Send C0 (version byte) + C1 in one write.
    let mut c0c1 = Vec::with_capacity(1 + HANDSHAKE_SIZE);
    c0c1.push(RTMP_VERSION);
    c0c1.extend_from_slice(&c1);
    stream
        .write_all(&c0c1)
        .await
        .context("failed to send C0+C1")?;
    stream.flush().await.context("failed to flush C0+C1")?;

    // ---- Read S0 + S1 + S2 ----
    // S0: 1 byte (server version)
    let s0 = read_exact_byte(stream).await.context("failed to read S0")?;
    if s0 != RTMP_VERSION {
        bail!("server returned RTMP version {s0}, expected {RTMP_VERSION}");
    }

    // S1: 1536 bytes
    let mut s1 = [0u8; HANDSHAKE_SIZE];
    stream
        .read_exact(&mut s1)
        .await
        .context("failed to read S1")?;

    // S2: 1536 bytes (should be echo of C1)
    let mut s2 = [0u8; HANDSHAKE_SIZE];
    stream
        .read_exact(&mut s2)
        .await
        .context("failed to read S2")?;

    // Validate S2: bytes 0..4 should match our C1 timestamp (0), bytes 8.. should match
    // our random data.  Many servers don't strictly echo, so we only log a warning.
    if s2[8..] != c1[8..] {
        tracing::warn!("S2 random data does not match C1 — server may use non-standard handshake");
    }

    // ---- Build and send C2 ----
    // C2 is an echo of S1, with bytes 4..8 replaced by our receive-timestamp.
    let mut c2 = s1;
    // Put the "time2" field (our receive timestamp) in bytes 4..8.
    // We use 0 since we don't track wall-clock offsets.
    c2[4..8].copy_from_slice(&0u32.to_be_bytes());

    stream
        .write_all(&c2)
        .await
        .context("failed to send C2")?;
    stream.flush().await.context("failed to flush C2")?;

    tracing::debug!("RTMP handshake completed successfully");
    Ok(())
}

/// Read exactly one byte from the stream.
async fn read_exact_byte<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<u8> {
    let mut buf = [0u8; 1];
    stream.read_exact(&mut buf).await?;
    Ok(buf[0])
}
