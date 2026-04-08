// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use serde::{Deserialize, Serialize};

/// Root configuration, persisted to config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Schema version for forward compatibility
    pub version: u32,
    /// Persistent node UUID (auto-generated on first run, used for NMOS IS-04)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// API server configuration
    pub server: ServerConfig,
    /// Optional web monitoring dashboard
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorConfig>,
    /// Optional human-readable device name/label (set during initial setup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Whether the /setup wizard page is enabled. Default: true.
    /// Set to false to disable the setup wizard after provisioning.
    #[serde(default = "default_true")]
    pub setup_enabled: bool,
    /// Optional manager connection for centralized monitoring
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<crate::manager::ManagerConfig>,
    /// List of all configured flows
    #[serde(default)]
    pub flows: Vec<FlowConfig>,
    /// Optional IP tunnels (relay or direct QUIC tunnels between edge nodes)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<crate::tunnel::TunnelConfig>,
    /// Optional SMPTE ST 2110 flow groups (essence bundles).
    ///
    /// A flow group bundles multiple essence flows (audio, ancillary, future video)
    /// that share PTP timing and NMOS activation. Each member references a top-level
    /// `FlowConfig` by ID. Existing single-flow configs do not need to use flow groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flow_groups: Vec<FlowGroupConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            flows: Vec::new(),
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// API listen address, e.g. "0.0.0.0"
    pub listen_addr: String,
    /// API listen port, default 8080
    pub listen_port: u16,
    /// Optional TLS configuration for HTTPS on the API server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    /// Optional OAuth 2.0 authentication configuration.
    /// When absent or `enabled: false`, all endpoints are unauthenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::api::auth::AuthConfig>,
}

/// TLS configuration for HTTPS serving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Path to PEM-encoded TLS certificate file.
    pub cert_path: String,
    /// Path to PEM-encoded TLS private key file.
    pub key_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".to_string(),
            listen_port: 8080,
            tls: None,
            auth: None,
        }
    }
}

/// Optional web monitoring dashboard configuration.
///
/// When present, bilbycast-edge starts a second HTTP server on the specified address
/// serving a self-contained HTML dashboard for browser-based status monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Dashboard listen address, e.g. "0.0.0.0"
    pub listen_addr: String,
    /// Dashboard listen port, e.g. 9090
    pub listen_port: u16,
}

/// A Flow is the unit of configuration: one input fanning out to N outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowConfig {
    /// Unique identifier for this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Whether this flow should be active on startup
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Enable media content analysis (codec, resolution, frame rate detection).
    /// Default: true. Set to false to save CPU on resource-constrained devices.
    #[serde(default = "default_true")]
    pub media_analysis: bool,
    /// Enable thumbnail generation for visual flow preview (requires ffmpeg on the device).
    /// Default: true. Thumbnails are only produced when ffmpeg is detected at startup.
    #[serde(default = "default_true")]
    pub thumbnail: bool,
    /// MPEG-TS program_number whose video to render in the thumbnail when
    /// the input is an MPTS. `None` = let ffmpeg pick (typically the first
    /// program). Must be > 0 if set; program_number 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_program_number: Option<u16>,
    /// Optional bandwidth limit for trust boundary enforcement (RP 2129).
    /// When configured, the node monitors the flow's input bitrate and takes
    /// the specified action if it exceeds the limit for the grace period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_limit: Option<BandwidthLimitConfig>,
    /// Optional SMPTE ST 2110 flow group membership. When set, this flow is an
    /// essence flow within the named group and shares its clock domain. Default: None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_group_id: Option<String>,
    /// Optional PTP clock domain (IEEE 1588) for this flow. 0..=127. Used by
    /// SMPTE ST 2110 inputs/outputs and the PTP state reporter to scope sync
    /// monitoring. Inherits from `flow_group` if set there. Default: None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// The single input source for this flow
    pub input: InputConfig,
    /// One or more output destinations (fan-out)
    pub outputs: Vec<OutputConfig>,
}

/// Bandwidth limit configuration for per-flow trust boundary enforcement.
///
/// Monitors the flow's input bitrate and triggers an action if it exceeds
/// the configured maximum for the duration of the grace period. This avoids
/// false positives from transient spikes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandwidthLimitConfig {
    /// Expected maximum bitrate in megabits per second.
    pub max_bitrate_mbps: f64,
    /// Action to take when the limit is exceeded.
    pub action: BandwidthLimitAction,
    /// Seconds the bitrate must continuously exceed the limit before
    /// triggering the action. Default: 5 seconds.
    #[serde(default = "default_grace_period")]
    pub grace_period_secs: u32,
}

/// Action to take when a flow's bandwidth limit is exceeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandwidthLimitAction {
    /// Raise a warning event and flag the flow on the dashboard.
    /// The flow continues operating normally.
    Alarm,
    /// Gate the flow: drop all incoming packets until the bandwidth
    /// returns to within the configured limit. The flow stays alive
    /// and automatically resumes when bandwidth normalizes.
    Block,
}

fn default_grace_period() -> u32 {
    5
}

fn default_true() -> bool {
    true
}

/// Input source configuration — RTP, UDP, SRT, RTMP, RTSP, or WebRTC
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputConfig {
    /// Receive RTP over UDP (unicast or multicast) with optional FEC and ingress filters
    #[serde(rename = "rtp")]
    Rtp(RtpInputConfig),
    /// Receive raw UDP datagrams (MPEG-TS or other payloads, no RTP header required)
    #[serde(rename = "udp")]
    Udp(UdpInputConfig),
    /// Receive RTP over SRT
    #[serde(rename = "srt")]
    Srt(SrtInputConfig),
    /// Receive H.264/AAC via RTMP (accept publish from OBS, ffmpeg, etc.)
    #[serde(rename = "rtmp")]
    Rtmp(RtmpInputConfig),
    /// Receive H.264/H.265 + AAC via RTSP (pull from IP cameras, media servers)
    #[serde(rename = "rtsp")]
    Rtsp(RtspInputConfig),
    /// Receive H.264/Opus via WebRTC WHIP (accept publish from OBS, browser, etc.)
    #[serde(rename = "webrtc")]
    Webrtc(WebrtcInputConfig),
    /// Receive H.264/Opus via WebRTC WHEP client (pull from external WHEP server)
    #[serde(rename = "whep")]
    Whep(WhepInputConfig),
    /// Receive SMPTE ST 2110-30 uncompressed PCM audio over RTP (L16/L24).
    #[serde(rename = "st2110_30")]
    St2110_30(St2110AudioInputConfig),
    /// Receive SMPTE ST 2110-31 AES3 transparent audio over RTP (preserves Dolby E,
    /// AES3 user/channel-status/validity bits). Wire format identical to -30 except
    /// the payload is AES3 sub-frames rather than linear PCM samples.
    #[serde(rename = "st2110_31")]
    St2110_31(St2110AudioInputConfig),
    /// Receive SMPTE ST 2110-40 ancillary data (RFC 8331) — SCTE-104 ad markers,
    /// SMPTE 12M timecode, CEA-608/708 captions, and other ANC essence.
    #[serde(rename = "st2110_40")]
    St2110_40(St2110AncillaryInputConfig),
    /// Receive RFC 3551 PCM audio over RTP/UDP without ST 2110 constraints.
    /// No PTP requirement, no RFC 7273 timing reference, no clock_domain
    /// advertising in NMOS. Useful for radio contribution feeds, talkback,
    /// and any general PCM-over-RTP source where ST 2110 is overkill.
    #[serde(rename = "rtp_audio")]
    RtpAudio(RtpAudioInputConfig),
}

