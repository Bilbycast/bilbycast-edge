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
    /// Optional manager connection for centralized monitoring
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<crate::manager::ManagerConfig>,
    /// List of all configured flows
    #[serde(default)]
    pub flows: Vec<FlowConfig>,
    /// Optional IP tunnels (relay or direct QUIC tunnels between edge nodes)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<crate::tunnel::TunnelConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            node_id: None,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            flows: Vec::new(),
            tunnels: Vec::new(),
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
    /// The single input source for this flow
    pub input: InputConfig,
    /// One or more output destinations (fan-out)
    pub outputs: Vec<OutputConfig>,
}

fn default_true() -> bool {
    true
}

/// Input source configuration -- RTP/UDP, SRT, or RTMP
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputConfig {
    /// Receive RTP over UDP (unicast or multicast)
    #[serde(rename = "rtp")]
    Rtp(RtpInputConfig),
    /// Receive RTP over SRT
    #[serde(rename = "srt")]
    Srt(SrtInputConfig),
    /// Receive H.264/AAC via RTMP (accept publish from OBS, ffmpeg, etc.)
    #[serde(rename = "rtmp")]
    Rtmp(RtmpInputConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrtInputConfig {
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address, e.g. "0.0.0.0:9000"
    pub local_addr: String,
    /// Remote address (required for caller and rendezvous modes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// SRT latency in milliseconds
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
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
    /// Optional: enable 2022-7 redundancy on input (merge from two SRT legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<SrtRedundancyConfig>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

fn default_latency() -> u64 {
    120
}

fn default_peer_idle_timeout() -> u64 {
    30
}

/// Output destination configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputConfig {
    /// Send RTP over UDP
    #[serde(rename = "rtp")]
    Rtp(RtpOutputConfig),
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
}

impl OutputConfig {
    /// Returns the unique identifier of this output, regardless of its concrete type.
    pub fn id(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.id,
            OutputConfig::Srt(c) => &c.id,
            OutputConfig::Rtmp(c) => &c.id,
            OutputConfig::Hls(c) => &c.id,
            OutputConfig::Webrtc(c) => &c.id,
        }
    }

}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn default_dscp() -> u8 {
    46 // Expedited Forwarding per RFC 4594
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// SRT latency in ms
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
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
    /// Optional: enable 2022-7 redundancy on output (duplicate to two SRT legs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<SrtRedundancyConfig>,
}

/// SRT connection mode, determining which side initiates the handshake.
///
/// The mode affects which address fields are required in the configuration:
/// - `Caller` and `Rendezvous` require a `remote_addr`.
/// - `Listener` only needs a `local_addr` to bind on.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Peer idle timeout in seconds for leg 2. Default: 30s.
    #[serde(default = "default_peer_idle_timeout")]
    pub peer_idle_timeout_secs: u64,
    /// Optional AES passphrase for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// AES key length for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key_len: Option<usize>,
}

/// RTMP/RTMPS output configuration for publishing to streaming platforms.
///
/// Demuxes H.264/AAC from the MPEG-2 TS stream, muxes into FLV, and publishes
/// via the RTMP protocol. Supports RTMPS (RTMP over TLS) via `rustls`.
///
/// # Limitations
/// - Output only (publish). RTMP input is not supported.
/// - Only H.264 video and AAC audio are supported (no HEVC/VP9).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn default_segment_duration() -> f64 {
    2.0
}

fn default_max_segments() -> usize {
    5
}

/// WebRTC/WHIP output configuration.
///
/// Extracts H.264 NALUs from the MPEG-2 TS stream, repacketizes them
/// as RFC 6184 RTP, and sends via a WebRTC PeerConnection established
/// through WHIP (WebRTC-HTTP Ingestion Protocol) signaling.
///
/// # Limitations
/// - Audio: only Opus passthrough is supported. AAC→Opus transcoding
///   requires C libraries and is NOT available in the pure-Rust build.
/// - If the source TS carries AAC audio, set `video_only: true` or
///   the audio track will be silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebrtcOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// WHIP endpoint URL for WebRTC signaling.
    pub whip_url: String,
    /// Optional Bearer token for WHIP authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// When true, only video is sent (audio track omitted).
    /// Use when source audio is AAC and cannot be transcoded to Opus.
    #[serde(default)]
    pub video_only: bool,
}

/// SMPTE 2022-1 FEC parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FecConfig {
    /// Number of columns (L parameter), typically 5-20
    pub columns: u8,
    /// Number of rows (D parameter), typically 5-20
    pub rows: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_config() {
        let config = AppConfig {
            version: 1,
            node_id: None,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            tunnels: Vec::new(),
            flows: vec![FlowConfig {
                id: "test-flow".to_string(),
                name: "Test Flow".to_string(),
                enabled: true,
                input: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    fec_decode: None,
                    allowed_sources: None,
                    allowed_payload_types: None,
                    max_bitrate_mbps: None,
                    tr07_mode: None,
                }),
                outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                    id: "out-1".to_string(),
                    name: "Output 1".to_string(),
                    dest_addr: "127.0.0.1:5004".to_string(),
                    bind_addr: None,
                    interface_addr: None,
                    fec_encode: None,
                    dscp: default_dscp(),
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
