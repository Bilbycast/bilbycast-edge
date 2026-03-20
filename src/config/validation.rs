use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{bail, Result};

use super::models::*;

/// Validates the entire application configuration.
///
/// Performs the following checks:
/// 1. **Duplicate flow IDs**: ensures no two flows share the same `id` value.
/// 2. **Per-flow validation**: delegates to [`validate_flow`] for each flow, which
///    checks ID/name non-emptiness, input validity, duplicate output IDs, and
///    individual output validity.
///
/// # Errors
///
/// Returns an error describing the first validation failure encountered.
pub fn validate_config(config: &AppConfig) -> Result<()> {
    // Validate monitor config if present
    if let Some(ref monitor) = config.monitor {
        let monitor_addr = format!("{}:{}", monitor.listen_addr, monitor.listen_port);
        validate_socket_addr(&monitor_addr, "monitor listen address")?;
        if monitor.listen_addr == config.server.listen_addr
            && monitor.listen_port == config.server.listen_port
        {
            bail!("Monitor listen address must differ from the API server address");
        }
    }

    // Validate TLS config if present
    if let Some(ref tls) = config.server.tls {
        if tls.cert_path.is_empty() {
            bail!("TLS cert_path cannot be empty");
        }
        if tls.key_path.is_empty() {
            bail!("TLS key_path cannot be empty");
        }
    }

    // Validate auth config if present
    if let Some(ref auth) = config.server.auth {
        if auth.enabled {
            if auth.jwt_secret.len() < 32 {
                bail!("Auth jwt_secret must be at least 32 characters for security");
            }
            if auth.clients.is_empty() {
                bail!("Auth is enabled but no clients are configured");
            }
            for client in &auth.clients {
                if client.client_id.is_empty() {
                    bail!("Auth client_id cannot be empty");
                }
                if client.client_secret.is_empty() {
                    bail!("Auth client_secret cannot be empty");
                }
                if client.role != "admin" && client.role != "monitor" {
                    bail!(
                        "Auth client '{}': role must be 'admin' or 'monitor', got '{}'",
                        client.client_id, client.role
                    );
                }
            }
        }
    }

    let mut flow_ids = HashSet::new();
    for flow in &config.flows {
        if !flow_ids.insert(&flow.id) {
            bail!("Duplicate flow ID: {}", flow.id);
        }
        validate_flow(flow)?;
    }
    Ok(())
}

/// Validates a single flow configuration.
///
/// Performs the following checks in order:
/// 1. **Non-empty ID**: the flow `id` must not be an empty string.
/// 2. **Non-empty name**: the flow `name` must not be an empty string.
/// 3. **Input validation**: delegates to `validate_input` to check the input
///    source configuration (bind addresses, SRT mode requirements, FEC params, etc.).
/// 4. **Duplicate output IDs**: ensures no two outputs within this flow share the
///    same `id` value.
/// 5. **Per-output validation**: delegates to `validate_output` for each output.
///
/// # Errors
///
/// Returns an error describing the first validation failure encountered.
pub fn validate_flow(flow: &FlowConfig) -> Result<()> {
    if flow.id.is_empty() {
        bail!("Flow ID cannot be empty");
    }
    if flow.name.is_empty() {
        bail!("Flow name cannot be empty");
    }

    validate_input(&flow.input)?;

    let mut output_ids = HashSet::new();
    for output in &flow.outputs {
        let oid = output.id();
        if !output_ids.insert(oid.to_string()) {
            bail!("Duplicate output ID '{}' in flow '{}'", oid, flow.id);
        }
        validate_output(output)?;
    }

    Ok(())
}