impl InputConfig {
    /// Returns true when this input produces MPEG-TS bytes (with or without
    /// an RTP wrapper) on the broadcast channel.
    ///
    /// Used by the flow runtime to gate MPEG-TS-only consumers like the
    /// TR-101290 analyzer. Audio-only and ANC inputs (ST 2110-30/-31/-40,
    /// `rtp_audio`) carry uncompressed PCM or RFC 8331 ancillary data and
    /// must not be subjected to TR-101290 sync-byte / CC checks — running
    /// the analyzer on them log-spams "sync lost" warnings forever.
    pub fn is_ts_carrier(&self) -> bool {
        match self {
            InputConfig::Rtp(_)
            | InputConfig::Udp(_)
            | InputConfig::Srt(_)
            | InputConfig::Rtmp(_)
            | InputConfig::Rtsp(_)
            | InputConfig::Webrtc(_)
            | InputConfig::Whep(_) => true,
            InputConfig::St2110_30(_)
            | InputConfig::St2110_31(_)
            | InputConfig::St2110_40(_)
            | InputConfig::RtpAudio(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpInputConfig {
    /// Local address to bind, e.g. "0.0.0.0:5000" or "239.1.1.1:5000" for multicast
    pub bind_addr: String,
    /// Network interface IP for multicast join (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional: decode incoming SMPTE 2022-1 FEC before forwarding
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fec_decode: Option<FecConfig>,
    /// Enable VSF TR-07 mode: validates JPEG XS stream presence in PMT.
    /// When true, the dashboard and API report TR-07 compliance status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tr07_mode: Option<bool>,
    /// Source IP allow-list (RP 2129 C5). Only packets from these IPs are accepted.
    /// When absent, all sources are allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_sources: Option<Vec<String>>,
    /// RTP payload type allow-list (RP 2129 U4). Only packets with these PTs are accepted.
    /// When absent, all payload types are allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_payload_types: Option<Vec<u8>>,
    /// Maximum ingress bitrate in Mbps (RP 2129 C7). Excess packets are dropped.
    /// When absent, no rate limiting is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_mbps: Option<f64>,
    /// Optional: enable SMPTE 2022-7 redundancy (merge from two UDP legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RtpRedundancyConfig>,
}

/// Raw UDP input — receives datagrams without requiring RTP headers.
///
/// Suitable for receiving raw MPEG-TS over UDP (e.g., from OBS, srt-live-transmit,
/// ffmpeg with `udp://` output). All datagrams are accepted and forwarded with
/// synthetic sequence numbers and `is_raw_ts: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpInputConfig {
    /// Local address to bind, e.g. "0.0.0.0:5000" or "239.1.1.1:5000" for multicast
    pub bind_addr: String,
    /// Network interface IP for multicast join (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtInputConfig {
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address, e.g. "0.0.0.0:9000"
    pub local_addr: String,
    /// Remote address (required for caller and rendezvous modes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// SRT latency in milliseconds (sets both receiver and peer/sender latency).
    /// Use recv_latency_ms / peer_latency_ms to override independently.
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
    /// Receiver-side latency override in milliseconds. When set, overrides latency_ms
    /// for the receiver side only (how long the receiver buffers before delivering).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_latency_ms: Option<u64>,
    /// Peer/sender-side latency override in milliseconds. When set, overrides latency_ms
    /// for the sender side only (minimum latency the sender requests from the receiver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_latency_ms: Option<u64>,
    /// Peer idle timeout in seconds. Connection is dropped if no data
    /// is received for this duration. Default: 30s (suitable for broadcast).
    #[serde(default = "default_peer_idle_timeout")]
    pub peer_idle_timeout_secs: u64,
    /// Optional AES encryption passphrase (10-79 chars)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// AES key length: 16, 24, or 32 (default 16)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key_len: Option<usize>,
    /// Encryption cipher mode: "aes-ctr" (default) or "aes-gcm" (authenticated encryption).
    /// AES-GCM requires libsrt >= 1.5.2 on the peer and only supports AES-128/256 keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypto_mode: Option<String>,
    /// Maximum retransmission bandwidth in bytes/sec (Token Bucket shaper).
    /// -1 = unlimited (default), 0 = disable retransmissions, >0 = cap in bytes/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rexmit_bw: Option<i64>,
    /// SRT Stream ID for access control (max 512 chars, per SRT spec).
    /// For callers: sent to the listener during handshake for stream identification.
    /// For listeners: if set, only connections with a matching stream_id are accepted.
    /// Supports both plain strings and the structured `#!::key=value,...` format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// SRT packet filter for FEC (Forward Error Correction).
    /// Format: "fec,cols:10,rows:5,layout:staircase,arq:onreq"
    /// Negotiated with peer during handshake. Both sides must agree on parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_filter: Option<String>,
    /// Maximum bandwidth in bytes/sec (0 = unlimited). Limits total send rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bw: Option<i64>,
    /// Estimated input bandwidth in bytes/sec. Helps congestion control estimate
    /// the rate. 0 = auto-detect from data rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_bw: Option<i64>,
    /// Overhead bandwidth as percentage (5-100) over the input rate for congestion
    /// control. Default: 25%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_bw: Option<i32>,
    /// Enforce encryption: reject connections from unencrypted peers. Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_encryption: Option<bool>,
    /// Connection timeout in seconds. Default: 3s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    /// Flow control window size in packets (default: 25600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_flag_size: Option<u32>,
    /// Send buffer size in packets (default: 8192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_buffer_size: Option<u32>,
    /// Receive buffer size in packets (default: 8192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_buffer_size: Option<u32>,
    /// IP Type of Service / DSCP value (0-255). Default: 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_tos: Option<i32>,
    /// Retransmission algorithm: "default" or "reduced" (v1.5.5 efficient algo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_algo: Option<String>,
    /// Extra delay in ms before sender drops a packet (-1 = off). Default: -1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_drop_delay: Option<i32>,
    /// Maximum reorder tolerance in packets (0 = adaptive). Default: 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_max_ttl: Option<i32>,
    /// Key material refresh rate in packets. Default: ~16M.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_refresh_rate: Option<u32>,
    /// Key material pre-announce in packets before refresh. Default: 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_pre_announce: Option<u32>,
    /// Maximum payload size per SRT packet (default: 1316 for MPEG-TS 7×188).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<u32>,
    /// Maximum Segment Size in bytes (default: 1500). Controls the maximum UDP
    /// packet size including SRT header. Adjust for non-standard MTU paths
    /// (e.g., lower for VPNs/tunnels, higher for jumbo frames up to 9000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mss: Option<u32>,
    /// Enable too-late packet drop (default: true in live mode). When enabled,
    /// packets that arrive after their TSBPD delivery deadline are dropped.
    /// Disable for recording/archival use cases where completeness matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tlpkt_drop: Option<bool>,
    /// IP Time To Live (default: 64). Range: 1-255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_ttl: Option<i32>,
    /// Optional: enable 2022-7 redundancy on input (merge from two SRT legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<SrtRedundancyConfig>,
    /// Transport mode for the SRT input stream. `"ts"` (default) auto-detects
    /// RTP/TS or raw TS as the existing path does. `"audio_302m"` forces a
    /// SMPTE 302M LPCM-in-MPEG-TS demux: the input task locates the BSSD
    /// audio elementary stream in the PMT, reassembles its private PES
    /// packets, depacketizes the LPCM samples, and republishes them as RTP
    /// audio packets onto the broadcast channel. Implementation arrives in
    /// a follow-up — until then `audio_302m` is accepted by the validator
    /// but the runtime falls back to standard auto-detect with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
}

/// RTMP input configuration — runs an RTMP server that accepts publish connections.
///
/// OBS, ffmpeg, or any RTMP encoder can push to `rtmp://<edge_ip>:<port>/<app>/<stream_key>`.
/// The received H.264 video and AAC audio are remuxed into MPEG-TS and pushed
/// through the broadcast channel like any other input.
///
/// # Example config
///
/// ```json
/// {
///   "type": "rtmp",
///   "listen_addr": "0.0.0.0:1935",
///   "app": "live",
///   "stream_key": "my_stream"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtmpInputConfig {
    /// RTMP listen address, e.g. "0.0.0.0:1935"
    pub listen_addr: String,
    /// RTMP application name. The publisher must use this in the URL path.
    /// e.g. "live" → publisher connects to `rtmp://host:port/live/stream_key`
    #[serde(default = "default_rtmp_app")]
    pub app: String,
    /// Optional stream key for authentication. If set, only publishers using
    /// this exact stream key are accepted. If absent, any stream key is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<String>,
    /// Maximum number of simultaneous publishers allowed (default: 1).
    /// For broadcast use, typically only one publisher is active at a time.
    #[serde(default = "default_max_publishers")]
    pub max_publishers: u32,
}

fn default_rtmp_app() -> String {
    "live".to_string()
}

fn default_max_publishers() -> u32 {
    1
}

/// RTSP transport mode for receiving RTP packets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum RtspTransport {
    /// RTP over TCP interleaved in the RTSP connection (most reliable, works through firewalls).
    #[default]
    #[serde(rename = "tcp")]
    Tcp,
    /// RTP over UDP (lower latency, may not work through NAT/firewalls).
    #[serde(rename = "udp")]
    Udp,
}

/// RTSP input configuration — pulls media from an RTSP source (IP cameras, media servers).
///
/// Uses the `retina` pure-Rust RTSP client to handle DESCRIBE/SETUP/PLAY signaling
/// and receive H.264 video (+ optional AAC audio) via RTP. The received media is
/// muxed into MPEG-TS and published to the flow's broadcast channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtspInputConfig {
    /// RTSP source URL, e.g. "rtsp://camera.local:554/stream1"
    pub rtsp_url: String,
    /// Username for RTSP authentication (Digest or Basic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Password for RTSP authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// RTP transport mode: "tcp" (interleaved, default) or "udp".
    #[serde(default)]
    pub transport: RtspTransport,
    /// Connection timeout in seconds (default: 10).
    #[serde(default = "default_rtsp_timeout")]
    pub timeout_secs: u64,
    /// Reconnect delay in seconds after connection loss (default: 5).
    #[serde(default = "default_rtsp_reconnect")]
    pub reconnect_delay_secs: u64,
}

