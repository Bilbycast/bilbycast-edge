// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Short-lived connection test tasks for validating input/output configs
//! without requiring a running flow.
//!
//! Each test function spawns a temporary socket/connection, waits briefly for
//! activity, then tears down. Results are returned as [`TestResult`] structs
//! serialized to JSON and sent back to the manager via `command_ack.data`.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;

use crate::config::models::{InputConfig, OutputConfig, SrtMode};

/// Maximum time a test is allowed to run before auto-cancellation.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of a connection test, serialized and returned in the command_ack payload.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    /// Whether the test was considered successful (bound, connected, or receiving).
    pub success: bool,
    /// Human-readable connection state, e.g. "bound", "connected", "timeout", "error".
    pub state: String,
    /// Descriptive message for the UI.
    pub message: String,
    /// SRT round-trip time in milliseconds (SRT caller connections only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Number of active connections (SRT listener / RTMP server only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    /// Packets received during the test (RTP/UDP only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packets_received: Option<u64>,
    /// How long the test ran in milliseconds.
    pub duration_ms: u64,
    /// Local address that was bound (when applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
}

impl TestResult {
    fn ok(state: &str, message: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: true,
            state: state.to_string(),
            message: message.into(),
            rtt_ms: None,
            connections: None,
            packets_received: None,
            duration_ms: duration.as_millis() as u64,
            local_addr: None,
        }
    }

    fn err(message: impl Into<String>, duration: Duration) -> Self {
        Self {
            success: false,
            state: "error".to_string(),
            message: message.into(),
            rtt_ms: None,
            connections: None,
            packets_received: None,
            duration_ms: duration.as_millis() as u64,
            local_addr: None,
        }
    }
}

/// Test an input config by temporarily binding/connecting.
pub async fn test_input(config: &InputConfig) -> TestResult {
    let cancel = CancellationToken::new();
    let start = Instant::now();

    let result = tokio::time::timeout(TEST_TIMEOUT, async {
        match config {
            InputConfig::Srt(srt) => test_srt_input(srt, &cancel).await,
            InputConfig::Rist(rist) => test_udp_bind(&rist.bind_addr, "RIST").await,
            InputConfig::Rtp(rtp) => test_udp_bind(&rtp.bind_addr, "RTP").await,
            InputConfig::Udp(udp) => test_udp_bind(&udp.bind_addr, "UDP").await,
            InputConfig::Rtmp(rtmp) => test_tcp_listen(&rtmp.listen_addr, "RTMP").await,
            InputConfig::Rtsp(rtsp) => test_rtsp_connect(&rtsp.rtsp_url).await,
            InputConfig::Webrtc(_) => {
                TestResult::ok("ready", "WebRTC WHIP server — test requires active flow", start.elapsed())
            }
            InputConfig::Whep(whep) => {
                TestResult::ok("configured", format!("WHEP client configured for {}", whep.whep_url), start.elapsed())
            }
            InputConfig::St2110_30(st) | InputConfig::St2110_31(st) => {
                test_udp_bind(&st.bind_addr, "ST 2110 audio").await
            }
            InputConfig::St2110_40(anc) => {
                test_udp_bind(&anc.bind_addr, "ST 2110-40 ANC").await
            }
            InputConfig::RtpAudio(rtp_audio) => {
                test_udp_bind(&rtp_audio.bind_addr, "RTP Audio").await
            }
            InputConfig::St2110_20(v) => {
                test_udp_bind(&v.bind_addr, "ST 2110-20 video").await
            }
            InputConfig::St2110_23(v) => {
                if let Some(first) = v.sub_streams.first() {
                    test_udp_bind(&first.bind_addr, "ST 2110-23 video (sub_stream[0])").await
                } else {
                    TestResult::err("ST 2110-23 input missing sub_streams", start.elapsed())
                }
            }
            InputConfig::Bonded(c) => {
                // Paths carry their own bind semantics — a shallow
                // test just confirms at least one path is
                // configured. Deep testing requires spinning the
                // full bond socket; do that by starting the flow.
                if c.paths.is_empty() {
                    TestResult::err("bonded input has zero paths", start.elapsed())
                } else {
                    TestResult::ok(
                        "configured",
                        format!("bonded input with {} path(s)", c.paths.len()),
                        start.elapsed(),
                    )
                }
            }
            InputConfig::TestPattern(c) => TestResult::ok(
                "configured",
                format!("test-pattern {}x{}@{}", c.width, c.height, c.fps),
                start.elapsed(),
            ),
            InputConfig::MediaPlayer(c) => {
                if c.sources.is_empty() {
                    TestResult::err("media-player input has zero sources", start.elapsed())
                } else {
                    TestResult::ok(
                        "configured",
                        format!("media-player ({} source(s))", c.sources.len()),
                        start.elapsed(),
                    )
                }
            }
        }
    })
    .await;

    cancel.cancel();

    match result {
        Ok(r) => r,
        Err(_) => TestResult::err("Test timed out after 10 seconds", start.elapsed()),
    }
}

