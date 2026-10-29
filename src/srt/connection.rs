// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use srt_protocol::access_control::StreamIdInfo;
use srt_protocol::config::{CryptoModeConfig, KeySize, MemberStatus, RetransmitAlgo};
use srt_transport::{GroupMode, SrtGroup, SrtGroupBuilder, SrtListener, SrtSocket, SrtSocketBuilder};

use crate::config::models::{
    SrtBondingConfig, SrtBondingMode, SrtInputConfig, SrtMode, SrtOutputConfig,
    SrtRedundancyConfig,
};
use crate::srt::bond_supervisor::{
    BondHealthTracker, BondMemberPlan, BondSupervisor, bond_attempt_endpoints,
    describe_connect_error, observe_members, plan_bond_members,
};
use crate::stats::collector::{SrtInputSocket, SrtInputStats};
use crate::stats::models::{SrtBondingMemberStats, SrtBondingStats, SrtLegStats};

/// Common SRT connection parameters extracted from any SRT config type.
///
/// This struct avoids passing 20+ positional parameters through the connection
/// pipeline. Use `From<&SrtInputConfig>`, `From<&SrtOutputConfig>`, or
/// `From<&SrtRedundancyConfig>` to construct.
pub struct SrtConnectionParams<'a> {
    pub mode: &'a SrtMode,
    pub local_addr: Option<&'a str>,
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
            local_addr: c.local_addr.as_deref(),
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
            local_addr: c.local_addr.as_deref(),
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
            local_addr: c.local_addr.as_deref(),
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

    if let Some(sid) = p.stream_id
        && !sid.is_empty() {
            builder = builder.stream_id(sid.to_string());
        }

    if let Some(pf) = p.packet_filter
        && !pf.is_empty() {
            builder = builder.packet_filter(pf.to_string());
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

    if let Some(pf) = p.packet_filter
        && !pf.is_empty() {
            lb = lb.packet_filter(pf.to_string());
        }

    lb = apply_advanced_options_listener(lb, p);
    lb
}

/// Does a caller's presented `stream_id` satisfy the configured one?
///
/// Compared per the **SRT Access Control spec**, not byte-for-byte. Both
/// `docs/CONFIGURATION.md` and `docs/supported-protocols.md` promise that
/// `stream_id` "supports plain strings or the structured
/// `#!::r=name,m=publish,u=user` format", and `StreamIdInfo::parse` maps a bare
/// string onto `resource` — so a configured `mystream` matches a caller sending
/// `#!::r=mystream,m=publish`, which is what OBS Studio and ffmpeg emit once a
/// stream key is set, and vice versa.
///
/// That promise cost nothing while this filter was dead: until the 2026-08
/// libsrt-rs fix, `srt_listen_callback` was registered *after* `srt_listen`,
/// which libsrt refuses silently, so no access-control policy on any SRT
/// listener ever ran. Now that it does run, a byte-for-byte comparison would
/// take a working `#!::`-emitting ingest dark on upgrade.
///
/// This only ever **widens** the match — an exact match is still a match — so
/// it cannot reject a caller that the verbatim comparison would have admitted.
fn stream_id_matches(expected: &str, presented: &str) -> bool {
    if presented == expected {
        return true;
    }
    let want = StreamIdInfo::parse(expected);
    let got = StreamIdInfo::parse(presented);
    want.resource.is_some() && want.resource == got.resource
}