fn default_rtsp_timeout() -> u64 {
    10
}

fn default_rtsp_reconnect() -> u64 {
    5
}

/// WebRTC/WHIP input configuration — runs a WHIP server endpoint accepting
/// WebRTC contributions from publishers (OBS, browsers, etc.).
///
/// The WHIP endpoint is auto-generated at `/api/v1/flows/{flow_id}/whip`.
/// Publishers POST an SDP offer and receive an SDP answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebrtcInputConfig {
    /// Optional Bearer token required from WHIP publishers for authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// When true, only video is received (audio tracks from publisher ignored).
    #[serde(default)]
    pub video_only: bool,
    /// Public IP to advertise in ICE candidates (for NAT traversal).
    /// If not set, auto-detects from the bound socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    /// STUN server URL for ICE candidate gathering (optional, ICE-lite doesn't need it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stun_server: Option<String>,
}

/// WebRTC/WHEP input configuration — pulls media from an external WHEP server.
///
/// The edge acts as a WHEP client: it POSTs an SDP offer to the WHEP endpoint
/// and establishes a receive-only WebRTC session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhepInputConfig {
    /// WHEP endpoint URL to pull media from.
    pub whep_url: String,
    /// Optional Bearer token for WHEP authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// When true, only video is received (audio tracks ignored).
    #[serde(default)]
    pub video_only: bool,
}

fn default_latency() -> u64 {
    120
}

fn default_peer_idle_timeout() -> u64 {
    30
}