/// Test an output config by temporarily connecting/binding.
pub async fn test_output(config: &OutputConfig) -> TestResult {
    let cancel = CancellationToken::new();
    let start = Instant::now();

    let result = tokio::time::timeout(TEST_TIMEOUT, async {
        match config {
            OutputConfig::Srt(srt) => test_srt_output(srt, &cancel).await,
            OutputConfig::Rist(rist) => test_udp_send_socket(&rist.remote_addr, "RIST").await,
            OutputConfig::Rtp(rtp) => test_udp_send_socket(&rtp.dest_addr, "RTP").await,
            OutputConfig::Udp(udp) => test_udp_send_socket(&udp.dest_addr, "UDP").await,
            OutputConfig::Rtmp(rtmp) => test_tcp_connect_url(&rtmp.dest_url, "RTMP").await,
            OutputConfig::Hls(hls) => {
                TestResult::ok("configured", format!("HLS output configured for {}", hls.ingest_url), start.elapsed())
            }
            OutputConfig::Cmaf(cmaf) => {
                TestResult::ok("configured", format!("CMAF output configured for {}", cmaf.ingest_url), start.elapsed())
            }
            OutputConfig::Webrtc(_) => {
                TestResult::ok("ready", "WebRTC output — test requires active flow", start.elapsed())
            }
            OutputConfig::St2110_30(st) | OutputConfig::St2110_31(st) => {
                test_udp_send_socket(&st.dest_addr, "ST 2110 audio").await
            }
            OutputConfig::St2110_40(anc) => {
                test_udp_send_socket(&anc.dest_addr, "ST 2110-40 ANC").await
            }
            OutputConfig::RtpAudio(rtp_audio) => {
                test_udp_send_socket(&rtp_audio.dest_addr, "RTP Audio").await
            }
            OutputConfig::St2110_20(v) => {
                test_udp_send_socket(&v.dest_addr, "ST 2110-20 video").await
            }
            OutputConfig::St2110_23(v) => {
                if let Some(first) = v.sub_streams.first() {
                    test_udp_send_socket(&first.dest_addr, "ST 2110-23 video (sub_stream[0])").await
                } else {
                    TestResult::err("ST 2110-23 output missing sub_streams", start.elapsed())
                }
            }
            OutputConfig::Bonded(c) => {
                if c.paths.is_empty() {
                    TestResult::err("bonded output has zero paths", start.elapsed())
                } else {
                    TestResult::ok(
                        "configured",
                        format!("bonded output with {} path(s)", c.paths.len()),
                        start.elapsed(),
                    )
                }
            }
        }
    })
    .await;

    cancel.cancel();

    match result {
        Ok(r) => r,
        Err(_) => TestResult::err("Test timed out after 10 seconds", start.elapsed()),
    }
}

// ── Per-protocol test implementations ──

async fn test_srt_input(
    config: &crate::config::models::SrtInputConfig,
    cancel: &CancellationToken,
) -> TestResult {
    let start = Instant::now();
    let addr_label = match config.mode {
        SrtMode::Listener => config.local_addr.as_deref().unwrap_or("0.0.0.0:0"),
        SrtMode::Caller | SrtMode::Rendezvous => config.remote_addr.as_deref().unwrap_or("(no address)"),
    };
    let mode_str = match config.mode {
        SrtMode::Listener => "listener",
        SrtMode::Caller => "caller",
        SrtMode::Rendezvous => "rendezvous",
    };

    match crate::srt::connection::connect_srt_input(config, cancel).await {
        Ok(socket) => {
            let stats = socket.stats().await;
            let leg = crate::srt::connection::convert_srt_stats(&stats);
            let mut result = if config.mode == SrtMode::Listener && stats.pkt_recv_total == 0 {
                TestResult::ok(
                    "listening",
                    format!("SRT {mode_str} bound on {addr_label}, waiting for caller"),
                    start.elapsed(),
                )
            } else {
                TestResult::ok(
                    "connected",
                    format!("SRT {mode_str} connected to {addr_label}, RTT {:.1}ms", leg.rtt_ms),
                    start.elapsed(),
                )
            };
            result.rtt_ms = Some(leg.rtt_ms);
            result.local_addr = config.local_addr.clone();
            result
        }
        Err(e) => TestResult::err(
            format!("SRT {mode_str} failed ({addr_label}): {e}"),
            start.elapsed(),
        ),
    }
}