/// Install the `stream_id` access-control filter on a listener builder, if one
/// is configured. Shared by every listener bind path so the two cannot drift.
fn apply_stream_id_access_control(
    mut lb: srt_transport::SrtListenerBuilder,
    p: &SrtConnectionParams,
) -> srt_transport::SrtListenerBuilder {
    if let Some(expected_sid) = p.stream_id
        && !expected_sid.is_empty()
    {
        let expected = expected_sid.to_string();
        lb = lb.access_control_fn(move |info| {
            if stream_id_matches(&expected, &info.stream_id) {
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
            let local_addr_str = p.local_addr.unwrap_or("0.0.0.0:0");
            let local_sa: SocketAddr = local_addr_str
                .parse()
                .context(format!("Invalid local address: {}", local_addr_str))?;

            tracing::info!("SRT caller connecting {} -> {}", local_addr_str, remote);

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
            let local_addr_str = p.local_addr
                .ok_or_else(|| anyhow::anyhow!("Listener mode requires local_addr"))?;
            let local_sa: SocketAddr = local_addr_str
                .parse()
                .context(format!("Invalid local address: {}", local_addr_str))?;

            tracing::info!("SRT listener waiting on {}", local_addr_str);

            let listener_builder = apply_stream_id_access_control(build_listener_builder(p), p);

            let mut listener = listener_builder
                .bind(local_sa)
                .await
                .context(format!("SRT listener bind on {} failed", local_addr_str))?;

            let sock = listener
                .accept()
                .await
                .context(format!("SRT listener accept on {} failed", local_addr_str))?;

            // Close the listener after accepting one connection
            let _ = listener.close().await;

            tracing::info!("SRT listener accepted connection on {}", local_addr_str);
            Ok(Arc::new(sock))
        }
        SrtMode::Rendezvous => {
            let remote = p.remote_addr
                .ok_or_else(|| anyhow::anyhow!("Rendezvous mode requires remote_addr"))?;
            let remote_sa: SocketAddr = remote
                .parse()
                .context(format!("Invalid remote address: {remote}"))?;
            let local_addr_str = p.local_addr
                .ok_or_else(|| anyhow::anyhow!("Rendezvous mode requires local_addr"))?;
            let local_sa: SocketAddr = local_addr_str
                .parse()
                .context(format!("Invalid local address: {}", local_addr_str))?;

            tracing::info!("SRT rendezvous connecting {} <-> {}", local_addr_str, remote);

            let builder = build_socket_builder(p);
            let sock = builder
                .rendezvous(true)
                .connect_rendezvous(local_sa, remote_sa)
                .await
                .context(format!("SRT rendezvous connect {} <-> {remote} failed", local_addr_str))?;

            tracing::info!("SRT rendezvous connected {} <-> {}", local_addr_str, remote);
            Ok(Arc::new(sock))
        }
    }
}

/// Consecutive failed connect attempts after which a caller-mode input is
/// considered connect-failed: the input's reported stats state flips to
/// `"connect_failed"` and the latched Critical event fires. Shared by the
/// retry observer in `engine::input_srt` and the snapshot path in
/// `stats::collector`.
pub const CONNECT_FAILED_THRESHOLD: u32 = 5;

/// Connect with retry logic and exponential back-off.
///
/// Retries indefinitely until the connection succeeds or the
/// `CancellationToken` is triggered.
pub async fn connect_srt_with_retry(
    p: &SrtConnectionParams<'_>,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    connect_srt_with_retry_observed(p, cancel, &mut |_, _| {}).await
}

/// Like [`connect_srt_with_retry`], invoking `on_attempt_failed` with the
/// consecutive-failure count after every failed attempt (before the
/// back-off sleep). The loop never returns `Err` except on cancellation,
/// so the observer is the only way a caller can surface a never-connecting
/// peer while retries continue.
pub async fn connect_srt_with_retry_observed(
    p: &SrtConnectionParams<'_>,
    cancel: &CancellationToken,
    on_attempt_failed: &mut (dyn FnMut(u32, &anyhow::Error) + Send),
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
                on_attempt_failed(attempt, &e);

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
    connect_srt_output_observed(config, cancel, &mut |_, _| {}).await
}

/// Like [`connect_srt_output`], with the per-attempt failure observer.
///
/// `connect_srt_with_retry` retries **forever** (30 s cap) and only logs, so
/// without an observer a caller-mode output that cannot reach its peer is
/// completely silent after its first event — for the whole outage. Mirrors
/// [`connect_srt_redundancy_leg_observed`], which is the existing precedent.
pub async fn connect_srt_output_observed(
    config: &SrtOutputConfig,
    cancel: &CancellationToken,
    on_attempt_failed: &mut (dyn FnMut(u32, &anyhow::Error) + Send),
) -> Result<Arc<SrtSocket>> {
    let p = SrtConnectionParams::from(config);
    connect_srt_with_retry_observed(&p, cancel, on_attempt_failed).await
}

/// Convenience wrapper: connect the SMPTE 2022-7 redundancy leg (leg 2)
/// using the parameters in [`SrtRedundancyConfig`], with automatic retry.
pub async fn connect_srt_redundancy_leg(
    redundancy: &SrtRedundancyConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtSocket>> {
    connect_srt_redundancy_leg_observed(redundancy, cancel, &mut |_, _| {}).await
}

/// Like [`connect_srt_redundancy_leg`], with the per-attempt failure
/// observer (see [`connect_srt_with_retry_observed`]).
pub async fn connect_srt_redundancy_leg_observed(
    redundancy: &SrtRedundancyConfig,
    cancel: &CancellationToken,
    on_attempt_failed: &mut (dyn FnMut(u32, &anyhow::Error) + Send),
) -> Result<Arc<SrtSocket>> {
    let p = SrtConnectionParams::from(redundancy);
    connect_srt_with_retry_observed(&p, cancel, on_attempt_failed).await
}

// ---------------------------------------------------------------------------
// Persistent SRT listener helpers
// ---------------------------------------------------------------------------

/// Bind an SRT listener without accepting any connection.
pub async fn bind_srt_listener(p: &SrtConnectionParams<'_>) -> Result<SrtListener> {
    let local_addr_str = p.local_addr
        .ok_or_else(|| anyhow::anyhow!("Listener mode requires local_addr"))?;
    let local_sa: SocketAddr = local_addr_str
        .parse()
        .context(format!("Invalid local address: {}", local_addr_str))?;

    let listener_builder = apply_stream_id_access_control(build_listener_builder(p), p);

    let listener = listener_builder
        .bind(local_sa)
        .await
        .map_err(|e| {
            let err_str = e.to_string();
            if err_str.contains("Address already in use")
                || err_str.contains("address already in use")
                || err_str.contains("EADDRINUSE")
            {
                // Inject a `PortInUse` marker so `anyhow_is_addr_in_use()`
                // detects this from string-based FFI errors. The user-facing
                // top-line message is unchanged.
                anyhow::Error::new(crate::util::port_error::PortInUse).context(format!(
                    "Port conflict: SRT listener could not bind to {local_addr_str} \
                     — address already in use. \
                     Check whether another service or edge component is using this port."
                ))
            } else {
                anyhow::anyhow!("SRT listener bind on {local_addr_str} failed: {e}")
            }
        })?;

    tracing::info!("SRT listener bound on {}", local_addr_str);
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

/// A socket's or a group's cumulative counters as its stats poller last read
/// them: one per poller, so one per socket or group.
///
/// libsrt's counters are cumulative **per socket**, so they restart at zero
/// on every reconnect. The poller of the new socket starts a fresh cursor and
/// hands on what moved, which the input adds to its running totals: that is
/// what makes them survive a reconnect instead of snapping back to zero.
#[derive(Default)]
pub(crate) struct LegCursor {
    /// `pktRcvDropTotal`, `pktRecvUniqueTotal`, `pktSentTotal`,
    /// `pktRecvTotal` and `msTimeStamp` of the last good reading.
    last: [i64; 5],
}

impl LegCursor {
    /// How far `reading` moved the too-late drop and delivered-packet
    /// counters (`pktRcvDropTotal`, `pktRecvUniqueTotal`) since the last good
    /// one. `None` for a failed read: `SrtSocket::stats` and `SrtGroup::stats`
    /// answer all zeros when libsrt refuses the query, so any of the counters
    /// it is read by going backwards is one. It is no sample — the cursor
    /// stays, so the next good read is measured from the last good one;
    /// taken as the new baseline, it made the next good read count the
    /// socket's whole drop history again.
    fn advance(&mut self, reading: &SrtLegStats) -> Option<(u64, u64)> {
        let now = [
            i64::from(reading.pkt_recv_drop_total),
            reading.pkt_recv_unique_total,
            reading.pkt_sent_total,
            reading.pkt_recv_total,
            reading.uptime_ms,
        ];
        if now.iter().zip(&self.last).any(|(now, last)| now < last) {
            return None;
        }
        let moved = ((now[0] - self.last[0]) as u64, (now[1] - self.last[1]) as u64);
        self.last = now;
        Some(moved)
    }
}

/// One tick of [`spawn_srt_stats_poller`] once the stats are read: hand the
/// socket's new too-late drops to `input` and publish the stats.
///
/// Publishing uses `send_replace`, not `send`: every one of these caches is
/// built as `watch::channel(None).0`, i.e. with its only receiver already
/// dropped, and `watch::Sender::send` refuses to store a value when there
/// are no receivers. With `send` the cache stayed `None` forever and
/// `srt_stats` was absent from every SRT input and output on the node.
///
/// A failed read ([`LegCursor::advance`]) is not published: the cache keeps
/// the last good reading. Published, its zeros read as a reset of
/// `bilbycast_edge_srt_loss_total`, a Prometheus counter, which `increase()`
/// then counted twice, and as a 0 ms latency and RTT in the drop reason.
pub(crate) fn publish_leg_stats(
    cache: &watch::Sender<Option<SrtLegStats>>,
    input: Option<(&SrtInputStats, SrtInputSocket)>,
    cursor: &mut LegCursor,
    leg_stats: SrtLegStats,
) {
    let moved = cursor.advance(&leg_stats);
    // Every poll, drops or not, and a failed read as one that found and
    // delivered nothing: a 2022-7 leg's poll also takes the merger's count
    // of what neither leg delivered, what the leg delivered is whether its
    // drops can explain that, and the pair's loss waits for the leg's polls.
    // Skipped, a leg whose reads kept failing held that loss back for good,
    // its cache still up.
    if let Some((input, socket)) = input {
        let (drops, delivered) = moved.unwrap_or_default();
        input.count_socket_drops(socket, drops, delivered);
    }
    if moved.is_some() {
        cache.send_replace(Some(leg_stats));
    }
}

/// The group counterpart of [`publish_leg_stats`], a failed read of the group
/// published no more than a socket's. Drops are read off the group aggregate
/// — libsrt's own count of packets no member delivered — never summed over
/// members: on a broadcast group a member skips packets another member
/// already delivered, which is no loss at all. They are counted when polled,
/// which is what decides whether they are the flow's: see
/// [`SrtInputStats::count_recv_drops`].
pub(crate) fn publish_bonding_stats(
    cache: &watch::Sender<Option<SrtBondingStats>>,
    input: Option<&SrtInputStats>,
    cursor: &mut LegCursor,
    snapshot: SrtBondingStats,
) {
    let Some((drops, _)) = cursor.advance(&snapshot.aggregate) else {
        return;
    };
    if let Some(input) = input
        && drops > 0
    {
        input.count_recv_drops(drops);
    }
    cache.send_replace(Some(snapshot));
}

/// Turns libsrt's since-connect figures into per-poll ones.
///
/// The stats are read without clearing (`srt_bistats(.., clear = 0, ..)` in
/// bilbycast-libsrt-rs), so libsrt's `mbpsSendRate` / `mbpsRecvRate` average
/// everything since the socket connected and its "interval" FEC counters
/// hold since-connect totals: a link that ran at 8 Mbps for ten hours still
/// read over 7 Mbps an hour after its source fell to 1. Each poller keeps
/// one sampler per socket and rewrites those fields as what moved since its
/// previous tick. Clearing in libsrt instead would reset the counters under
/// every other `stats()` caller.
#[derive(Default)]
struct IntervalSampler {
    prev: Option<IntervalSample>,
}

#[derive(Clone, Copy)]
struct IntervalSample {
    at: Instant,
    bytes_sent: u64,
    bytes_recv: u64,
    filter_extra: i32,
    filter_supply: i32,
    filter_loss: i32,
}

impl IntervalSampler {
    /// Rewrite `leg`'s send / receive rates and interval FEC counters as what
    /// moved since the previous call, the rates measured on `bytes_sent` /
    /// `bytes_recv`. The first call keeps libsrt's figures, which then cover
    /// the same span. A reading whose byte counters went backwards — a
    /// failed read comes back zeroed — is left as read and does not move the
    /// baseline, so the next good one is measured from the last good one;
    /// the poller does not publish it ([`publish_leg_stats`]).
    fn apply(&mut self, leg: &mut SrtLegStats, bytes_sent: u64, bytes_recv: u64, now: Instant) {
        let cur = IntervalSample {
            at: now,
            bytes_sent,
            bytes_recv,
            filter_extra: leg.pkt_send_filter_extra,
            filter_supply: leg.pkt_recv_filter_supply,
            filter_loss: leg.pkt_recv_filter_loss,
        };
        let Some(prev) = self.prev else {
            self.prev = Some(cur);
            return;
        };
        if bytes_sent < prev.bytes_sent || bytes_recv < prev.bytes_recv {
            return;
        }
        let elapsed_us = now.saturating_duration_since(prev.at).as_micros() as f64;
        if elapsed_us > 0.0 {
            leg.send_rate_mbps = (bytes_sent - prev.bytes_sent) as f64 * 8.0 / elapsed_us;
            leg.recv_rate_mbps = (bytes_recv - prev.bytes_recv) as f64 * 8.0 / elapsed_us;
        }
        leg.pkt_send_filter_extra = (cur.filter_extra - prev.filter_extra).max(0);
        leg.pkt_recv_filter_supply = (cur.filter_supply - prev.filter_supply).max(0);
        leg.pkt_recv_filter_loss = (cur.filter_loss - prev.filter_loss).max(0);
        self.prev = Some(cur);
    }
}

/// A 1 s poll interval that never fires back-to-back: after a stall the next
/// tick waits a full period, so every interval the samplers measure is about
/// a second long rather than a burst of near-zero ones.
fn poll_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Spawn a background task that polls SRT socket stats every second and
/// publishes the result via the provided lock-free watch channel. Runs
/// until the cancellation token fires.
///
/// `input`, when given, is the SRT input this socket serves and which of its
/// sockets it is: each poll hands it the socket's new receiver-side too-late
/// drops (`pktRcvDropTotal`), so they add up across reconnects. Those are
/// packets libsrt gave up on because they missed the TSBPD deadline — the
/// only SRT loss the application never sees, since libsrt simply skips them
/// and delivers the next packet. A single-socket input counts them all; a
/// leg of a SMPTE 2022-7 input counts what the pair lost instead (see
/// [`SrtInputStats::count_socket_drops`]). Counted while the input is on
/// air, they reach `InputStats.packets_lost` and the flow health; outputs
/// pass `None`.
pub fn spawn_srt_stats_poller(
    socket: Arc<SrtSocket>,
    cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    input: Option<(Arc<SrtInputStats>, SrtInputSocket)>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = poll_interval();
        let mut cursor = LegCursor::default();
        let mut rates = IntervalSampler::default();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let stats = socket.stats().await;
                    let mut leg_stats = convert_srt_stats(&stats);
                    rates.apply(
                        &mut leg_stats,
                        stats.byte_sent_total,
                        stats.byte_recv_total,
                        Instant::now(),
                    );
                    publish_leg_stats(
                        &cache,
                        input.as_ref().map(|(input, socket)| (input.as_ref(), *socket)),
                        &mut cursor,
                        leg_stats,
                    );
                }
            }
        }
        // The socket is gone. A snapshot that still says "connected" with a
        // healthy RTT would be a lie for as long as the reconnect takes.
        cache.send_replace(None);
    });
}