/// Output destination configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputConfig {
    /// Send RTP-wrapped packets over UDP (with RTP headers, optional FEC)
    #[serde(rename = "rtp")]
    Rtp(RtpOutputConfig),
    /// Send raw MPEG-TS over UDP (no RTP headers, 7×188-byte datagrams)
    #[serde(rename = "udp")]
    Udp(UdpOutputConfig),
    /// Send RTP over SRT
    #[serde(rename = "srt")]
    Srt(SrtOutputConfig),
    /// Publish to RTMP/RTMPS server (e.g. Twitch, YouTube)
    #[serde(rename = "rtmp")]
    Rtmp(RtmpOutputConfig),
    /// Send TS segments via HLS ingest (e.g. YouTube HLS)
    #[serde(rename = "hls")]
    Hls(HlsOutputConfig),
    /// Send via WebRTC/WHIP
    #[serde(rename = "webrtc")]
    Webrtc(WebrtcOutputConfig),
    /// Send SMPTE ST 2110-30 uncompressed PCM audio over RTP (L16/L24).
    #[serde(rename = "st2110_30")]
    St2110_30(St2110AudioOutputConfig),
    /// Send SMPTE ST 2110-31 AES3 transparent audio over RTP.
    #[serde(rename = "st2110_31")]
    St2110_31(St2110AudioOutputConfig),
    /// Send SMPTE ST 2110-40 ancillary data over RTP.
    #[serde(rename = "st2110_40")]
    St2110_40(St2110AncillaryOutputConfig),
    /// Send PCM audio via RFC 3551 RTP/UDP without ST 2110 constraints
    /// (no PTP, no RFC 7273 timing, no NMOS clock_domain advertising).
    /// Supports the same `transcode` block as ST 2110-30 outputs and
    /// optionally MPEG-TS / SMPTE 302M wrapping via `transport_mode`.
    #[serde(rename = "rtp_audio")]
    RtpAudio(RtpAudioOutputConfig),
}

impl OutputConfig {
    /// Returns the unique identifier of this output, regardless of its concrete type.
    pub fn id(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.id,
            OutputConfig::Udp(c) => &c.id,
            OutputConfig::Srt(c) => &c.id,
            OutputConfig::Rtmp(c) => &c.id,
            OutputConfig::Hls(c) => &c.id,
            OutputConfig::Webrtc(c) => &c.id,
            OutputConfig::St2110_30(c) => &c.id,
            OutputConfig::St2110_31(c) => &c.id,
            OutputConfig::St2110_40(c) => &c.id,
            OutputConfig::RtpAudio(c) => &c.id,
        }
    }

}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpOutputConfig {
    /// Unique output ID within this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Destination address, e.g. "192.168.1.100:5004" or "239.1.2.1:5004"
    pub dest_addr: String,
    /// Source bind address (optional, defaults to "0.0.0.0:0")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Network interface IP for multicast send (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional: encode SMPTE 2022-1 FEC on output
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fec_encode: Option<FecConfig>,
    /// DSCP value for QoS marking on egress (RP 2129 C10), range 0-63.
    /// Default: 46 (Expedited Forwarding per RFC 4594).
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    /// Optional: enable SMPTE 2022-7 redundancy (duplicate to two UDP legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RtpOutputRedundancyConfig>,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// MPEG-TS program before sending. Default: passthrough (full MPTS).
    /// Must be > 0; program_number 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
}

fn default_dscp() -> u8 {
    46 // Expedited Forwarding per RFC 4594
}

/// Raw UDP output — sends MPEG-TS datagrams without RTP headers.
///
/// Sends TS-aligned datagrams (7 × 188 = 1316 bytes each). If the input
/// is RTP-wrapped, the RTP header is stripped before sending. Suitable for
/// feeding ffplay, VLC, multicast distribution, or any receiver expecting
/// raw MPEG-TS over UDP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpOutputConfig {
    /// Unique output ID within this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Destination address, e.g. "192.168.1.100:5004" or "239.1.2.1:5004"
    pub dest_addr: String,
    /// Source bind address (optional, defaults to "0.0.0.0:0")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Network interface IP for multicast send (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// DSCP value for QoS marking on egress, range 0-63.
    /// Default: 46 (Expedited Forwarding per RFC 4594).
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// MPEG-TS program before sending. Default: passthrough (full MPTS).
    /// Must be > 0; program_number 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Transport mode for this UDP output. `"ts"` (default) sends raw
    /// MPEG-TS as the existing path does. `"audio_302m"` runs the per-output
    /// transcode + 302M-in-MPEG-TS pipeline and sends the resulting
    /// 7×188-byte TS chunks as plain UDP datagrams — useful for legacy
    /// hardware decoders that expect raw MPEG-TS over UDP carrying SMPTE
    /// 302M LPCM audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtOutputConfig {
    /// Unique output ID within this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address
    pub local_addr: String,
    /// Remote address (required for caller and rendezvous)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// SRT latency in ms (sets both receiver and peer/sender latency).
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
    /// Receiver-side latency override in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_latency_ms: Option<u64>,
    /// Peer/sender-side latency override in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_latency_ms: Option<u64>,
    /// Peer idle timeout in seconds. Connection is dropped if no data
    /// is received for this duration. Default: 30s (suitable for broadcast).
    #[serde(default = "default_peer_idle_timeout")]
    pub peer_idle_timeout_secs: u64,
    /// Optional AES encryption passphrase
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// AES key length: 16, 24, or 32
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key_len: Option<usize>,
    /// Encryption cipher mode: "aes-ctr" (default) or "aes-gcm" (authenticated encryption).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypto_mode: Option<String>,
    /// Maximum retransmission bandwidth in bytes/sec (Token Bucket shaper).
    /// -1 = unlimited (default), 0 = disable retransmissions, >0 = cap in bytes/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rexmit_bw: Option<i64>,
    /// SRT Stream ID for access control (max 512 chars, per SRT spec).
    /// For callers: sent to the listener during handshake for stream identification.
    /// For listeners: if set, only connections with a matching stream_id are accepted.
    /// Supports both plain strings and the structured `#!::key=value,...` format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// SRT packet filter for FEC (Forward Error Correction).
    /// Format: "fec,cols:10,rows:5,layout:staircase,arq:onreq"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_filter: Option<String>,
    /// Maximum bandwidth in bytes/sec (0 = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bw: Option<i64>,
    /// Estimated input bandwidth in bytes/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_bw: Option<i64>,
    /// Overhead bandwidth as percentage (5-100). Default: 25%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_bw: Option<i32>,
    /// Enforce encryption: reject unencrypted peers. Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_encryption: Option<bool>,
    /// Connection timeout in seconds. Default: 3s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    /// Flow control window size in packets (default: 25600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_flag_size: Option<u32>,
    /// Send buffer size in packets (default: 8192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_buffer_size: Option<u32>,
    /// Receive buffer size in packets (default: 8192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_buffer_size: Option<u32>,
    /// IP Type of Service / DSCP value (0-255). Default: 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_tos: Option<i32>,
    /// Retransmission algorithm: "default" or "reduced".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_algo: Option<String>,
    /// Extra delay in ms before sender drops a packet (-1 = off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_drop_delay: Option<i32>,
    /// Maximum reorder tolerance in packets (0 = adaptive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_max_ttl: Option<i32>,
    /// Key material refresh rate in packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_refresh_rate: Option<u32>,
    /// Key material pre-announce in packets before refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_pre_announce: Option<u32>,
    /// Maximum payload size per SRT packet (default: 1316).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<u32>,
    /// Maximum Segment Size in bytes (default: 1500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mss: Option<u32>,
    /// Enable too-late packet drop (default: true in live mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tlpkt_drop: Option<bool>,
    /// IP Time To Live (default: 64). Range: 1-255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_ttl: Option<i32>,
    /// Optional: enable 2022-7 redundancy on output (duplicate to two SRT legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<SrtRedundancyConfig>,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// MPEG-TS program before sending. Default: passthrough (full MPTS).
    /// Must be > 0; program_number 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Transport mode for this SRT output. `"ts"` (default) sends whatever
    /// the upstream broadcast channel produces (RTP-wrapped or raw TS).
    /// `"audio_302m"` runs the per-output transcode + 302M packetizer
    /// + TS muxer pipeline and ships 7×188-byte SMPTE 302M-in-MPEG-TS
    /// datagrams over SRT. Interoperable with `ffmpeg -c:a s302m`,
    /// `srt-live-transmit`, and broadcast hardware decoders that expect
    /// 302M LPCM in MPEG-TS over SRT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
}