/// Validates the input source configuration for a flow.
///
/// For **RTP** inputs: validates the `bind_addr` as a socket address, the optional
/// `interface_addr` as an IP address, and any FEC decode parameters.
///
/// For **SRT** inputs: validates the `local_addr` as a socket address, checks SRT
/// mode-specific requirements (e.g., `remote_addr` required for caller/rendezvous),
/// validates passphrase length and AES key length, and validates the optional
/// redundancy (leg 2) configuration.
fn validate_input(input: &InputConfig) -> Result<()> {
    match input {
        InputConfig::Rtp(rtp) => {
            validate_socket_addr(&rtp.bind_addr, "RTP input bind_addr")?;
            if let Some(ref iface) = rtp.interface_addr {
                validate_ip_addr(iface, "RTP input interface_addr")?;
            }
            if let Some(ref fec) = rtp.fec_decode {
                validate_fec(fec)?;
            }
            // Validate address family consistency
            validate_rtp_input_addr_family(rtp)?;
            // Validate source IP allow-list
            if let Some(ref sources) = rtp.allowed_sources {
                for src in sources {
                    validate_ip_addr(src, "RTP input allowed_sources")?;
                }
            }
            // Validate payload type allow-list
            if let Some(ref pts) = rtp.allowed_payload_types {
                for &pt in pts {
                    if pt > 127 {
                        bail!("RTP input allowed_payload_types: PT must be 0-127, got {pt}");
                    }
                }
            }
            // Validate rate limit
            if let Some(rate) = rtp.max_bitrate_mbps {
                if rate <= 0.0 {
                    bail!("RTP input max_bitrate_mbps must be positive, got {rate}");
                }
            }
        }
        InputConfig::Srt(srt) => {
            validate_socket_addr(&srt.local_addr, "SRT input local_addr")?;
            validate_srt_common(
                &srt.mode,
                &srt.remote_addr,
                srt.passphrase.as_deref(),
                srt.aes_key_len,
                "SRT input",
            )?;
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT input")?;
            }
        }
        InputConfig::Rtmp(rtmp) => {
            validate_socket_addr(&rtmp.listen_addr, "RTMP input listen_addr")?;
            if rtmp.app.is_empty() {
                bail!("RTMP input app name must not be empty");
            }
        }
    }
    Ok(())
}

/// Validates a single output configuration.
///
/// For **RTP** outputs: checks that the output ID is non-empty, validates `dest_addr`
/// as a socket address, validates the optional `bind_addr`, and checks any FEC encode
/// parameters.
///
/// For **SRT** outputs: checks that the output ID is non-empty, validates `local_addr`
/// as a socket address, checks SRT mode-specific requirements (caller/rendezvous need
/// `remote_addr`), validates passphrase and AES key length, and validates the optional
/// redundancy configuration.
///
/// # Errors
///
/// Returns an error describing the first validation failure encountered.
pub fn validate_output(output: &OutputConfig) -> Result<()> {
    match output {
        OutputConfig::Rtp(rtp) => {
            if rtp.id.is_empty() {
                bail!("RTP output ID cannot be empty");
            }
            validate_socket_addr(&rtp.dest_addr, "RTP output dest_addr")?;
            if let Some(ref bind) = rtp.bind_addr {
                validate_socket_addr(bind, "RTP output bind_addr")?;
            }
            if let Some(ref iface) = rtp.interface_addr {
                validate_ip_addr(iface, "RTP output interface_addr")?;
            }
            if let Some(ref fec) = rtp.fec_encode {
                validate_fec(fec)?;
            }
            // Validate address family consistency
            validate_rtp_output_addr_family(rtp)?;
            // Validate DSCP value
            if rtp.dscp > 63 {
                bail!("RTP output '{}': DSCP must be 0-63, got {}", rtp.id, rtp.dscp);
            }
        }
        OutputConfig::Srt(srt) => {
            if srt.id.is_empty() {
                bail!("SRT output ID cannot be empty");
            }
            validate_socket_addr(&srt.local_addr, "SRT output local_addr")?;
            validate_srt_common(
                &srt.mode,
                &srt.remote_addr,
                srt.passphrase.as_deref(),
                srt.aes_key_len,
                "SRT output",
            )?;
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT output")?;
            }
        }
        OutputConfig::Rtmp(rtmp) => {
            if rtmp.id.is_empty() {
                bail!("RTMP output ID cannot be empty");
            }
            if !rtmp.dest_url.starts_with("rtmp://") && !rtmp.dest_url.starts_with("rtmps://") {
                bail!("RTMP output '{}': dest_url must start with rtmp:// or rtmps://", rtmp.id);
            }
            if rtmp.stream_key.is_empty() {
                bail!("RTMP output '{}': stream_key cannot be empty", rtmp.id);
            }
            if rtmp.reconnect_delay_secs == 0 {
                bail!("RTMP output '{}': reconnect_delay_secs must be > 0", rtmp.id);
            }
        }
        OutputConfig::Hls(hls) => {
            if hls.id.is_empty() {
                bail!("HLS output ID cannot be empty");
            }
            if !hls.ingest_url.starts_with("http://") && !hls.ingest_url.starts_with("https://") {
                bail!("HLS output '{}': ingest_url must start with http:// or https://", hls.id);
            }
            if hls.segment_duration_secs < 0.5 || hls.segment_duration_secs > 10.0 {
                bail!(
                    "HLS output '{}': segment_duration_secs must be 0.5-10.0, got {}",
                    hls.id, hls.segment_duration_secs
                );
            }
            if hls.max_segments == 0 || hls.max_segments > 30 {
                bail!("HLS output '{}': max_segments must be 1-30, got {}", hls.id, hls.max_segments);
            }
        }
        OutputConfig::Webrtc(webrtc) => {
            if webrtc.id.is_empty() {
                bail!("WebRTC output ID cannot be empty");
            }
            if !webrtc.whip_url.starts_with("http://") && !webrtc.whip_url.starts_with("https://") {
                bail!("WebRTC output '{}': whip_url must start with http:// or https://", webrtc.id);
            }
        }
    }
    Ok(())
}

