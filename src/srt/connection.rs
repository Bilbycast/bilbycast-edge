// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use srt_protocol::config::{CryptoModeConfig, KeySize, RetransmitAlgo};
use srt_transport::{SrtListener, SrtSocket, SrtSocketBuilder};

use crate::config::models::{SrtInputConfig, SrtMode, SrtOutputConfig, SrtRedundancyConfig};
use crate::stats::models::SrtLegStats;

/// Common SRT connection parameters extracted from any SRT config type.
///
/// This struct avoids passing 20+ positional parameters through the connection
/// pipeline. Use `From<&SrtInputConfig>`, `From<&SrtOutputConfig>`, or
/// `From<&SrtRedundancyConfig>` to construct.
pub struct SrtConnectionParams<'a> {
    pub mode: &'a SrtMode,
    pub local_addr: &'a str,
    pub remote_addr: Option<&'a str>,
    pub latency_ms: u64,
    pub recv_latency_ms: Option<u64>,
    pub peer_latency_ms: Option<u64>,
    pub peer_idle_timeout_secs: u64,
    pub passphrase: Option<&'a str>,
    pub aes_key_len: Option<usize>,
    pub crypto_mode: Option<&'a str>,
    pub max_rexmit_bw: Option<i64>,
    pub stream_id: Option<&'a str>,
    pub packet_filter: Option<&'a str>,
    pub max_bw: Option<i64>,
    pub input_bw: Option<i64>,
    pub overhead_bw: Option<i32>,
    pub enforced_encryption: Option<bool>,
    pub connect_timeout_secs: Option<u64>,
    pub flight_flag_size: Option<u32>,
    pub send_buffer_size: Option<u32>,
    pub recv_buffer_size: Option<u32>,
    pub ip_tos: Option<i32>,
    pub retransmit_algo: Option<&'a str>,
    pub send_drop_delay: Option<i32>,
    pub loss_max_ttl: Option<i32>,
    pub km_refresh_rate: Option<u32>,
    pub km_pre_announce: Option<u32>,
    pub payload_size: Option<u32>,
    pub mss: Option<u32>,
    pub tlpkt_drop: Option<bool>,
    pub ip_ttl: Option<i32>,
}

impl<'a> From<&'a SrtInputConfig> for SrtConnectionParams<'a> {
    fn from(c: &'a SrtInputConfig) -> Self {
        Self {
            mode: &c.mode,
            local_addr: &c.local_addr,
            remote_addr: c.remote_addr.as_deref(),
            latency_ms: c.latency_ms,
            recv_latency_ms: c.recv_latency_ms,
            peer_latency_ms: c.peer_latency_ms,
            peer_idle_timeout_secs: c.peer_idle_timeout_secs,
            passphrase: c.passphrase.as_deref(),
            aes_key_len: c.aes_key_len,
            crypto_mode: c.crypto_mode.as_deref(),
            max_rexmit_bw: c.max_rexmit_bw,
            stream_id: c.stream_id.as_deref(),
            packet_filter: c.packet_filter.as_deref(),
            max_bw: c.max_bw,
            input_bw: c.input_bw,
            overhead_bw: c.overhead_bw,
            enforced_encryption: c.enforced_encryption,
            connect_timeout_secs: c.connect_timeout_secs,
            flight_flag_size: c.flight_flag_size,
            send_buffer_size: c.send_buffer_size,
            recv_buffer_size: c.recv_buffer_size,
            ip_tos: c.ip_tos,
            retransmit_algo: c.retransmit_algo.as_deref(),
            send_drop_delay: c.send_drop_delay,
            loss_max_ttl: c.loss_max_ttl,
            km_refresh_rate: c.km_refresh_rate,
            km_pre_announce: c.km_pre_announce,
            payload_size: c.payload_size,
            mss: c.mss,
            tlpkt_drop: c.tlpkt_drop,
            ip_ttl: c.ip_ttl,
        }
    }
}