// ── SRT bonding (native libsrt socket group) helpers ──

fn socket_status_str(s: srt_protocol::config::SocketStatus) -> &'static str {
    use srt_protocol::config::SocketStatus::*;
    match s {
        Init => "init",
        Opened => "opened",
        Listening => "listening",
        Connecting => "connecting",
        Connected => "connected",
        Broken => "broken",
        Closing => "closing",
        Closed => "closed",
        NonExist => "nonexist",
    }
}

fn member_status_str(s: MemberStatus) -> &'static str {
    match s {
        MemberStatus::Pending => "pending",
        MemberStatus::Idle => "idle",
        MemberStatus::Running => "running",
        MemberStatus::Broken => "broken",
    }
}

fn bonding_mode_str(m: &SrtBondingMode) -> &'static str {
    match m {
        SrtBondingMode::Broadcast => "broadcast",
        SrtBondingMode::Backup => "backup",
    }
}

pub(crate) fn srt_bonding_mode_to_group_mode(m: &SrtBondingMode) -> GroupMode {
    match m {
        SrtBondingMode::Broadcast => GroupMode::Broadcast,
        SrtBondingMode::Backup => GroupMode::Backup,
    }
}

/// Build an [`SrtGroupBuilder`] (bonded caller) from the SRT options on the
/// parent input/output config, plus the bonding mode. Members are added by
/// the caller with `.add_member(...)`.
pub fn build_group_builder(p: &SrtConnectionParams, mode: GroupMode) -> SrtGroupBuilder {
    let timeout = if p.peer_idle_timeout_secs == 0 { 30 } else { p.peer_idle_timeout_secs };
    let mut builder = SrtGroup::builder(mode)
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

    if let Some(sid) = p.stream_id
        && !sid.is_empty() {
            builder = builder.stream_id(sid.to_string());
        }

    if let Some(pf) = p.packet_filter
        && !pf.is_empty() {
            builder = builder.packet_filter(pf.to_string());
        }

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

/// Connect a bonded SRT group (caller side) given the parent SRT config
/// and a bonding block. Each endpoint in `bond.endpoints` becomes one
/// group member, with its source (`local_addr`, else the address of the
/// NIC named by `interface_binding`, resolved afresh on every attempt), its
/// libsrt weight and its index as the member token — see
/// [`crate::srt::bond_supervisor`]. With automatic retry on transient errors.
pub async fn connect_srt_group(
    p: &SrtConnectionParams<'_>,
    bond: &SrtBondingConfig,
    cancel: &CancellationToken,
) -> Result<Arc<SrtGroup>> {
    connect_srt_group_observed(p, bond, cancel, &mut |_, _| {}).await
}

/// Like [`connect_srt_group`], with the per-attempt failure observer
/// (see [`connect_srt_with_retry_observed`]) so a never-connecting bonded
/// caller can surface `connect_failed` state + the latched Critical event.
pub async fn connect_srt_group_observed(
    p: &SrtConnectionParams<'_>,
    bond: &SrtBondingConfig,
    cancel: &CancellationToken,
    on_attempt_failed: &mut (dyn FnMut(u32, &anyhow::Error) + Send),
) -> Result<Arc<SrtGroup>> {
    if !matches!(p.mode, SrtMode::Caller) {
        bail!("SRT bonding caller-connect requires caller mode");
    }
    let plans = plan_bond_members(bond)?;
    let mode = srt_bonding_mode_to_group_mode(&bond.mode);

    // Retry with exponential back-off, same shape as single-socket connect.
    let mut delay_ms: u64 = 250;
    let mut attempts = 0u32;
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled before SRT group connect");
        }
        // Sources are resolved per attempt (a NIC's address may have
        // changed), and a member whose NIC is missing is left out of this
        // attempt rather than failing it — unless none is left.
        let attempt = match bond_attempt_endpoints(&plans) {
            Ok(members) => {
                let sources: Vec<SocketAddr> = members.iter().filter_map(|m| m.local_addr).collect();
                let mut builder = build_group_builder(p, mode);
                for member in members {
                    builder = builder.add_member(member);
                }
                builder
                    .connect()
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", describe_connect_error(&e, &sources)))
            }
            Err(e) => Err(e),
        };
        match attempt {
            Ok(group) => return Ok(group),
            Err(err) => {
                attempts += 1;
                tracing::warn!("SRT group connect failed: {err}; retrying in {delay_ms}ms");
                on_attempt_failed(attempts, &err);
                tokio::select! {
                    _ = cancel.cancelled() => bail!("cancelled during SRT group reconnect"),
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }
                delay_ms = (delay_ms * 2).min(5000);
            }
        }
    }
}