fn validate_srt_common(
    mode: &SrtMode,
    remote_addr: &Option<String>,
    passphrase: Option<&str>,
    aes_key_len: Option<usize>,
    context: &str,
) -> Result<()> {
    match mode {
        SrtMode::Caller | SrtMode::Rendezvous => {
            let addr = remote_addr
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{context}: remote_addr is required for caller/rendezvous mode"))?;
            validate_socket_addr(addr, &format!("{context} remote_addr"))?;
        }
        SrtMode::Listener => {}
    }

    if let Some(pass) = passphrase {
        if pass.len() < 10 || pass.len() > 79 {
            bail!("{context}: passphrase must be 10-79 characters, got {}", pass.len());
        }
    }

    if let Some(key_len) = aes_key_len {
        if key_len != 16 && key_len != 24 && key_len != 32 {
            bail!("{context}: aes_key_len must be 16, 24, or 32, got {key_len}");
        }
    }

    Ok(())
}

fn validate_srt_redundancy(red: &SrtRedundancyConfig, context: &str) -> Result<()> {
    validate_socket_addr(&red.local_addr, &format!("{context} redundancy local_addr"))?;
    validate_srt_common(
        &red.mode,
        &red.remote_addr,
        red.passphrase.as_deref(),
        red.aes_key_len,
        &format!("{context} redundancy"),
    )
}

/// Validates that the RTP input bind address and interface address use the same address family.
fn validate_rtp_input_addr_family(rtp: &RtpInputConfig) -> Result<()> {
    if let Some(ref iface) = rtp.interface_addr {
        let bind: SocketAddr = rtp.bind_addr.parse()?;
        let iface_ip: std::net::IpAddr = iface.parse()?;
        if bind.is_ipv4() != iface_ip.is_ipv4() {
            bail!(
                "RTP input: bind_addr '{}' and interface_addr '{}' must use the same address family (both IPv4 or both IPv6)",
                rtp.bind_addr, iface
            );
        }
    }
    Ok(())
}

/// Validates that the RTP output dest, bind, and interface addresses use consistent address families.
fn validate_rtp_output_addr_family(rtp: &RtpOutputConfig) -> Result<()> {
    let dest: SocketAddr = rtp.dest_addr.parse()?;

    if let Some(ref bind) = rtp.bind_addr {
        let bind_addr: SocketAddr = bind.parse()?;
        if dest.is_ipv4() != bind_addr.is_ipv4() {
            bail!(
                "RTP output '{}': dest_addr '{}' and bind_addr '{}' must use the same address family",
                rtp.id, rtp.dest_addr, bind
            );
        }
    }

    if let Some(ref iface) = rtp.interface_addr {
        let iface_ip: std::net::IpAddr = iface.parse()?;
        if dest.is_ipv4() != iface_ip.is_ipv4() {
            bail!(
                "RTP output '{}': dest_addr '{}' and interface_addr '{}' must use the same address family",
                rtp.id, rtp.dest_addr, iface
            );
        }
    }

    Ok(())
}

/// Validates SMPTE 2022-1 FEC parameters.
///
/// Checks that `columns` (L parameter) is in the range 1..=20 and `rows` (D parameter)
/// is in the range 4..=20.
fn validate_fec(fec: &FecConfig) -> Result<()> {
    if fec.columns < 1 || fec.columns > 20 {
        bail!("FEC columns must be 1-20, got {}", fec.columns);
    }
    if fec.rows < 4 || fec.rows > 20 {
        bail!("FEC rows must be 4-20, got {}", fec.rows);
    }
    Ok(())
}

/// Validates that `addr` is a parseable `ip:port` socket address.
///
/// Uses [`std::net::SocketAddr`] parsing, which accepts both IPv4 and IPv6 formats.
/// The `context` parameter is included in the error message to identify which config
/// field failed validation.
fn validate_socket_addr(addr: &str, context: &str) -> Result<()> {
    addr.parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("{context}: invalid socket address '{addr}': {e}"))?;
    Ok(())
}