impl<'a> From<&'a SrtOutputConfig> for SrtConnectionParams<'a> {
    fn from(c: &'a SrtOutputConfig) -> Self {
        Self {
            mode: &c.mode,
            local_addr: &c.local_addr,
            remote_addr: c.remote_addr.as_deref(),
            latency_ms: c.latency_ms,
            recv_latency_ms: c.recv_latency_ms,
            peer_latency_ms: c.peer_latency_ms,
            peer_idle_timeout_secs: c.peer_idle_timeout_secs,
            passphrase: c.passphrase.as_deref(),
            aes_key_len: c.aes_key_len,
            crypto_mode: c.crypto_mode.as_deref(),
            max_rexmit_bw: c.max_rexmit_bw,
            stream_id: c.stream_id.as_deref(),
            packet_filter: c.packet_filter.as_deref(),
            max_bw: c.max_bw,
            input_bw: c.input_bw,
            overhead_bw: c.overhead_bw,
            enforced_encryption: c.enforced_encryption,
            connect_timeout_secs: c.connect_timeout_secs,
            flight_flag_size: c.flight_flag_size,
            send_buffer_size: c.send_buffer_size,
            recv_buffer_size: c.recv_buffer_size,
            ip_tos: c.ip_tos,
            retransmit_algo: c.retransmit_algo.as_deref(),
            send_drop_delay: c.send_drop_delay,
            loss_max_ttl: c.loss_max_ttl,
            km_refresh_rate: c.km_refresh_rate,
            km_pre_announce: c.km_pre_announce,
            payload_size: c.payload_size,
            mss: c.mss,
            tlpkt_drop: c.tlpkt_drop,
            ip_ttl: c.ip_ttl,
        }
    }
}

impl<'a> From<&'a SrtRedundancyConfig> for SrtConnectionParams<'a> {
    fn from(c: &'a SrtRedundancyConfig) -> Self {
        Self {
            mode: &c.mode,
            local_addr: &c.local_addr,
            remote_addr: c.remote_addr.as_deref(),
            latency_ms: c.latency_ms,
            recv_latency_ms: c.recv_latency_ms,
            peer_latency_ms: c.peer_latency_ms,
            peer_idle_timeout_secs: c.peer_idle_timeout_secs,
            passphrase: c.passphrase.as_deref(),
            aes_key_len: c.aes_key_len,
            crypto_mode: c.crypto_mode.as_deref(),
            max_rexmit_bw: c.max_rexmit_bw,
            stream_id: c.stream_id.as_deref(),
            packet_filter: c.packet_filter.as_deref(),
            max_bw: c.max_bw,
            input_bw: c.input_bw,
            overhead_bw: c.overhead_bw,
            enforced_encryption: c.enforced_encryption,
            connect_timeout_secs: c.connect_timeout_secs,
            flight_flag_size: c.flight_flag_size,
            send_buffer_size: c.send_buffer_size,
            recv_buffer_size: c.recv_buffer_size,
            ip_tos: c.ip_tos,
            retransmit_algo: c.retransmit_algo.as_deref(),
            send_drop_delay: c.send_drop_delay,
            loss_max_ttl: c.loss_max_ttl,
            km_refresh_rate: c.km_refresh_rate,
            km_pre_announce: c.km_pre_announce,
            payload_size: c.payload_size,
            mss: c.mss,
            tlpkt_drop: c.tlpkt_drop,
            ip_ttl: c.ip_ttl,
        }
    }
}

fn parse_retransmit_algo(s: Option<&str>) -> RetransmitAlgo {
    match s {
        Some("reduced") => RetransmitAlgo::Reduced,
        _ => RetransmitAlgo::Default,
    }
}

/// Apply the common SRT advanced options to a socket builder.
fn apply_advanced_options(mut builder: SrtSocketBuilder, p: &SrtConnectionParams) -> SrtSocketBuilder {
    if let Some(bw) = p.max_bw { builder = builder.max_bw(bw); }
    if let Some(bw) = p.input_bw { builder = builder.input_bw(bw); }
    if let Some(pct) = p.overhead_bw { builder = builder.overhead_bw(pct); }
    if let Some(e) = p.enforced_encryption { builder = builder.enforced_encryption(e); }
    if let Some(t) = p.connect_timeout_secs { builder = builder.connect_timeout(Duration::from_secs(t)); }
    if let Some(s) = p.flight_flag_size { builder = builder.flight_flag_size(s); }
    if let Some(s) = p.send_buffer_size { builder = builder.send_buffer_size(s * 1316); }
    if let Some(s) = p.recv_buffer_size { builder = builder.recv_buffer_size(s * 1316); }
    if let Some(t) = p.ip_tos { builder = builder.ip_tos(t); }
    if p.retransmit_algo.is_some() { builder = builder.retransmit_algo(parse_retransmit_algo(p.retransmit_algo)); }
    if let Some(d) = p.send_drop_delay { builder = builder.send_drop_delay(d); }
    if let Some(t) = p.loss_max_ttl { builder = builder.loss_max_ttl(t); }
    if let Some(r) = p.km_refresh_rate { builder = builder.km_refresh_rate(r); }
    if let Some(r) = p.km_pre_announce { builder = builder.km_pre_announce(r); }
    if let Some(s) = p.payload_size { builder = builder.payload_size(s); }
    if let Some(s) = p.mss { builder = builder.mss(s); }
    if let Some(t) = p.tlpkt_drop { builder = builder.tlpkt_drop(t); }
    if let Some(t) = p.ip_ttl { builder = builder.ip_ttl(t); }
    builder
}