/// Assemble a group stats snapshot from the polled aggregate and members,
/// every rate measured over the poll interval. `samplers` holds one
/// [`IntervalSampler`] per member socket id and is pruned to the members
/// present, so a member that leaves takes its baseline with it.
///
/// `plans` is the caller group's member plan, when there is one: a member's
/// `weight` is then the **configured** weight of its endpoint (lower is
/// preferred) rather than the rank libsrt was given. A listener's members
/// have no plan and show the weight the remote caller sent (libsrt's,
/// greater is preferred).
fn bonding_snapshot(
    mode: &SrtBondingMode,
    agg_raw: &srt_protocol::stats::SrtStats,
    state: srt_protocol::config::SocketStatus,
    members: Vec<srt_transport::GroupMemberStats>,
    aggregate_rates: &mut IntervalSampler,
    samplers: &mut HashMap<i32, IntervalSampler>,
    plans: Option<&[BondMemberPlan]>,
    now: Instant,
) -> SrtBondingStats {
    let mut aggregate = convert_srt_stats(agg_raw);
    aggregate.state = socket_status_str(state).to_string();
    // libsrt's group stats (`bstatsSocket`) never fill `byteSent` /
    // `byteRecv`, so their own rate is always 0: measure the group on the
    // unique bytes it did count.
    aggregate_rates.apply(
        &mut aggregate,
        agg_raw.byte_sent_unique_total,
        agg_raw.byte_recv_unique_total,
        now,
    );
    samplers.retain(|id, _| members.iter().any(|m| m.id == *id));
    let members = members
        .into_iter()
        .map(|m| {
            let endpoint = m
                .peer_addr
                .map(|a| a.to_string())
                .unwrap_or_else(String::new);
            let mut stats = convert_srt_stats(&m.stats);
            samplers.entry(m.id).or_default().apply(
                &mut stats,
                m.stats.byte_sent_total,
                m.stats.byte_recv_total,
                now,
            );
            let weight = plans
                .and_then(|plans| plans.iter().find(|p| p.token == m.token))
                .map_or(m.weight, |p| p.configured_weight);
            SrtBondingMemberStats {
                endpoint,
                socket_status: socket_status_str(m.socket_status).to_string(),
                member_status: member_status_str(m.member_status).to_string(),
                weight,
                stats,
            }
        })
        .collect();
    SrtBondingStats {
        mode: bonding_mode_str(mode).to_string(),
        aggregate,
        members,
    }
}