async fn test_srt_output(
    config: &crate::config::models::SrtOutputConfig,
    cancel: &CancellationToken,
) -> TestResult {
    let start = Instant::now();
    let addr_label = match config.mode {
        SrtMode::Listener => config.local_addr.as_deref().unwrap_or("0.0.0.0:0"),
        SrtMode::Caller | SrtMode::Rendezvous => config.remote_addr.as_deref().unwrap_or("(no address)"),
    };
    let mode_str = match config.mode {
        SrtMode::Listener => "listener",
        SrtMode::Caller => "caller",
        SrtMode::Rendezvous => "rendezvous",
    };

    match crate::srt::connection::connect_srt_output(config, cancel).await {
        Ok(socket) => {
            let stats = socket.stats().await;
            let leg = crate::srt::connection::convert_srt_stats(&stats);
            let mut result = if config.mode == SrtMode::Listener {
                TestResult::ok(
                    "listening",
                    format!("SRT output {mode_str} bound on {addr_label}, waiting for caller"),
                    start.elapsed(),
                )
            } else {
                TestResult::ok(
                    "connected",
                    format!("SRT output {mode_str} connected to {addr_label}, RTT {:.1}ms", leg.rtt_ms),
                    start.elapsed(),
                )
            };
            result.rtt_ms = Some(leg.rtt_ms);
            result.local_addr = config.local_addr.clone();
            result
        }
        Err(e) => TestResult::err(
            format!("SRT output {mode_str} failed ({addr_label}): {e}"),
            start.elapsed(),
        ),
    }
}

/// Test binding a UDP socket (for RTP, UDP, ST 2110, RTP Audio inputs).
async fn test_udp_bind(bind_addr: &str, protocol: &str) -> TestResult {
    let start = Instant::now();
    let addr: SocketAddr = match bind_addr.parse() {
        Ok(a) => a,
        Err(e) => return TestResult::err(format!("Invalid bind address '{bind_addr}': {e}"), start.elapsed()),
    };

    match UdpSocket::bind(addr).await {
        Ok(sock) => {
            let local = sock.local_addr().map(|a| a.to_string()).unwrap_or_default();
            // Wait briefly (2s) to see if any packets arrive
            let mut buf = [0u8; 2048];
            let packets = match tokio::time::timeout(Duration::from_secs(2), sock.recv(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => 1u64,
                _ => 0,
            };

            let mut result = if packets > 0 {
                TestResult::ok(
                    "receiving",
                    format!("{protocol} socket bound on {local}, receiving data"),
                    start.elapsed(),
                )
            } else {
                TestResult::ok(
                    "bound",
                    format!("{protocol} socket bound on {local}, no data received yet"),
                    start.elapsed(),
                )
            };
            result.local_addr = Some(local);
            result.packets_received = Some(packets);
            result
        }
        Err(e) => TestResult::err(
            format!("{protocol} bind to {bind_addr} failed: {e}"),
            start.elapsed(),
        ),
    }
}

/// Test binding a TCP listener (for RTMP server input).
async fn test_tcp_listen(listen_addr: &str, protocol: &str) -> TestResult {
    let start = Instant::now();
    let addr: SocketAddr = match listen_addr.parse() {
        Ok(a) => a,
        Err(e) => return TestResult::err(format!("Invalid listen address '{listen_addr}': {e}"), start.elapsed()),
    };

    match TcpListener::bind(addr).await {
        Ok(listener) => {
            let local = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
            // Wait briefly (3s) to see if any connection arrives
            let connected = match tokio::time::timeout(Duration::from_secs(3), listener.accept()).await {
                Ok(Ok((_, peer))) => Some(peer),
                _ => None,
            };

            let mut result = if let Some(peer) = connected {
                let mut r = TestResult::ok(
                    "connected",
                    format!("{protocol} server on {local}: client connected from {peer}"),
                    start.elapsed(),
                );
                r.connections = Some(1);
                r
            } else {
                TestResult::ok(
                    "listening",
                    format!("{protocol} server listening on {local}, no client connected yet"),
                    start.elapsed(),
                )
            };
            result.local_addr = Some(local);
            result
        }
        Err(e) => TestResult::err(
            format!("{protocol} listen on {listen_addr} failed: {e}"),
            start.elapsed(),
        ),
    }
}

/// Test RTSP connection by attempting to connect and then dropping.
async fn test_rtsp_connect(rtsp_url: &str) -> TestResult {
    let start = Instant::now();
    // Parse the host:port from the RTSP URL to check TCP reachability
    let url = match url::Url::parse(rtsp_url) {
        Ok(u) => u,
        Err(e) => return TestResult::err(format!("Invalid RTSP URL: {e}"), start.elapsed()),
    };
    let host = url.host_str().unwrap_or("localhost");
    let port = url.port().unwrap_or(554);
    let tcp_addr = format!("{host}:{port}");

    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&tcp_addr),
    )
    .await
    {
        Ok(Ok(_stream)) => TestResult::ok(
            "reachable",
            format!("RTSP server at {tcp_addr} is reachable (TCP connected)"),
            start.elapsed(),
        ),
        Ok(Err(e)) => TestResult::err(
            format!("RTSP server at {tcp_addr} unreachable: {e}"),
            start.elapsed(),
        ),
        Err(_) => TestResult::err(
            format!("RTSP connection to {tcp_addr} timed out"),
            start.elapsed(),
        ),
    }
}