/// SRT connection mode, determining which side initiates the handshake.
///
/// The mode affects which address fields are required in the configuration:
/// - `Caller` and `Rendezvous` require a `remote_addr`.
/// - `Listener` only needs a `local_addr` to bind on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrtMode {
    /// Active mode: the SRT endpoint initiates the connection to a remote listener.
    /// Requires `remote_addr` to be specified. This is the most common mode for
    /// sending streams to a known destination.
    #[serde(rename = "caller")]
    Caller,
    /// Passive mode: the SRT endpoint binds to `local_addr` and waits for an
    /// incoming connection from a remote caller. Does not require `remote_addr`.
    /// Commonly used on ingest servers that accept streams from field encoders.
    #[serde(rename = "listener")]
    Listener,
    /// Symmetric mode: both endpoints simultaneously attempt to connect to each
    /// other. Requires `remote_addr`. Both sides must use rendezvous mode and
    /// know each other's address. Useful for NAT traversal scenarios.
    #[serde(rename = "rendezvous")]
    Rendezvous,
}

/// SMPTE 2022-7 redundancy config for an SRT leg.
/// The primary SRT config in the parent is leg 1; this struct defines leg 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtRedundancyConfig {
    /// SRT mode for the second leg
    pub mode: SrtMode,
    /// Local bind address for leg 2
    pub local_addr: String,
    /// Remote address for leg 2 (for caller/rendezvous)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// SRT latency for leg 2
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
    /// Receiver-side latency override for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_latency_ms: Option<u64>,
    /// Peer/sender-side latency override for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_latency_ms: Option<u64>,
    /// Peer idle timeout in seconds for leg 2. Default: 30s.
    #[serde(default = "default_peer_idle_timeout")]
    pub peer_idle_timeout_secs: u64,
    /// Optional AES passphrase for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// AES key length for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key_len: Option<usize>,
    /// Encryption cipher mode for leg 2: "aes-ctr" (default) or "aes-gcm".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypto_mode: Option<String>,
    /// Maximum retransmission bandwidth in bytes/sec for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rexmit_bw: Option<i64>,
    /// SRT Stream ID for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// SRT packet filter for FEC on leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_filter: Option<String>,
    /// Maximum bandwidth in bytes/sec for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bw: Option<i64>,
    /// Estimated input bandwidth in bytes/sec for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_bw: Option<i64>,
    /// Overhead bandwidth as percentage for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overhead_bw: Option<i32>,
    /// Enforce encryption for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_encryption: Option<bool>,
    /// Connection timeout in seconds for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    /// Flow control window size for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_flag_size: Option<u32>,
    /// Send buffer size in packets for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_buffer_size: Option<u32>,
    /// Receive buffer size in packets for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_buffer_size: Option<u32>,
    /// IP TOS / DSCP for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_tos: Option<i32>,
    /// Retransmission algorithm for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_algo: Option<String>,
    /// Extra delay before sender drop for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_drop_delay: Option<i32>,
    /// Maximum reorder tolerance for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_max_ttl: Option<i32>,
    /// Key material refresh rate for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_refresh_rate: Option<u32>,
    /// Key material pre-announce for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub km_pre_announce: Option<u32>,
    /// Maximum payload size for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<u32>,
    /// Maximum Segment Size for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mss: Option<u32>,
    /// Enable too-late packet drop for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tlpkt_drop: Option<bool>,
    /// IP Time To Live for leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_ttl: Option<i32>,
}

/// SMPTE 2022-7 redundancy config for an RTP input (leg 2).
/// The primary bind_addr in the parent RtpInputConfig is leg 1; this defines leg 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpRedundancyConfig {
    /// Bind address for leg 2, e.g. "239.1.1.2:5000" or "0.0.0.0:5002"
    pub bind_addr: String,
    /// Network interface IP for multicast join on leg 2 (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
}

/// SMPTE 2022-7 redundancy config for an RTP output (leg 2).
/// The primary dest_addr in the parent RtpOutputConfig is leg 1; this defines leg 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpOutputRedundancyConfig {
    /// Destination address for leg 2, e.g. "239.1.2.1:5004"
    pub dest_addr: String,
    /// Source bind address for leg 2 (optional, defaults to "0.0.0.0:0")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Network interface IP for multicast send on leg 2 (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// DSCP value for leg 2 (optional, defaults to parent's dscp)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dscp: Option<u8>,
}

/// RTMP/RTMPS output configuration for publishing to streaming platforms.
///
/// Demuxes H.264/AAC from the MPEG-2 TS stream, muxes into FLV, and publishes
/// via the RTMP protocol. Supports RTMPS (RTMP over TLS) via `rustls`.
///
/// # Limitations
/// - Output only (publish). RTMP input is not supported.
/// - Only H.264 video and AAC audio are supported (no HEVC/VP9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtmpOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// RTMP destination URL, e.g. "rtmp://live.twitch.tv/app" or "rtmps://a.rtmps.youtube.com/live2"
    pub dest_url: String,
    /// Stream key for authentication.
    pub stream_key: String,
    /// Reconnect delay in seconds after connection failure (default: 5).
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,
    /// Maximum reconnection attempts. None = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reconnect_attempts: Option<u32>,
    /// MPEG-TS program_number to extract elementary streams from when the
    /// input is an MPTS. `None` = lock onto the lowest program_number found
    /// in the PAT (deterministic default). RTMP is single-program by spec,
    /// so this only changes which program is published — it does not
    /// preserve MPTS structure. Must be > 0 if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional audio encode block. When set, this output will decode the
    /// input audio (must be AAC-LC), re-encode it via the ffmpeg sidecar
    /// encoder, and emit the result as the audio elementary stream of the
    /// FLV publish. RTMP-FLV only supports AAC, so the codec field must
    /// be one of `aac_lc`, `he_aac_v1`, `he_aac_v2`. Requires ffmpeg in
    /// PATH at runtime — outputs without `audio_encode` set keep working
    /// without ffmpeg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
}