/// Spawn a background task that polls per-member SRT group stats every
/// second and publishes the snapshot via the provided lock-free watch
/// channel. Runs until the cancellation token fires.
///
/// `input` is the bonded SRT input the group serves, as on
/// [`spawn_srt_stats_poller`], fed from the group's own drop count: inputs
/// pass themselves, outputs `None`.
///
/// `supervisor`, on a caller group, runs on the same tick after the
/// snapshot is published: it re-adds members libsrt dropped as broken and
/// raises the member down / restored events (see
/// [`crate::srt::bond_supervisor`]). The task hands it back once `cancel`
/// fires, for the disconnect event's member report
/// ([`crate::srt::bond_supervisor::final_member_report`]).
pub(crate) fn spawn_srt_group_stats_poller(
    group: Arc<SrtGroup>,
    mode: SrtBondingMode,
    cache: Arc<watch::Sender<Option<SrtBondingStats>>>,
    input: Option<Arc<SrtInputStats>>,
    cancel: CancellationToken,
    supervisor: Option<BondSupervisor>,
) -> tokio::task::JoinHandle<Option<BondSupervisor>> {
    tokio::spawn(async move {
        let mut supervisor = supervisor;
        let mut interval = poll_interval();
        let mut cursor = LegCursor::default();
        let mut aggregate_rates = IntervalSampler::default();
        let mut member_rates = HashMap::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let agg_raw = group.stats().await;
                    let members = group.member_stats().await;
                    // One instant for the tick, taken straight after the
                    // reads it describes. The supervisor's re-adds are
                    // srt-io round trips (a UDP bind each) and run after
                    // the snapshot is published, so they cannot stretch
                    // the interval the rates are divided by.
                    let now = Instant::now();
                    let view = observe_members(&members);
                    let snapshot = bonding_snapshot(
                        &mode,
                        &agg_raw,
                        group.status(),
                        members,
                        &mut aggregate_rates,
                        &mut member_rates,
                        supervisor.as_ref().map(|s| s.plans()),
                        now,
                    );
                    publish_bonding_stats(&cache, input.as_deref(), &mut cursor, snapshot);
                    if let Some(supervisor) = supervisor.as_mut() {
                        supervisor.tick(&group, &view, now).await;
                    }
                }
            }
        }
        // The group is gone; its members' last "running" states and RTTs
        // would stand for the whole reconnect.
        cache.send_replace(None);
        supervisor
    })
}

