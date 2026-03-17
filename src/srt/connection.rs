use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use srt_protocol::config::KeySize;
use srt_transport::{SrtListener, SrtSocket, SrtSocketBuilder};

use crate::config::models::{SrtInputConfig, SrtMode, SrtOutputConfig, SrtRedundancyConfig};

/// Build an [`SrtSocketBuilder`] from common SRT configuration parameters.
///
/// Applies sensible defaults for peer idle timeout (5 s) and live mode.
/// If a passphrase is provided, AES encryption is enabled with the given
/// key length (defaulting to 16 bytes / AES-128).
fn build_socket_builder(
    latency_ms: u64,
    passphrase: Option<&str>,
    aes_key_len: Option<usize>,
) -> SrtSocketBuilder {
    let mut builder = SrtSocket::builder()
        .latency(Duration::from_millis(latency_ms))
        .live_mode()
        .peer_idle_timeout(Duration::from_secs(5));

    if let Some(pass) = passphrase {
        let key_size = match aes_key_len.unwrap_or(16) {
            24 => KeySize::AES192,
            32 => KeySize::AES256,
            _ => KeySize::AES128,
        };
        builder = builder.encryption(pass, key_size);
    }

    builder
}

/// Connect an SRT socket based on mode (caller / listener).
///
/// Builds a socket with the supplied parameters and connects in the
/// requested mode. Returns the connected socket wrapped in an `Arc`.
///
/// # Errors
///
/// Returns an error if `remote_addr` is `None` for caller mode
/// or if the underlying SRT connection fails.
pub async fn connect_srt(
    mode: &SrtMode,
    local_addr: &str,
    remote_addr: Option<&str>,
    latency_ms: u64,
    passphrase: Option<&str>,
    aes_key_len: Option<usize>,
) -> Result<Arc<SrtSocket>> {
    match mode {
        SrtMode::Caller => {
            let remote = remote_addr
                .ok_or_else(|| anyhow::anyhow!("Caller mode requires remote_addr"))?;
            let remote_sa: SocketAddr = remote
                .parse()
                .context(format!("Invalid remote address: {remote}"))?;
            let local_sa: SocketAddr = local_addr
                .parse()
                .context(format!("Invalid local address: {local_addr}"))?;

            tracing::info!("SRT caller connecting {} -> {}", local_addr, remote);

            let builder = build_socket_builder(latency_ms, passphrase, aes_key_len);
            let sock = builder
                .bind(local_sa)
                .connect(remote_sa)
                .await
                .context(format!("SRT caller connect to {remote} failed"))?;

            tracing::info!("SRT caller connected to {}", remote);
            Ok(Arc::new(sock))
        }
        SrtMode::Listener => {
            let local_sa: SocketAddr = local_addr
                .parse()
                .context(format!("Invalid local address: {local_addr}"))?;

            tracing::info!("SRT listener waiting on {}", local_addr);

            let mut listener_builder = SrtListener::builder()
                .latency(Duration::from_millis(latency_ms))
                .live_mode();

            if let Some(pass) = passphrase {
                let key_size = match aes_key_len.unwrap_or(16) {
                    24 => KeySize::AES192,
                    32 => KeySize::AES256,
                    _ => KeySize::AES128,
                };
                listener_builder = listener_builder.encryption(pass, key_size);
            }

            let mut listener = listener_builder
                .bind(local_sa)
                .await
                .context(format!("SRT listener bind on {local_addr} failed"))?;

            let sock = listener
                .accept()
                .await
                .context(format!("SRT listener accept on {local_addr} failed"))?;

            // Close the listener after accepting one connection
            let _ = listener.close().await;

            tracing::info!("SRT listener accepted connection on {}", local_addr);
            Ok(Arc::new(sock))
        }
        SrtMode::Rendezvous => {
            bail!("Rendezvous mode is not yet supported with bilbycast-srt");
        }
    }
}

/// Connect with retry logic and exponential back-off.
///
/// Retries indefinitely until the connection succeeds or the
/// `CancellationToken` is triggered. The back-off starts at 1 s and
/// doubles each attempt up to a maximum of 30 s.
///
/// # Errors
///
/// Returns an error only if the retry loop is cancelled.
pub async fn connect_srt_with_retry(
    mode: &SrtMode,
    local_addr: &str,
    remote_addr: Option<&str>,
    latency_ms: u64,
    passphrase: Option<&str>,
    aes_key_len: Option<usize>,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    let mut attempt = 0u32;
    let max_delay = Duration::from_secs(30);

    loop {
        match connect_srt(mode, local_addr, remote_addr, latency_ms, passphrase, aes_key_len).await
        {
            Ok(sock) => return Ok(sock),
            Err(e) => {
                attempt += 1;
                let delay = std::cmp::min(
                    Duration::from_millis(500 * 2u64.pow(attempt.min(6))),
                    max_delay,
                );
                tracing::warn!(
                    "SRT connection attempt {attempt} failed: {e}. Retrying in {:.1}s",
                    delay.as_secs_f64()
                );

                tokio::select! {
                    _ = cancel.cancelled() => {
                        bail!("SRT connection cancelled during retry");
                    }
                    _ = tokio::time::sleep(delay) => {
                        // Continue to next attempt
                    }
                }
            }
        }
    }
}

/// Convenience wrapper: connect an SRT socket for an input using the
/// parameters in [`SrtInputConfig`], with automatic retry.
pub async fn connect_srt_input(
    config: &SrtInputConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    connect_srt_with_retry(
        &config.mode,
        &config.local_addr,
        config.remote_addr.as_deref(),
        config.latency_ms,
        config.passphrase.as_deref(),
        config.aes_key_len,
        cancel,
    )
    .await
}

/// Convenience wrapper: connect an SRT socket for an output using the
/// parameters in [`SrtOutputConfig`], with automatic retry.
pub async fn connect_srt_output(
    config: &SrtOutputConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    connect_srt_with_retry(
        &config.mode,
        &config.local_addr,
        config.remote_addr.as_deref(),
        config.latency_ms,
        config.passphrase.as_deref(),
        config.aes_key_len,
        cancel,
    )
    .await
}

/// Convenience wrapper: connect the SMPTE 2022-7 redundancy leg (leg 2)
/// using the parameters in [`SrtRedundancyConfig`], with automatic retry.
pub async fn connect_srt_redundancy_leg(
    redundancy: &SrtRedundancyConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    connect_srt_with_retry(
        &redundancy.mode,
        &redundancy.local_addr,
        redundancy.remote_addr.as_deref(),
        redundancy.latency_ms,
        redundancy.passphrase.as_deref(),
        redundancy.aes_key_len,
        cancel,
    )
    .await
}