/// Build an [`SrtSocketBuilder`] from [`SrtConnectionParams`].
fn build_socket_builder(p: &SrtConnectionParams) -> SrtSocketBuilder {
    let timeout = if p.peer_idle_timeout_secs == 0 { 30 } else { p.peer_idle_timeout_secs };
    let mut builder = SrtSocket::builder()
        .latency(Duration::from_millis(p.latency_ms))
        .live_mode()
        .peer_idle_timeout(Duration::from_secs(timeout));

    if let Some(recv) = p.recv_latency_ms {
        builder = builder.receiver_latency(Duration::from_millis(recv));
    }
    if let Some(peer) = p.peer_latency_ms {
        builder = builder.sender_latency(Duration::from_millis(peer));
    }

    if let Some(pass) = p.passphrase {
        let key_size = match p.aes_key_len.unwrap_or(16) {
            24 => KeySize::AES192,
            32 => KeySize::AES256,
            _ => KeySize::AES128,
        };
        builder = builder.encryption(pass, key_size);
    }

    if p.crypto_mode == Some("aes-gcm") {
        builder = builder.crypto_mode(CryptoModeConfig::AesGcm);
    }

    if let Some(bw) = p.max_rexmit_bw {
        builder = builder.max_rexmit_bw(bw);
    }

    if let Some(sid) = p.stream_id {
        if !sid.is_empty() {
            builder = builder.stream_id(sid.to_string());
        }
    }

    if let Some(pf) = p.packet_filter {
        if !pf.is_empty() {
            builder = builder.packet_filter(pf.to_string());
        }
    }

    builder = apply_advanced_options(builder, p);
    builder
}

/// Apply common SRT advanced options to a listener builder.
fn apply_advanced_options_listener(
    mut lb: srt_transport::SrtListenerBuilder,
    p: &SrtConnectionParams,
) -> srt_transport::SrtListenerBuilder {
    if let Some(bw) = p.max_bw { lb = lb.max_bw(bw); }
    if let Some(bw) = p.input_bw { lb = lb.input_bw(bw); }
    if let Some(pct) = p.overhead_bw { lb = lb.overhead_bw(pct); }
    if let Some(e) = p.enforced_encryption { lb = lb.enforced_encryption(e); }
    if let Some(t) = p.connect_timeout_secs { lb = lb.connect_timeout(Duration::from_secs(t)); }
    if let Some(s) = p.flight_flag_size { lb = lb.flight_flag_size(s); }
    if let Some(s) = p.send_buffer_size { lb = lb.send_buffer_size(s * 1316); }
    if let Some(s) = p.recv_buffer_size { lb = lb.recv_buffer_size(s * 1316); }
    if let Some(t) = p.ip_tos { lb = lb.ip_tos(t); }
    if p.retransmit_algo.is_some() { lb = lb.retransmit_algo(parse_retransmit_algo(p.retransmit_algo)); }
    if let Some(d) = p.send_drop_delay { lb = lb.send_drop_delay(d); }
    if let Some(t) = p.loss_max_ttl { lb = lb.loss_max_ttl(t); }
    if let Some(r) = p.km_refresh_rate { lb = lb.km_refresh_rate(r); }
    if let Some(r) = p.km_pre_announce { lb = lb.km_pre_announce(r); }
    if let Some(s) = p.payload_size { lb = lb.payload_size(s); }
    if let Some(s) = p.mss { lb = lb.mss(s); }
    if let Some(t) = p.tlpkt_drop { lb = lb.tlpkt_drop(t); }
    if let Some(t) = p.ip_ttl { lb = lb.ip_ttl(t); }
    lb
}