/// Listener-side bonding stats poller.
///
/// An `SrtSocket` returned from a listener with `group_connect(true)` may
/// be a group socket internally; `SrtSocket::member_stats()` returns the
/// per-member data in that case and an empty vec for single-socket
/// sessions (harmless for unbonded peers). `input` as for
/// [`spawn_srt_group_stats_poller`]. `tracker` reports the session running
/// below the most members it has had; only the remote caller can re-add.
pub(crate) fn spawn_srt_socket_group_stats_poller(
    socket: Arc<SrtSocket>,
    mode: SrtBondingMode,
    cache: Arc<watch::Sender<Option<SrtBondingStats>>>,
    input: Option<Arc<SrtInputStats>>,
    cancel: CancellationToken,
    tracker: Option<BondHealthTracker>,
) {
    tokio::spawn(async move {
        let mut tracker = tracker;
        let mut interval = poll_interval();
        let mut cursor = LegCursor::default();
        let mut aggregate_rates = IntervalSampler::default();
        let mut member_rates = HashMap::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let agg_raw = socket.stats().await;
                    let members = socket.member_stats().await;
                    let now = Instant::now();
                    if let Some(tracker) = tracker.as_mut() {
                        tracker.tick(&members, now);
                    }
                    let snapshot = bonding_snapshot(
                        &mode,
                        &agg_raw,
                        socket.status(),
                        members,
                        &mut aggregate_rates,
                        &mut member_rates,
                        None,
                        now,
                    );
                    publish_bonding_stats(&cache, input.as_deref(), &mut cursor, snapshot);
                }
            }
        }
        // As for the caller-side group: no stale "connected" while waiting
        // for the next caller.
        cache.send_replace(None);
    });
}

/// Bind a listener for a bonded SRT **input** — same parameters as
/// [`bind_srt_listener_for_input`] but with `SRTO_GROUPCONNECT=true` so
/// the listener accepts native libsrt group handshakes from bonded
/// callers.
pub async fn bind_srt_listener_for_bonded_input(
    config: &SrtInputConfig,
) -> Result<SrtListener> {
    let p = SrtConnectionParams::from(config);
    build_listener_builder(&p)
        .group_connect(true)
        .bind(parse_local_bind(&p)?)
        .await
        .map_err(|e| anyhow::anyhow!("bonded SRT input listener bind: {e}"))
}

/// Bind a listener for a bonded SRT **output**.
pub async fn bind_srt_listener_for_bonded_output(
    config: &SrtOutputConfig,
) -> Result<SrtListener> {
    let p = SrtConnectionParams::from(config);
    build_listener_builder(&p)
        .group_connect(true)
        .bind(parse_local_bind(&p)?)
        .await
        .map_err(|e| anyhow::anyhow!("bonded SRT output listener bind: {e}"))
}

fn parse_local_bind(p: &SrtConnectionParams) -> Result<SocketAddr> {
    let addr = p
        .local_addr
        .ok_or_else(|| anyhow::anyhow!("SRT listener requires local_addr"))?;
    addr.parse()
        .with_context(|| format!("parse SRT listener local_addr '{addr}'"))
}

#[cfg(test)]
mod tests {
    /// The stats caches are created with their receiver already dropped
    /// (`watch::channel(None).0`). `send` stores nothing on such a channel,
    /// which is why `srt_stats` never appeared; the pollers' publish helpers
    /// must store regardless, and count the drops on the way.
    #[test]
    fn publishing_stores_into_a_cache_with_no_receiver() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let cache: tokio::sync::watch::Sender<Option<SrtLegStats>> =
            tokio::sync::watch::channel(None).0;
        let input = flow.register_srt_input_stats("leg");
        let mut cursor = super::LegCursor::default();
        let leg = SrtLegStats { pkt_recv_drop_total: 7, rtt_ms: 21.0, ..Default::default() };
        super::publish_leg_stats(&cache, Some((&input, SrtInputSocket::Only)), &mut cursor, leg);
        assert_eq!(cache.borrow().as_ref().map(|s| s.rtt_ms), Some(21.0));
        assert_eq!(input.recv_dropped(), 7);