fn default_reconnect_delay() -> u64 {
    5
}

/// HLS ingest output configuration for YouTube HLS or similar endpoints.
///
/// Segments the MPEG-2 TS data into time-bounded chunks and uploads them
/// via HTTP PUT/POST along with a rolling M3U8 playlist. Supports HEVC/HDR
/// content that RTMP cannot carry.
///
/// # Limitations
/// - Output only. Segment-based transport inherently adds 1-4s latency.
/// - The ingest endpoint must support HTTP PUT or POST for segment upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlsOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// HLS ingest URL (base URL for segment and playlist uploads).
    pub ingest_url: String,
    /// Target segment duration in seconds (default: 2.0, range: 0.5-10.0).
    #[serde(default = "default_segment_duration")]
    pub segment_duration_secs: f64,
    /// Optional authentication token (sent as Authorization: Bearer header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Maximum number of segments in the rolling playlist (default: 5).
    #[serde(default = "default_max_segments")]
    pub max_segments: usize,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// MPEG-TS program before segmenting. Default: passthrough (full MPTS
    /// in each `.ts` segment). Must be > 0; program_number 0 is reserved
    /// for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional audio encode block. When set, this output stops being a
    /// pure TS-passthrough segmenter: it demuxes video + audio, re-encodes
    /// audio via the ffmpeg sidecar encoder, and re-muxes a fresh TS for
    /// the segment buffer. HLS-TS supports `aac_lc`, `he_aac_v1`,
    /// `he_aac_v2`, `mp2`, and `ac3`. Requires ffmpeg in PATH at runtime.
    /// The same-codec fast path (codec=aac_lc with no overrides on an
    /// AAC-LC source) skips the re-mux and falls back to passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
}

fn default_segment_duration() -> f64 {
    2.0
}

fn default_max_segments() -> usize {
    5
}

/// WebRTC output mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum WebrtcOutputMode {
    /// Push media to an external WHIP endpoint (edge acts as WHIP client).
    #[default]
    #[serde(rename = "whip_client")]
    WhipClient,
    /// Serve media to browser viewers via WHEP (edge acts as WHEP server).
    #[serde(rename = "whep_server")]
    WhepServer,
}

/// WebRTC output configuration.
///
/// Supports two modes:
/// - **WHIP client** (`mode: "whip_client"`): Pushes media to an external
///   WHIP endpoint (e.g., CDN, cloud encoder). Requires `whip_url`.
/// - **WHEP server** (`mode: "whep_server"`): Serves media to browser
///   viewers. The WHEP endpoint is auto-generated at
///   `/api/v1/flows/{flow_id}/whep`.
///
/// Extracts H.264 NALUs from the MPEG-2 TS stream and repacketizes as
/// RFC 6184 RTP. Opus audio is passed through when available.
///
/// # Audio limitations
/// - Only Opus passthrough is supported. AAC→Opus transcoding requires
///   C libraries and is NOT available in the pure-Rust build.
/// - If the source TS carries AAC audio, the audio track will be
///   automatically omitted for WebRTC outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebrtcOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Output mode: `"whip_client"` or `"whep_server"`. Default: `"whip_client"`.
    #[serde(default)]
    pub mode: WebrtcOutputMode,
    /// WHIP endpoint URL (required for `whip_client` mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whip_url: Option<String>,
    /// Optional Bearer token for WHIP/WHEP authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Maximum concurrent viewers (WHEP server mode only, default: 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_viewers: Option<u32>,
    /// Public IP to advertise in ICE candidates (for NAT traversal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    /// When true, only video is sent (audio track omitted).
    #[serde(default)]
    pub video_only: bool,
    /// MPEG-TS program_number to extract elementary streams from when the
    /// input is an MPTS. `None` = lock onto the lowest program_number found
    /// in the PAT (deterministic default). WebRTC is single-program by spec,
    /// so this only changes which program is sent. Must be > 0 if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional audio encode block. When set, the output decodes the
    /// AAC-LC audio from the input TS, encodes it via the ffmpeg sidecar
    /// encoder, and writes the resulting Opus packets to the WebRTC
    /// audio MID. WebRTC realistically only supports `opus` here.
    /// Requires `video_only=false` (an audio MID must be negotiated in
    /// SDP) and ffmpeg in PATH at runtime. This is the only path that
    /// gets audio onto a WebRTC output today; without `audio_encode`
    /// the output is video-only by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
}

/// Audio encoder configuration block. Used by RTMP, HLS, and WebRTC
/// outputs (as `audio_encode`) to enable PCM → compressed-audio re-encoding
/// via the ffmpeg sidecar encoder (see `engine::audio_encode`).
///
/// The valid codec set depends on the output container — see the
/// per-output documentation. Validation in `config::validation` enforces
/// the matrix at config load time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioEncodeConfig {
    /// Codec name. One of:
    /// `aac_lc`, `he_aac_v1`, `he_aac_v2`, `opus`, `mp2`, `ac3`.
    pub codec: String,
    /// Optional bitrate in kbps. Defaults to a per-codec value when
    /// unset (AAC-LC=128, HE-AAC-v1=64, HE-AAC-v2=32, Opus=96,
    /// MP2=192, AC-3=192). Range: 16..=512 kbps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Optional output sample rate in Hz. Defaults to the input audio
    /// sample rate. Allowed values: 8000, 16000, 22050, 24000, 32000,
    /// 44100, 48000. (Opus is always carried at 48 kHz on the wire
    /// regardless of this field.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Optional output channel count (1 or 2). Defaults to the input
    /// audio channel count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
}

/// SMPTE 2022-1 FEC parameters
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FecConfig {
    /// Number of columns (L parameter), typically 5-20
    pub columns: u8,
    /// Number of rows (D parameter), typically 5-20
    pub rows: u8,
}