/// Build a listener from [`SrtConnectionParams`].
fn build_listener_builder(p: &SrtConnectionParams) -> srt_transport::SrtListenerBuilder {
    let timeout = if p.peer_idle_timeout_secs == 0 { 30 } else { p.peer_idle_timeout_secs };
    let mut lb = SrtListener::builder()
        .latency(Duration::from_millis(p.latency_ms))
        .live_mode()
        .peer_idle_timeout(Duration::from_secs(timeout));

    if let Some(recv) = p.recv_latency_ms {
        lb = lb.receiver_latency(Duration::from_millis(recv));
    }
    if let Some(peer) = p.peer_latency_ms {
        lb = lb.sender_latency(Duration::from_millis(peer));
    }

    if let Some(pass) = p.passphrase {
        let key_size = match p.aes_key_len.unwrap_or(16) {
            24 => KeySize::AES192,
            32 => KeySize::AES256,
            _ => KeySize::AES128,
        };
        lb = lb.encryption(pass, key_size);
    }

    if p.crypto_mode == Some("aes-gcm") {
        lb = lb.crypto_mode(CryptoModeConfig::AesGcm);
    }

    if let Some(bw) = p.max_rexmit_bw {
        lb = lb.max_rexmit_bw(bw);
    }

    if let Some(pf) = p.packet_filter {
        if !pf.is_empty() {
            lb = lb.packet_filter(pf.to_string());
        }
    }

    lb = apply_advanced_options_listener(lb, p);
    lb
}

/// Connect an SRT socket based on mode (caller / listener).
pub async fn connect_srt(p: &SrtConnectionParams<'_>) -> Result<Arc<SrtSocket>> {
    match p.mode {
        SrtMode::Caller => {
            let remote = p.remote_addr
                .ok_or_else(|| anyhow::anyhow!("Caller mode requires remote_addr"))?;
            let remote_sa: SocketAddr = remote
                .parse()
                .context(format!("Invalid remote address: {remote}"))?;
            let local_sa: SocketAddr = p.local_addr
                .parse()
                .context(format!("Invalid local address: {}", p.local_addr))?;

            tracing::info!("SRT caller connecting {} -> {}", p.local_addr, remote);

            let builder = build_socket_builder(p);
            let sock = builder
                .bind(local_sa)
                .connect(remote_sa)
                .await
                .context(format!("SRT caller connect to {remote} failed"))?;

            tracing::info!("SRT caller connected to {}", remote);
            Ok(Arc::new(sock))
        }
        SrtMode::Listener => {
            let local_sa: SocketAddr = p.local_addr
                .parse()
                .context(format!("Invalid local address: {}", p.local_addr))?;

            tracing::info!("SRT listener waiting on {}", p.local_addr);

            let mut listener_builder = build_listener_builder(p);

            // Add access control for stream_id filtering on listener
            if let Some(expected_sid) = p.stream_id {
                if !expected_sid.is_empty() {
                    let expected = expected_sid.to_string();
                    listener_builder = listener_builder.access_control_fn(move |info| {
                        if info.stream_id == expected {
                            Ok(())
                        } else {
                            tracing::warn!(
                                "SRT listener: rejecting connection from {} — stream_id {:?} does not match expected {:?}",
                                info.peer_addr, info.stream_id, expected
                            );
                            Err(srt_protocol::error::RejectReason::Peer)
                        }
                    });
                }
            }

            let mut listener = listener_builder
                .bind(local_sa)
                .await
                .context(format!("SRT listener bind on {} failed", p.local_addr))?;

            let sock = listener
                .accept()
                .await
                .context(format!("SRT listener accept on {} failed", p.local_addr))?;

            // Close the listener after accepting one connection
            let _ = listener.close().await;

            tracing::info!("SRT listener accepted connection on {}", p.local_addr);
            Ok(Arc::new(sock))
        }
        SrtMode::Rendezvous => {
            let remote = p.remote_addr
                .ok_or_else(|| anyhow::anyhow!("Rendezvous mode requires remote_addr"))?;
            let remote_sa: SocketAddr = remote
                .parse()
                .context(format!("Invalid remote address: {remote}"))?;
            let local_sa: SocketAddr = p.local_addr
                .parse()
                .context(format!("Invalid local address: {}", p.local_addr))?;

            tracing::info!("SRT rendezvous connecting {} <-> {}", p.local_addr, remote);

            let builder = build_socket_builder(p);
            let sock = builder
                .rendezvous(true)
                .connect_rendezvous(local_sa, remote_sa)
                .await
                .context(format!("SRT rendezvous connect {} <-> {remote} failed", p.local_addr))?;

            tracing::info!("SRT rendezvous connected {} <-> {}", p.local_addr, remote);
            Ok(Arc::new(sock))
        }
    }
}