        let group: tokio::sync::watch::Sender<Option<SrtBondingStats>> =
            tokio::sync::watch::channel(None).0;
        let input = flow.register_srt_input_stats("bond");
        let mut cursor = super::LegCursor::default();
        // Members' own drop counters are not loss on a broadcast group: only
        // the aggregate's count is taken.
        let member = SrtBondingMemberStats {
            stats: SrtLegStats { pkt_recv_drop_total: 500, ..Default::default() },
            ..Default::default()
        };
        let snapshot = SrtBondingStats {
            aggregate: SrtLegStats { pkt_recv_drop_total: 3, ..Default::default() },
            members: vec![member],
            ..Default::default()
        };
        super::publish_bonding_stats(&group, Some(&input), &mut cursor, snapshot);
        assert!(group.borrow().is_some());
        assert_eq!(input.recv_dropped(), 3);
    }

    /// Rates and interval FEC counters are what moved since the last poll,
    /// not libsrt's since-connect average.
    #[test]
    fn interval_sampler_measures_each_poll_not_the_connection() {
        let t0 = Instant::now();
        let mut sampler = super::IntervalSampler::default();
        let read = |mbps: f64, extra: i32| SrtLegStats {
            recv_rate_mbps: mbps,
            pkt_send_filter_extra: extra,
            ..Default::default()
        };

        // First sample: nothing to measure against, libsrt's figure stands.
        let mut leg = read(8.0, 40);
        sampler.apply(&mut leg, 0, 36_000_000_000, t0);
        assert_eq!(leg.recv_rate_mbps, 8.0);
        assert_eq!(leg.pkt_send_filter_extra, 40);

        // Ten hours in, libsrt still says ~8 Mbps; the last second carried 1.
        let mut leg = read(7.9, 45);
        sampler.apply(&mut leg, 0, 36_000_125_000, t0 + Duration::from_secs(1));
        assert!((leg.recv_rate_mbps - 1.0).abs() < 1e-9, "{}", leg.recv_rate_mbps);
        assert_eq!(leg.send_rate_mbps, 0.0);
        assert_eq!(leg.pkt_send_filter_extra, 5);

        // A failed read comes back zeroed: left as read, baseline kept …
        let mut leg = read(0.0, 0);
        sampler.apply(&mut leg, 0, 0, t0 + Duration::from_secs(2));
        assert_eq!(leg.recv_rate_mbps, 0.0);
        // … so the next good read measures from the last good one, not from 0.
        let mut leg = read(7.8, 45);
        sampler.apply(&mut leg, 0, 36_000_375_000, t0 + Duration::from_secs(3));
        assert!((leg.recv_rate_mbps - 1.0).abs() < 1e-9, "{}", leg.recv_rate_mbps);
        assert_eq!(leg.pkt_send_filter_extra, 0);
    }

    /// A group poller's cursor: the aggregate's drops add up across polls,
    /// and a failed read of the group is no sample, so the next good read
    /// adds what moved since the last good one — not the group's whole drop
    /// history again.
    #[test]
    fn a_groups_drops_survive_a_failed_read() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let input = flow.register_srt_input_stats("bond");
        let cache: tokio::sync::watch::Sender<Option<SrtBondingStats>> =
            tokio::sync::watch::channel(None).0;
        let mut cursor = super::LegCursor::default();
        for (drops, delivered) in [(92, 1_000), (93, 2_000), (0, 0), (95, 3_000)] {
            let snapshot = SrtBondingStats {
                aggregate: SrtLegStats {
                    pkt_recv_drop_total: drops,
                    pkt_recv_unique_total: delivered,
                    ..Default::default()
                },
                ..Default::default()
            };
            super::publish_bonding_stats(&cache, Some(&input), &mut cursor, snapshot);
        }
        assert_eq!(input.recv_dropped(), 95);
        // A new group starts a fresh cursor, so its first reading counts in full.
        let mut fresh = super::LegCursor::default();
        let snapshot = SrtBondingStats {
            aggregate: SrtLegStats { pkt_recv_drop_total: 49, ..Default::default() },
            ..Default::default()
        };
        super::publish_bonding_stats(&cache, Some(&input), &mut fresh, snapshot);
        assert_eq!(input.recv_dropped(), 144);
    }

    /// A caller group shows each member's **configured** weight (lower is
    /// preferred), not the rank libsrt was handed (greater is preferred);
    /// a listener's members, which have no plan, show what libsrt holds.
    #[test]
    fn caller_group_member_reports_its_configured_weight() {
        use crate::config::models::{SrtBondingConfig, SrtBondingEndpoint, SrtBondingMode};
        let endpoint = |addr: &str, weight| SrtBondingEndpoint {
            addr: addr.into(),
            local_addr: None,
            weight: Some(weight),
            interface_binding: None,
        };
        let plans = crate::srt::bond_supervisor::plan_bond_members(&SrtBondingConfig {
            mode: SrtBondingMode::Backup,
            endpoints: vec![endpoint("10.0.0.1:9000", 0), endpoint("10.0.0.2:9000", 4)],
        })
        .unwrap();
        assert_eq!(plans[0].srt_weight, 1);
        let member = |token: i32, weight: u16| srt_transport::GroupMemberStats {
            id: 100 + token,
            peer_addr: None,
            socket_status: srt_protocol::config::SocketStatus::Connected,
            member_status: MemberStatus::Idle,
            weight,
            token,
            stats: Default::default(),
        };
        let snapshot = |plans: Option<&[crate::srt::bond_supervisor::BondMemberPlan]>| {
            super::bonding_snapshot(
                &SrtBondingMode::Backup,
                &Default::default(),
                srt_protocol::config::SocketStatus::Connected,
                vec![member(0, 1), member(1, 0)],
                &mut super::IntervalSampler::default(),
                &mut HashMap::new(),
                plans,
                Instant::now(),
            )
        };
        let caller = snapshot(Some(&plans));
        assert_eq!(caller.members.iter().map(|m| m.weight).collect::<Vec<_>>(), [0, 4]);
        let listener = snapshot(None);
        assert_eq!(listener.members.iter().map(|m| m.weight).collect::<Vec<_>>(), [1, 0]);
    }

    /// A leg poller's failed read is no sample for either counter: neither
    /// its drops nor its delivered packets are counted again by the next
    /// good read, and the failed one reaches the pair's ledger as a poll that
    /// found and delivered nothing.
    #[test]
    fn a_failed_read_is_no_sample_for_a_legs_counters() {
        let read = |drops: i32, delivered: i64, uptime_ms: i64| SrtLegStats {
            pkt_recv_drop_total: drops,
            pkt_recv_unique_total: delivered,
            pkt_recv_total: delivered,
            uptime_ms,
            ..Default::default()
        };
        let mut cursor = super::LegCursor::default();
        assert_eq!(cursor.advance(&read(40, 1_000, 5_000)), Some((40, 1_000)));
        assert_eq!(cursor.advance(&read(0, 0, 0)), None, "zeroed");
        assert_eq!(cursor.advance(&read(42, 0, 6_000)), None, "any counter back is no sample");
        assert_eq!(cursor.advance(&read(42, 1_500, 7_000)), Some((2, 500)));
        // A sender's socket moves none of the receive counters: its clock and
        // its sent packets tell its failed reads.
        let mut cursor = super::LegCursor::default();
        let sent = SrtLegStats { pkt_sent_total: 900, uptime_ms: 9_000, ..Default::default() };
        assert_eq!(cursor.advance(&sent), Some((0, 0)));
        assert_eq!(cursor.advance(&SrtLegStats::default()), None);
    }

    /// A failed read is not published: the cache keeps the last good
    /// reading, on an input's socket, an output's and a group. Published, its
    /// zeros made `bilbycast_edge_srt_loss_total` — a Prometheus counter —
    /// read 0, a reset that `increase()` counted twice, and the drop
    /// reason's latency and RTT read 0 ms.
    #[test]
    fn a_failed_read_keeps_the_last_good_stats() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let good = SrtLegStats {
            pkt_loss_total: 50,
            pkt_sent_total: 4_000,
            pkt_recv_total: 4_000,
            pkt_recv_unique_total: 3_950,
            ms_recv_tsbpd_delay: 120,
            rtt_ms: 21.0,
            uptime_ms: 60_000,
            ..Default::default()
        };
        let input = flow.register_srt_input_stats("main");
        for socket in [Some((&*input, SrtInputSocket::Only)), None] {
            let cache: tokio::sync::watch::Sender<Option<SrtLegStats>> =
                tokio::sync::watch::channel(None).0;
            let mut cursor = super::LegCursor::default();
            super::publish_leg_stats(&cache, socket, &mut cursor, good.clone());
            super::publish_leg_stats(&cache, socket, &mut cursor, SrtLegStats::default());
            let kept = cache.borrow().clone().expect("published");
            assert_eq!(kept.pkt_loss_total, 50);
            assert_eq!((kept.ms_recv_tsbpd_delay, kept.rtt_ms), (120, 21.0));
        }

        let cache: tokio::sync::watch::Sender<Option<SrtBondingStats>> =
            tokio::sync::watch::channel(None).0;
        let mut cursor = super::LegCursor::default();
        let snapshot = |aggregate: SrtLegStats| SrtBondingStats { aggregate, ..Default::default() };
        super::publish_bonding_stats(&cache, Some(&input), &mut cursor, snapshot(good.clone()));
        super::publish_bonding_stats(&cache, Some(&input), &mut cursor, snapshot(Default::default()));
        assert_eq!(cache.borrow().as_ref().map(|s| s.aggregate.pkt_loss_total), Some(50));
    }

    use super::*;

    /// The listener filter is compared per the SRT Access Control spec, not
    /// byte-for-byte. Until the 2026-08 libsrt-rs fix the filter was never
    /// installed at all (`srt_listen_callback` after `srt_listen`, which libsrt
    /// refuses silently), so a verbatim comparison had no observable
    /// consequence. It does now: an ingest whose caller emits
    /// `#!::r=name,m=publish` — the shape OBS Studio and ffmpeg produce once a
    /// stream key is set, and the format both `docs/CONFIGURATION.md` and
    /// `docs/supported-protocols.md` promise is supported — must keep working
    /// against a plainly configured `name`.
    #[test]
    fn a_structured_stream_id_matches_the_plain_resource_it_names() {
        assert!(stream_id_matches("mystream", "#!::r=mystream,m=publish"));
        assert!(stream_id_matches("mystream", "#!::m=publish,r=mystream"));
        assert!(stream_id_matches("mystream", "#!::r=mystream"));
        // ... and the mirror case: configured structured, caller plain.
        assert!(stream_id_matches("#!::r=mystream,m=publish", "mystream"));
        // ... and structured on both sides, differing only in the extras.
        assert!(stream_id_matches(
            "#!::r=mystream,m=publish",
            "#!::r=mystream,m=request,u=bob"
        ));
    }

    /// Widening must not become "accepts anything". These are the callers the
    /// filter exists to keep out, and every one of them is still refused.
    #[test]
    fn a_stream_id_naming_a_different_resource_is_still_rejected() {
        assert!(!stream_id_matches("mystream", "otherstream"));
        assert!(!stream_id_matches("mystream", "#!::r=otherstream,m=publish"));
        assert!(!stream_id_matches("mystream", "#!::m=publish,u=bob"));
        assert!(!stream_id_matches("mystream", ""));
        assert!(!stream_id_matches("mystream", "#!::"));
        // A near-miss must not pass: no prefix or substring matching.
        assert!(!stream_id_matches("mystream", "mystream2"));
        assert!(!stream_id_matches("mystream", "#!::r=mystream2"));
    }

    /// Exact equality is still a match, whatever the shape — the widening is
    /// strictly additive, so no caller the old comparison admitted is lost.
    #[test]
    fn an_exactly_equal_stream_id_still_matches() {
        assert!(stream_id_matches("mystream", "mystream"));
        assert!(stream_id_matches(
            "#!::r=mystream,m=publish",
            "#!::r=mystream,m=publish"
        ));
        // No configured filter is never installed (see
        // `apply_stream_id_access_control`), but an empty expected value must
        // not silently match every caller if one ever reaches here.
        assert!(!stream_id_matches("", "anything"));
    }
}