// ─────────────────────────── SMPTE ST 2110 ───────────────────────────
//
// Phase 1 ST 2110 support: -30 (PCM audio), -31 (AES3 transparent), -40
// (ancillary data). All three are RTP-over-UDP and reuse the existing
// `RtpPacket` abstraction. Inputs and outputs may bind to two physically
// disjoint networks (Red/Blue) for SMPTE 2022-7 hitless redundancy.
//
// The actual packetizers/depacketizers and I/O tasks live under
// `src/engine/st2110/`. The runtime spawn entry points are stubbed in
// step 1 and implemented in step 4 of the Phase 1 plan.

/// Second-leg bind for SMPTE 2022-7 dual-network (Red/Blue) operation.
///
/// The primary bind/dest in the parent input/output config is leg 1 (typically
/// the "Red" network); this struct defines leg 2 ("Blue"). Both legs receive
/// or transmit the same RTP stream; on input, the existing `HitlessMerger`
/// dedupes by RTP sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedBlueBindConfig {
    /// Second-leg socket address. For an input this is the bind address;
    /// for an output it is the destination address.
    pub addr: String,
    /// Network interface IP for multicast on leg 2 (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
}

/// SMPTE ST 2110-30 / -31 audio input configuration.
///
/// Both -30 (linear PCM L16/L24) and -31 (AES3 transparent) share this struct.
/// The variant in `InputConfig` (`St2110_30` vs `St2110_31`) selects the payload
/// format at runtime. AES3 always uses 24-bit sub-frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110AudioInputConfig {
    /// Primary (Red) bind address, e.g. "239.10.10.1:5004" or "0.0.0.0:5004".
    pub bind_addr: String,
    /// Network interface IP for multicast join on the primary leg (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional SMPTE 2022-7 second leg (Blue network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    /// Sample rate in Hz. ST 2110-30 PM profile: 48000. AM profile: 96000. Default: 48000.
    #[serde(default = "default_st2110_sample_rate")]
    pub sample_rate: u32,
    /// Bit depth per sample. -30 supports 16 or 24 (L16/L24). -31 is always 24
    /// (AES3 sub-frames). Default: 24.
    #[serde(default = "default_st2110_bit_depth")]
    pub bit_depth: u8,
    /// Channel count: 1, 2, 4, 8, or 16. Default: 2.
    #[serde(default = "default_st2110_channels")]
    pub channels: u8,
    /// RTP packet time in microseconds. Common values: 125, 250, 333, 500,
    /// 1000, 4000. Default: 1000 (1ms, ST 2110-30 PM profile).
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    /// Dynamic RTP payload type (96..=127). Default: 97.
    #[serde(default = "default_st2110_audio_pt")]
    pub payload_type: u8,
    /// PTP clock domain (IEEE 1588), 0..=127. Inherits from the parent flow
    /// or flow group when omitted. Used by the PTP state reporter to scope
    /// sync monitoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Source IP allow-list (RP 2129 C5). When absent, all sources accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_sources: Option<Vec<String>>,
    /// Maximum ingress bitrate in Mbps (RP 2129 C7). Excess packets dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_mbps: Option<f64>,
}

/// SMPTE ST 2110-30 / -31 audio output configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110AudioOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Primary (Red) destination address.
    pub dest_addr: String,
    /// Source bind address (optional, defaults to "0.0.0.0:0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Network interface IP for multicast send on the primary leg (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional SMPTE 2022-7 second leg (Blue network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    /// Sample rate in Hz (48000 default, 96000 allowed).
    #[serde(default = "default_st2110_sample_rate")]
    pub sample_rate: u32,
    /// Bit depth per sample (16 or 24 for -30; 24 for -31).
    #[serde(default = "default_st2110_bit_depth")]
    pub bit_depth: u8,
    /// Channel count (1, 2, 4, 8, 16).
    #[serde(default = "default_st2110_channels")]
    pub channels: u8,
    /// RTP packet time in microseconds.
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    /// Dynamic RTP payload type (96..=127).
    #[serde(default = "default_st2110_audio_pt")]
    pub payload_type: u8,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// DSCP value for QoS marking (0-63). Default: 46 (EF per RFC 4594).
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    /// Optional fixed RTP SSRC. When omitted, a random SSRC is generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<u32>,
    /// Optional per-output PCM transcode block. When set, the spawn module
    /// inserts a [`crate::engine::audio_transcode::TranscodeStage`] between
    /// the broadcast subscriber and the RTP send loop, allowing the output
    /// to differ from the input in sample rate, bit depth, channel layout,
    /// packet time, and payload type. When unset, byte-identical passthrough
    /// is used (default behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
}

/// Generic RFC 3551 PCM-over-RTP audio input — no ST 2110 baggage.
///
/// Identical wire format to ST 2110-30 but with relaxed constraints:
///
/// - Sample rate: any of 32000, 44100, 48000, 88200, 96000 Hz
/// - Bit depth: 16 or 24 (L16/L24)
/// - Channels: 1..=16
/// - Packet time: any reasonable value (default 1000 µs)
/// - No PTP requirement, no RFC 7273 timing reference
/// - No NMOS `clock_domain` advertising
///
/// Use this when bridging audio over the public internet, between studios
/// without a shared PTP fabric, or to interoperate with hardware/software
/// that produces RFC 3551 RTP audio (ffmpeg, OBS, GStreamer, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpAudioInputConfig {
    /// Local UDP bind address (e.g. "0.0.0.0:5004" or "239.10.10.1:5004").
    pub bind_addr: String,
    /// Multicast NIC IP for joining (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional SMPTE 2022-7 second leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    /// Sample rate in Hz. One of 32000, 44100, 48000, 88200, 96000.
    pub sample_rate: u32,
    /// PCM bit depth: 16 or 24.
    pub bit_depth: u8,
    /// Channel count: 1..=16.
    pub channels: u8,
    /// RTP packet time in microseconds. Default: 1000 (1 ms).
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    /// Dynamic RTP payload type (96..=127).
    #[serde(default = "default_st2110_audio_pt")]
    pub payload_type: u8,
    /// Source IP allow-list (RP 2129 C5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_sources: Option<Vec<String>>,
}

/// Generic RFC 3551 PCM-over-RTP audio output. See [`RtpAudioInputConfig`]
/// for the rationale. Supports the same `transcode` block as ST 2110-30
/// outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpAudioOutputConfig {
    pub id: String,
    pub name: String,
    pub dest_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u8,
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    #[serde(default = "default_st2110_audio_pt")]
    pub payload_type: u8,
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<u32>,
    /// Optional per-output PCM transcode block (see St2110AudioOutputConfig.transcode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Transport mode: `"rtp"` (default) sends RFC 3551 PCM over UDP,
    /// `"audio_302m"` wraps the PCM as SMPTE 302M LPCM in MPEG-TS and sends
    /// the TS via RTP/MP2T (RFC 2250). The 302M mode is implemented in
    /// Chunks 5–7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
}