/// Test creating a UDP send socket (for RTP, UDP, ST 2110 outputs).
async fn test_udp_send_socket(dest_addr: &str, protocol: &str) -> TestResult {
    let start = Instant::now();
    let addr: SocketAddr = match dest_addr.parse() {
        Ok(a) => a,
        Err(e) => return TestResult::err(
            format!("Invalid destination address '{dest_addr}': {e}"),
            start.elapsed(),
        ),
    };

    // Bind an ephemeral socket and verify we can "connect" to the destination
    let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    match UdpSocket::bind(bind).await {
        Ok(sock) => {
            let local = sock.local_addr().map(|a| a.to_string()).unwrap_or_default();
            // connect() validates routing (but doesn't send anything)
            match sock.connect(addr).await {
                Ok(()) => {
                    let mut result = TestResult::ok(
                        "ready",
                        format!("{protocol} output socket ready, destination {dest_addr} is routable"),
                        start.elapsed(),
                    );
                    result.local_addr = Some(local);
                    result
                }
                Err(e) => TestResult::err(
                    format!("{protocol} destination {dest_addr} not routable: {e}"),
                    start.elapsed(),
                ),
            }
        }
        Err(e) => TestResult::err(
            format!("{protocol} output socket bind failed: {e}"),
            start.elapsed(),
        ),
    }
}

/// Test TCP connect for RTMP/HLS output destinations.
async fn test_tcp_connect_url(dest_url: &str, protocol: &str) -> TestResult {
    let start = Instant::now();
    let url = match url::Url::parse(dest_url) {
        Ok(u) => u,
        Err(e) => return TestResult::err(format!("Invalid {protocol} URL: {e}"), start.elapsed()),
    };
    let host = url.host_str().unwrap_or("localhost");
    let port = url.port().unwrap_or(1935);
    let tcp_addr = format!("{host}:{port}");

    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&tcp_addr),
    )
    .await
    {
        Ok(Ok(_stream)) => TestResult::ok(
            "reachable",
            format!("{protocol} server at {tcp_addr} is reachable (TCP connected)"),
            start.elapsed(),
        ),
        Ok(Err(e)) => TestResult::err(
            format!("{protocol} server at {tcp_addr} unreachable: {e}"),
            start.elapsed(),
        ),
        Err(_) => TestResult::err(
            format!("{protocol} connection to {tcp_addr} timed out"),
            start.elapsed(),
        ),
    }
}