/// Connect with retry logic and exponential back-off.
///
/// Retries indefinitely until the connection succeeds or the
/// `CancellationToken` is triggered.
pub async fn connect_srt_with_retry(
    p: &SrtConnectionParams<'_>,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    let mut attempt = 0u32;
    let max_delay = Duration::from_secs(30);

    loop {
        match connect_srt(p).await {
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
    let p = SrtConnectionParams::from(config);
    connect_srt_with_retry(&p, cancel).await
}

/// Convenience wrapper: connect an SRT socket for an output using the
/// parameters in [`SrtOutputConfig`], with automatic retry.
pub async fn connect_srt_output(
    config: &SrtOutputConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    let p = SrtConnectionParams::from(config);
    connect_srt_with_retry(&p, cancel).await
}

/// Convenience wrapper: connect the SMPTE 2022-7 redundancy leg (leg 2)
/// using the parameters in [`SrtRedundancyConfig`], with automatic retry.
pub async fn connect_srt_redundancy_leg(
    redundancy: &SrtRedundancyConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    let p = SrtConnectionParams::from(redundancy);
    connect_srt_with_retry(&p, cancel).await
}

// ---------------------------------------------------------------------------
// Persistent SRT listener helpers
// ---------------------------------------------------------------------------

/// Bind an SRT listener without accepting any connection.
pub async fn bind_srt_listener(p: &SrtConnectionParams<'_>) -> Result<SrtListener> {
    let local_sa: SocketAddr = p.local_addr
        .parse()
        .context(format!("Invalid local address: {}", p.local_addr))?;

    let mut listener_builder = build_listener_builder(p);

    // Add access control for stream_id filtering on listener
    if let Some(expected_sid) = p.stream_id {
        if !expected_sid.is_empty() {
            let expected = expected_sid.to_string();
            listener_builder = listener_builder.access_control_fn(move |info| {
                if info.stream_id == expected {
                    Ok(())
                } else {
                    tracing::warn!(
                        "SRT listener: rejecting connection from {} — stream_id {:?} does not match expected {:?}",
                        info.peer_addr, info.stream_id, expected
                    );
                    Err(srt_protocol::error::RejectReason::Peer)
                }
            });
        }
    }

    let listener = listener_builder
        .bind(local_sa)
        .await
        .context(format!("SRT listener bind on {} failed", p.local_addr))?;

    tracing::info!("SRT listener bound on {}", p.local_addr);
    Ok(listener)
}

/// Accept a single incoming SRT connection on an existing listener.
pub async fn accept_srt_connection(
    listener: &mut SrtListener,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    let local_addr = listener.local_addr();
    tracing::info!("SRT listener waiting for connection on {}", local_addr);

    tokio::select! {
        _ = cancel.cancelled() => {
            bail!("SRT accept cancelled");
        }
        result = listener.accept() => {
            let sock = result.context(format!(
                "SRT listener accept on {} failed", local_addr
            ))?;
            tracing::info!("SRT listener accepted connection on {}", local_addr);
            Ok(Arc::new(sock))
        }
    }
}

/// Bind an SRT listener for an output using [`SrtOutputConfig`] parameters.
pub async fn bind_srt_listener_for_output(config: &SrtOutputConfig) -> Result<SrtListener> {
    let p = SrtConnectionParams::from(config);
    bind_srt_listener(&p).await
}

/// Bind an SRT listener for an input using [`SrtInputConfig`] parameters.
pub async fn bind_srt_listener_for_input(config: &SrtInputConfig) -> Result<SrtListener> {
    let p = SrtConnectionParams::from(config);
    bind_srt_listener(&p).await
}

/// Bind an SRT listener for a redundancy leg using [`SrtRedundancyConfig`] parameters.
pub async fn bind_srt_listener_for_redundancy(config: &SrtRedundancyConfig) -> Result<SrtListener> {
    let p = SrtConnectionParams::from(config);
    bind_srt_listener(&p).await
}

// ---------------------------------------------------------------------------
// SRT stats conversion
// ---------------------------------------------------------------------------

/// Convert [`srt_protocol::stats::SrtStats`] into the edge API [`SrtLegStats`].
pub fn convert_srt_stats(stats: &srt_protocol::stats::SrtStats) -> SrtLegStats {
    SrtLegStats {
        state: "connected".to_string(),
        rtt_ms: stats.ms_rtt,
        send_rate_mbps: stats.mbps_send_rate,
        recv_rate_mbps: stats.mbps_recv_rate,
        bandwidth_mbps: stats.mbps_bandwidth,
        max_bw_mbps: stats.mbps_max_bw,

        // Cumulative counters
        pkt_sent_total: stats.pkt_sent_total,
        pkt_recv_total: stats.pkt_recv_total,
        pkt_loss_total: (stats.pkt_snd_loss_total as i64) + (stats.pkt_rcv_loss_total as i64),
        pkt_send_loss_total: stats.pkt_snd_loss_total,
        pkt_recv_loss_total: stats.pkt_rcv_loss_total,
        pkt_retransmit_total: stats.pkt_retrans_total,
        pkt_recv_retransmit_total: stats.pkt_rcv_retrans_total,
        pkt_recv_drop_total: stats.pkt_rcv_drop_total,
        pkt_send_drop_total: stats.pkt_snd_drop_total,
        pkt_recv_undecrypt_total: stats.pkt_rcv_undecrypt_total,
        byte_sent_total: stats.byte_sent_total,
        byte_recv_total: stats.byte_recv_total,
        byte_retrans_total: stats.byte_retrans_total,
        byte_recv_drop_total: stats.byte_rcv_drop_total,
        byte_recv_loss_total: stats.byte_rcv_loss_total,
        byte_send_drop_total: stats.byte_snd_drop_total,
        byte_recv_undecrypt_total: stats.byte_rcv_undecrypt_total,
        pkt_sent_unique_total: stats.pkt_sent_unique_total,
        pkt_recv_unique_total: stats.pkt_recv_unique_total,
        byte_sent_unique_total: stats.byte_sent_unique_total,
        byte_recv_unique_total: stats.byte_recv_unique_total,

        // ACK/NAK
        pkt_sent_ack_total: stats.pkt_sent_ack_total,
        pkt_recv_ack_total: stats.pkt_recv_ack_total,
        pkt_sent_nak_total: stats.pkt_sent_nak_total,
        pkt_recv_nak_total: stats.pkt_recv_nak_total,

        // Flow control / buffer state
        pkt_flow_window: stats.pkt_flow_window,
        pkt_congestion_window: stats.pkt_congestion_window,
        pkt_flight_size: stats.pkt_flight_size,
        byte_avail_send_buf: stats.byte_avail_snd_buf,
        byte_avail_recv_buf: stats.byte_avail_rcv_buf,
        ms_send_buf: stats.ms_snd_buf,
        ms_recv_buf: stats.ms_rcv_buf,
        ms_send_tsbpd_delay: stats.ms_snd_tsbpd_delay,
        ms_recv_tsbpd_delay: stats.ms_rcv_tsbpd_delay,

        // Buffer occupancy
        pkt_send_buf: stats.pkt_snd_buf,
        byte_send_buf: stats.byte_snd_buf,
        pkt_recv_buf: stats.pkt_rcv_buf,
        byte_recv_buf: stats.byte_rcv_buf,

        // Pacing
        us_pkt_send_period: stats.us_pkt_snd_period,

        // Reorder / belated
        pkt_reorder_distance: stats.pkt_reorder_distance,
        pkt_reorder_tolerance: stats.pkt_reorder_tolerance,
        pkt_recv_belated: stats.pkt_rcv_belated,
        pkt_recv_avg_belated_time: stats.pkt_rcv_avg_belated_time,

        // FEC stats
        pkt_send_filter_extra_total: stats.pkt_snd_filter_extra_total,
        pkt_recv_filter_extra_total: stats.pkt_rcv_filter_extra_total,
        pkt_recv_filter_supply_total: stats.pkt_rcv_filter_supply_total,
        pkt_recv_filter_loss_total: stats.pkt_rcv_filter_loss_total,
        pkt_send_filter_extra: stats.pkt_snd_filter_extra,
        pkt_recv_filter_supply: stats.pkt_rcv_filter_supply,
        pkt_recv_filter_loss: stats.pkt_rcv_filter_loss,

        uptime_ms: stats.ms_timestamp,
    }
}

/// Spawn a background task that polls SRT socket stats every second and
/// publishes the result via the provided lock-free watch channel. Runs
/// until the cancellation token fires.
pub fn spawn_srt_stats_poller(
    socket: Arc<SrtSocket>,
    cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let stats = socket.stats().await;
                    let leg_stats = convert_srt_stats(&stats);
                    let _ = cache.send(Some(leg_stats));
                }
            }
        }
    });
}