/// SMPTE ST 2110-40 ancillary data input configuration.
///
/// Carries SCTE-104 ad markers, SMPTE 12M timecode, CEA-608/708 captions, and
/// other ancillary data per RFC 8331.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110AncillaryInputConfig {
    /// Primary (Red) bind address.
    pub bind_addr: String,
    /// Network interface IP for multicast join on the primary leg (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional SMPTE 2022-7 second leg (Blue network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    /// Dynamic RTP payload type (96..=127). Default: 100.
    #[serde(default = "default_st2110_anc_pt")]
    pub payload_type: u8,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Source IP allow-list (RP 2129 C5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_sources: Option<Vec<String>>,
}

/// SMPTE ST 2110-40 ancillary data output configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110AncillaryOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Primary (Red) destination address.
    pub dest_addr: String,
    /// Source bind address (optional, defaults to "0.0.0.0:0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    /// Network interface IP for multicast send on the primary leg (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional SMPTE 2022-7 second leg (Blue network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    /// Dynamic RTP payload type (96..=127).
    #[serde(default = "default_st2110_anc_pt")]
    pub payload_type: u8,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// DSCP value for QoS marking (0-63). Default: 46.
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    /// Optional fixed RTP SSRC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<u32>,
}

/// SMPTE ST 2110 flow group ("essence bundle") configuration.
///
/// A flow group bundles multiple essence flows that share PTP timing and NMOS
/// activation. Member flows are referenced by their `FlowConfig.id`. The group's
/// `clock_domain` is the default for all members; an individual flow may override
/// it. Existing single-flow configs do not need to use flow groups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowGroupConfig {
    /// Unique identifier for this flow group (max 64 chars).
    pub id: String,
    /// Human-readable name (max 256 chars).
    pub name: String,
    /// PTP clock domain (IEEE 1588), 0..=127, shared by all member flows
    /// unless they override it individually.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Member flow IDs. Each must reference a top-level `FlowConfig.id`.
    /// Accepts `flow_ids` as an alias on input so the manager wire format
    /// (which uses `flow_ids`) round-trips through the same struct.
    #[serde(default, alias = "flow_ids")]
    pub flows: Vec<String>,
}

fn default_st2110_sample_rate() -> u32 {
    48_000
}

fn default_st2110_bit_depth() -> u8 {
    24
}

fn default_st2110_channels() -> u8 {
    2
}

fn default_st2110_packet_time_us() -> u32 {
    1_000
}

fn default_st2110_audio_pt() -> u8 {
    97
}

fn default_st2110_anc_pt() -> u8 {
    100
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for Bug C (2026-04-09): the TR-101290 analyzer must only
    /// run on inputs that actually carry MPEG-TS bytes. Audio-only inputs
    /// (ST 2110-30/-31, `rtp_audio`) and ANC inputs (ST 2110-40) carry PCM
    /// or RFC 8331 ancillary data on the broadcast channel, and feeding
    /// those into TR-101290 produces a stream of "sync lost" warnings
    /// every second forever. This test pins down which inputs are TS
    /// carriers so anyone adding a new input variant is forced to make a
    /// conscious classification.
    #[test]
    fn input_config_is_ts_carrier_classification() {
        // Build the variants from JSON to avoid coupling the regression
        // test to every field on every input struct.
        let parse = |json: &str| -> InputConfig {
            serde_json::from_str(json).expect("test JSON must parse")
        };

        // Non-TS — analyzer must NOT spawn for these.
        assert!(!parse(
            r#"{"type":"st2110_30","bind_addr":"127.0.0.1:5000","sample_rate":48000,"channels":2,"bit_depth":24,"packet_duration_us":1000}"#
        ).is_ts_carrier());
        assert!(!parse(
            r#"{"type":"st2110_31","bind_addr":"127.0.0.1:5000","sample_rate":48000,"channels":2,"bit_depth":24,"packet_duration_us":1000}"#
        ).is_ts_carrier());
        assert!(!parse(
            r#"{"type":"st2110_40","bind_addr":"127.0.0.1:5000"}"#
        ).is_ts_carrier());
        assert!(!parse(
            r#"{"type":"rtp_audio","bind_addr":"127.0.0.1:5000","sample_rate":48000,"channels":2,"bit_depth":24}"#
        ).is_ts_carrier());

        // TS-carrying — analyzer must spawn for these.
        assert!(parse(
            r#"{"type":"udp","bind_addr":"127.0.0.1:5000"}"#
        ).is_ts_carrier());
        assert!(parse(
            r#"{"type":"rtp","bind_addr":"127.0.0.1:5000"}"#
        ).is_ts_carrier());
    }

    #[test]
    fn test_roundtrip_config() {
        let config = AppConfig {
            version: 1,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
            flows: vec![FlowConfig {
                id: "test-flow".to_string(),
                name: "Test Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    fec_decode: None,
                    allowed_sources: None,
                    allowed_payload_types: None,
                    max_bitrate_mbps: None,
                    tr07_mode: None,
                    redundancy: None,
                }),
                outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                    id: "out-1".to_string(),
                    name: "Output 1".to_string(),
                    dest_addr: "127.0.0.1:5004".to_string(),
                    bind_addr: None,
                    interface_addr: None,
                    fec_encode: None,
                    dscp: default_dscp(),
                    redundancy: None,
                    program_number: None,
                })],
            }],
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.flows.len(), 1);
        assert_eq!(parsed.flows[0].id, "test-flow");
    }

    #[test]
    fn test_srt_config_with_redundancy() {
        let json = r#"{
            "version": 1,
            "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
            "flows": [{
                "id": "srt-flow",
                "name": "SRT Flow",
                "enabled": true,
                "input": {
                    "type": "srt",
                    "mode": "listener",
                    "local_addr": "0.0.0.0:9000",
                    "latency_ms": 500,
                    "redundancy": {
                        "mode": "listener",
                        "local_addr": "0.0.0.0:9001",
                        "latency_ms": 500
                    }
                },
                "outputs": [{
                    "type": "rtp",
                    "id": "out-1",
                    "name": "Output",
                    "dest_addr": "192.168.1.50:5004"
                }]
            }]
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.flows.len(), 1);
        if let InputConfig::Srt(srt) = &config.flows[0].input {
            assert!(srt.redundancy.is_some());
        } else {
            panic!("Expected SRT input");
        }
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.version, 1);
        assert_eq!(config.server.listen_port, 8080);
        assert!(config.flows.is_empty());
    }
}