/// Validates that `addr` is a parseable IP address (without port).
///
/// Uses [`std::net::IpAddr`] parsing, which accepts both IPv4 and IPv6 formats.
/// The `context` parameter is included in the error message to identify which config
/// field failed validation.
fn validate_ip_addr(addr: &str, context: &str) -> Result<()> {
    addr.parse::<std::net::IpAddr>()
        .map_err(|e| anyhow::anyhow!("{context}: invalid IP address '{addr}': {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_rtp_flow() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
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
                name: "Out 1".to_string(),
                dest_addr: "127.0.0.1:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_ok());
    }

    #[test]
    fn test_invalid_bind_addr() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "not-an-address".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_srt_caller_missing_remote() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            input: InputConfig::Srt(SrtInputConfig {
                mode: SrtMode::Caller,
                local_addr: "0.0.0.0:9000".to_string(),
                remote_addr: None,
                latency_ms: 120,
                peer_idle_timeout_secs: 30,
                passphrase: None,
                aes_key_len: None,
                redundancy: None,
            }),
            outputs: vec![],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_duplicate_flow_ids() {
        let config = AppConfig {
            version: 1,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            tunnels: Vec::new(),
            flows: vec![
                FlowConfig {
                    id: "same-id".to_string(),
                    name: "Flow 1".to_string(),
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
                    outputs: vec![],
                },
                FlowConfig {
                    id: "same-id".to_string(),
                    name: "Flow 2".to_string(),
                    enabled: true,
                    input: InputConfig::Rtp(RtpInputConfig {
                        bind_addr: "0.0.0.0:5001".to_string(),
                        interface_addr: None,
                        fec_decode: None,
                        allowed_sources: None,
                        allowed_payload_types: None,
                        max_bitrate_mbps: None,
                        tr07_mode: None,
                    }),
                    outputs: vec![],
                },
            ],
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_passphrase_length() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            input: InputConfig::Srt(SrtInputConfig {
                mode: SrtMode::Listener,
                local_addr: "0.0.0.0:9000".to_string(),
                remote_addr: None,
                latency_ms: 120,
                peer_idle_timeout_secs: 30,
                passphrase: Some("short".to_string()),
                aes_key_len: None,
                redundancy: None,
            }),
            outputs: vec![],
        };
        assert!(validate_flow(&flow).is_err());
    }

    // --- IPv6 address validation tests ---

    #[test]
    fn test_valid_ipv6_unicast_rtp_flow() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "IPv6 Unicast".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[::]:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Out 1".to_string(),
                dest_addr: "[::1]:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_ok());
    }

    #[test]
    fn test_valid_ipv4_multicast_rtp_flow() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "IPv4 Multicast".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),
                interface_addr: Some("192.168.1.100".to_string()),
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Multicast Out".to_string(),
                dest_addr: "239.1.2.1:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("192.168.1.100".to_string()),
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_ok());
    }

    #[test]
    fn test_valid_ipv6_multicast_rtp_flow() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "IPv6 Multicast".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[ff7e::1]:5000".to_string(),
                interface_addr: Some("::1".to_string()),
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "IPv6 Mcast Out".to_string(),
                dest_addr: "[ff7e::2]:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("::1".to_string()),
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_ok());
    }

    // --- Address family mismatch tests ---

    #[test]
    fn test_rtp_input_mismatched_addr_family() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Mismatched".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),         // IPv4
                interface_addr: Some("::1".to_string()),          // IPv6 - mismatch!
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_rtp_output_mismatched_dest_bind_family() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Mismatched".to_string(),
            enabled: true,
            input: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[::]:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
            }),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Bad".to_string(),
                dest_addr: "[::1]:5004".to_string(),            // IPv6
                bind_addr: Some("0.0.0.0:0".to_string()),      // IPv4 - mismatch!
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_rtp_output_mismatched_dest_iface_family() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Mismatched".to_string(),
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
                name: "Bad".to_string(),
                dest_addr: "239.1.1.1:5004".to_string(),       // IPv4
                bind_addr: None,
                interface_addr: Some("::1".to_string()),        // IPv6 - mismatch!
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_rtp_output_invalid_interface_addr() {
        let flow = FlowConfig {
            id: "f1".to_string(),
            name: "Bad iface".to_string(),
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
                name: "Bad".to_string(),
                dest_addr: "239.1.1.1:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("not-an-ip".to_string()),
                fec_encode: None,
                dscp: 46,
            })],
        };
        assert!(validate_flow(&flow).is_err());
    }
}
