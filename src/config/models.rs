use serde::{Deserialize, Serialize};

/// Root configuration, persisted to config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Schema version for forward compatibility
    pub version: u32,
    /// API server configuration
    pub server: ServerConfig,
    /// Optional web monitoring dashboard
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorConfig>,
    /// List of all configured flows
    #[serde(default)]
    pub flows: Vec<FlowConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            server: ServerConfig::default(),
            monitor: None,
            flows: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// API listen address, e.g. "0.0.0.0"
    pub listen_addr: String,
    /// API listen port, default 8080
    pub listen_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".to_string(),
            listen_port: 8080,
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

/// Input source configuration -- either RTP/UDP or SRT
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputConfig {
    /// Receive RTP over UDP (unicast or multicast)
    #[serde(rename = "rtp")]
    Rtp(RtpInputConfig),
    /// Receive RTP over SRT
    #[serde(rename = "srt")]
    Srt(SrtInputConfig),
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

fn default_latency() -> u64 {
    120
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
}

impl OutputConfig {
    /// Returns the unique identifier of this output, regardless of its concrete type
    /// (RTP or SRT). The ID is unique within the scope of a single flow.
    pub fn id(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.id,
            OutputConfig::Srt(c) => &c.id,
        }
    }

    /// Returns the human-readable display name of this output, regardless of its
    /// concrete type (RTP or SRT).
    pub fn name(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.name,
            OutputConfig::Srt(c) => &c.name,
        }
    }

    /// Returns the transport type name of this output as a static string: `"rtp"` or `"srt"`.
    /// Useful for display, logging, and serialization where the variant tag is needed
    /// without pattern-matching the full enum.
    pub fn type_name(&self) -> &'static str {
        match self {
            OutputConfig::Rtp(_) => "rtp",
            OutputConfig::Srt(_) => "srt",
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
    /// Optional AES passphrase for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// AES key length for leg 2
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key_len: Option<usize>,
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
            server: ServerConfig::default(),
            monitor: None,
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
