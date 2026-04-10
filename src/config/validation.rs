// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

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

    // Validate device name if present
    if let Some(ref name) = config.device_name {
        if name.len() > 256 {
            bail!("Device name must be at most 256 characters");
        }
    }

    // Validate manager config if present
    if let Some(ref mgr) = config.manager {
        if mgr.enabled {
            if mgr.url.is_empty() {
                bail!("Manager URL cannot be empty when manager is enabled");
            }
            if !mgr.url.starts_with("wss://") {
                bail!("Manager URL must start with wss:// (TLS required)");
            }
            if mgr.url.len() > 2048 {
                bail!("Manager URL must be at most 2048 characters");
            }
            if let Some(ref token) = mgr.registration_token {
                if token.len() > 4096 {
                    bail!("Manager registration token must be at most 4096 characters");
                }
            }
        }
    }

    let mut flow_ids: HashSet<String> = HashSet::new();
    for flow in &config.flows {
        if !flow_ids.insert(flow.id.clone()) {
            bail!("Duplicate flow ID: {}", flow.id);
        }
        validate_flow(flow)?;
    }

    // Validate ST 2110 flow groups (essence bundles).
    // Each group must have a unique ID and only reference defined flows; each
    // member flow's `flow_group_id` (if set) must point at this group.
    let mut seen_groups: HashSet<String> = HashSet::new();
    for group in &config.flow_groups {
        validate_flow_group(group, &flow_ids, &mut seen_groups)?;
    }
    // Cross-check: any flow with `flow_group_id` set must reference an existing
    // group, and the group must list it as a member (no orphan back-references).
    for flow in &config.flows {
        if let Some(ref gid) = flow.flow_group_id {
            let group = config.flow_groups.iter().find(|g| g.id == *gid).ok_or_else(|| {
                anyhow::anyhow!(
                    "Flow '{}' references flow_group_id '{}' which is not defined",
                    flow.id, gid
                )
            })?;
            if !group.flows.iter().any(|f| f == &flow.id) {
                bail!(
                    "Flow '{}' references flow_group_id '{}' but is not a member of that group",
                    flow.id, gid
                );
            }
        }
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
    if flow.id.len() > 64 {
        bail!("Flow ID must be at most 64 characters");
    }
    if flow.name.is_empty() {
        bail!("Flow name cannot be empty");
    }
    if flow.name.len() > 256 {
        bail!("Flow name must be at most 256 characters");
    }

    // Validate thumbnail program selector
    validate_program_number(
        flow.thumbnail_program_number,
        &format!("Flow '{}' thumbnail", flow.id),
    )?;

    // Validate optional flow group membership and clock domain
    if let Some(ref gid) = flow.flow_group_id {
        if gid.is_empty() {
            bail!("Flow '{}': flow_group_id must not be an empty string", flow.id);
        }
        if gid.len() > 64 {
            bail!("Flow '{}': flow_group_id must be at most 64 characters", flow.id);
        }
    }
    validate_clock_domain(flow.clock_domain, &format!("Flow '{}'", flow.id))?;

    // Validate bandwidth limit if configured
    if let Some(ref bw) = flow.bandwidth_limit {
        if bw.max_bitrate_mbps <= 0.0 {
            bail!(
                "Flow '{}': bandwidth_limit max_bitrate_mbps must be positive, got {}",
                flow.id, bw.max_bitrate_mbps
            );
        }
        if bw.max_bitrate_mbps > 10_000.0 {
            bail!(
                "Flow '{}': bandwidth_limit max_bitrate_mbps must be at most 10000 (10 Gbps), got {}",
                flow.id, bw.max_bitrate_mbps
            );
        }
        if bw.grace_period_secs < 1 {
            bail!(
                "Flow '{}': bandwidth_limit grace_period_secs must be at least 1, got {}",
                flow.id, bw.grace_period_secs
            );
        }
        if bw.grace_period_secs > 60 {
            bail!(
                "Flow '{}': bandwidth_limit grace_period_secs must be at most 60, got {}",
                flow.id, bw.grace_period_secs
            );
        }
    }

    if let Some(ref input) = flow.input {
        validate_input(input)?;
    }

    // If the flow's input is itself an uncompressed audio source we can pass
    // its (sample_rate, bit_depth, channels) shape down to per-output
    // transcode validation so that channel-map presets and matrix rules are
    // checked against the *actual* upstream channel count, not the output's
    // own placeholder. For non-audio inputs (RTMP/RTSP/UDP/SRT carrying TS,
    // etc.) the audio_decode bridge runs at runtime and the upstream shape
    // isn't statically known here — None falls back to the output's own
    // declared shape, matching the legacy placeholder behaviour.
    let upstream_audio = flow.input.as_ref().and_then(upstream_audio_shape);

    let mut output_ids = HashSet::new();
    for output in &flow.outputs {
        let oid = output.id();
        if !output_ids.insert(oid.to_string()) {
            bail!("Duplicate output ID '{}' in flow '{}'", oid, flow.id);
        }
        validate_output_with_input(output, upstream_audio)?;
    }

    Ok(())
}

/// If `input` is itself an uncompressed audio source, return its
/// `(sample_rate, bit_depth, channels)` shape so per-output transcode
/// validation can check matrices against the real upstream channel count
/// instead of the output's own placeholder.
fn upstream_audio_shape(input: &InputConfig) -> Option<(u32, u8, u8)> {
    match input {
        InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => {
            Some((c.sample_rate, c.bit_depth, c.channels))
        }
        InputConfig::RtpAudio(c) => Some((c.sample_rate, c.bit_depth, c.channels)),
        _ => None,
    }
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
                if rate > 10_000.0 {
                    bail!("RTP input max_bitrate_mbps must be at most 10000 (10 Gbps), got {rate}");
                }
            }
            // Validate 2022-7 redundancy (leg 2)
            if let Some(ref red) = rtp.redundancy {
                validate_socket_addr(&red.bind_addr, "RTP input redundancy bind_addr")?;
                if let Some(ref iface) = red.interface_addr {
                    validate_ip_addr(iface, "RTP input redundancy interface_addr")?;
                }
                // Validate leg 2 address family consistency
                let leg1: SocketAddr = rtp.bind_addr.parse()?;
                let leg2: SocketAddr = red.bind_addr.parse()?;
                if leg1.is_ipv4() != leg2.is_ipv4() {
                    bail!("RTP input redundancy: leg 1 ({}) and leg 2 ({}) must use the same address family", rtp.bind_addr, red.bind_addr);
                }
                if let Some(ref iface) = red.interface_addr {
                    let iface_ip: std::net::IpAddr = iface.parse()?;
                    if leg2.is_ipv4() != iface_ip.is_ipv4() {
                        bail!("RTP input redundancy: bind_addr and interface_addr must use the same address family");
                    }
                }
            }
        }
        InputConfig::Udp(udp) => {
            validate_socket_addr(&udp.bind_addr, "UDP input bind_addr")?;
            if let Some(ref iface) = udp.interface_addr {
                validate_ip_addr(iface, "UDP input interface_addr")?;
                // Validate address family consistency
                let bind_sa: SocketAddr = udp.bind_addr.parse().unwrap();
                let iface_ip: std::net::IpAddr = iface.parse().map_err(|_| {
                    anyhow::anyhow!("UDP input interface_addr: invalid IP address '{iface}'")
                })?;
                if bind_sa.ip().is_ipv4() != iface_ip.is_ipv4() {
                    bail!(
                        "UDP input: bind_addr '{}' and interface_addr '{}' must use the same address family",
                        udp.bind_addr, iface
                    );
                }
            }
        }
        InputConfig::Srt(srt) => {
            validate_socket_addr(&srt.local_addr, "SRT input local_addr")?;
            validate_srt_common(
                &srt.mode, &srt.remote_addr,
                srt.passphrase.as_deref(), srt.aes_key_len, srt.crypto_mode.as_deref(),
                srt.max_rexmit_bw, srt.stream_id.as_deref(), srt.packet_filter.as_deref(),
                srt.max_bw, srt.overhead_bw, srt.flight_flag_size,
                srt.send_buffer_size, srt.recv_buffer_size, srt.ip_tos,
                srt.retransmit_algo.as_deref(), srt.send_drop_delay, srt.loss_max_ttl,
                srt.km_refresh_rate, srt.km_pre_announce, srt.payload_size,
                srt.mss, srt.ip_ttl,
                "SRT input",
            )?;
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT input")?;
            }
            if let Some(ref tm) = srt.transport_mode {
                if !matches!(tm.as_str(), "ts" | "audio_302m") {
                    bail!(
                        "SRT input: transport_mode must be 'ts' or 'audio_302m', got '{}'",
                        tm
                    );
                }
            }
        }
        InputConfig::Rtmp(rtmp) => {
            validate_socket_addr(&rtmp.listen_addr, "RTMP input listen_addr")?;
            if rtmp.app.is_empty() {
                bail!("RTMP input app name must not be empty");
            }
            if rtmp.app.len() > 64 {
                bail!("RTMP input app name must be at most 64 characters");
            }
            if let Some(ref key) = rtmp.stream_key {
                if key.len() > 256 {
                    bail!("RTMP input stream_key must be at most 256 characters");
                }
            }
        }
        InputConfig::Rtsp(rtsp) => {
            if !rtsp.rtsp_url.starts_with("rtsp://") && !rtsp.rtsp_url.starts_with("rtsps://") {
                bail!("RTSP input: rtsp_url must start with rtsp:// or rtsps://");
            }
            if rtsp.rtsp_url.len() > 2048 {
                bail!("RTSP input: rtsp_url must be at most 2048 characters");
            }
            if let Some(ref user) = rtsp.username {
                if user.len() > 256 {
                    bail!("RTSP input: username must be at most 256 characters");
                }
            }
            if let Some(ref pass) = rtsp.password {
                if pass.len() > 256 {
                    bail!("RTSP input: password must be at most 256 characters");
                }
            }
        }
        InputConfig::Webrtc(webrtc) => {
            if let Some(ref token) = webrtc.bearer_token {
                if token.len() > 4096 {
                    bail!("WebRTC input: bearer_token must be at most 4096 characters");
                }
            }
            if let Some(ref ip) = webrtc.public_ip {
                if ip.parse::<std::net::IpAddr>().is_err() {
                    bail!("WebRTC input: invalid public_ip '{}'", ip);
                }
            }
            if let Some(ref stun) = webrtc.stun_server {
                if stun.len() > 2048 {
                    bail!("WebRTC input: stun_server must be at most 2048 characters");
                }
            }
        }
        InputConfig::Whep(whep) => {
            if !whep.whep_url.starts_with("http://") && !whep.whep_url.starts_with("https://") {
                bail!("WHEP input: whep_url must start with http:// or https://");
            }
            if whep.whep_url.len() > 2048 {
                bail!("WHEP input: whep_url must be at most 2048 characters");
            }
            if let Some(ref token) = whep.bearer_token {
                if token.len() > 4096 {
                    bail!("WHEP input: bearer_token must be at most 4096 characters");
                }
            }
        }
        InputConfig::St2110_30(c) => validate_st2110_audio_input(c, St2110Profile::Pcm)?,
        InputConfig::St2110_31(c) => validate_st2110_audio_input(c, St2110Profile::Aes3)?,
        InputConfig::St2110_40(c) => validate_st2110_ancillary_input(c)?,
        InputConfig::RtpAudio(c) => validate_rtp_audio_input(c)?,
    }
    Ok(())
}

/// SMPTE ST 2110 audio profile selector — controls per-format validation
/// rules (e.g., -31 only permits 24-bit AES3 sub-frames).
#[derive(Debug, Clone, Copy)]
enum St2110Profile {
    /// ST 2110-30 — linear PCM, 16 or 24 bit
    Pcm,
    /// ST 2110-31 — AES3 transparent, always 24 bit
    Aes3,
}

impl St2110Profile {
    fn label(self) -> &'static str {
        match self {
            St2110Profile::Pcm => "ST 2110-30",
            St2110Profile::Aes3 => "ST 2110-31",
        }
    }
}

fn validate_st2110_audio_input(c: &St2110AudioInputConfig, profile: St2110Profile) -> Result<()> {
    let label = profile.label();
    validate_socket_addr(&c.bind_addr, &format!("{label} input bind_addr"))?;
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, &format!("{label} input interface_addr"))?;
    }
    validate_st2110_audio_params(profile, c.sample_rate, c.bit_depth, c.channels, c.packet_time_us, &format!("{label} input"))?;
    validate_rtp_payload_type(c.payload_type, &format!("{label} input"))?;
    validate_clock_domain(c.clock_domain, &format!("{label} input"))?;
    if let Some(ref sources) = c.allowed_sources {
        for s in sources {
            validate_ip_addr(s, &format!("{label} input allowed_sources"))?;
        }
    }
    if let Some(rate) = c.max_bitrate_mbps {
        if rate <= 0.0 {
            bail!("{label} input max_bitrate_mbps must be positive, got {rate}");
        }
        if rate > 10_000.0 {
            bail!("{label} input max_bitrate_mbps must be at most 10000 (10 Gbps), got {rate}");
        }
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.bind_addr, &format!("{label} input redundancy"))?;
    }
    Ok(())
}

fn validate_st2110_audio_output(
    c: &St2110AudioOutputConfig,
    profile: St2110Profile,
    upstream_audio: Option<(u32, u8, u8)>,
) -> Result<()> {
    let label = profile.label();
    validate_id(&c.id, &format!("{label} output"))?;
    validate_name(&c.name, &format!("{label} output"))?;
    validate_socket_addr(&c.dest_addr, &format!("{label} output dest_addr"))?;
    if let Some(ref bind) = c.bind_addr {
        validate_socket_addr(bind, &format!("{label} output bind_addr"))?;
    }
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, &format!("{label} output interface_addr"))?;
    }
    validate_st2110_audio_params(profile, c.sample_rate, c.bit_depth, c.channels, c.packet_time_us, &format!("{label} output '{}'", c.id))?;
    validate_rtp_payload_type(c.payload_type, &format!("{label} output '{}'", c.id))?;
    validate_clock_domain(c.clock_domain, &format!("{label} output '{}'", c.id))?;
    if c.dscp > 63 {
        bail!("{label} output '{}': DSCP must be 0-63, got {}", c.id, c.dscp);
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.dest_addr, &format!("{label} output '{}' redundancy", c.id))?;
    }
    if let Some(ref tj) = c.transcode {
        // Use the parent flow's upstream audio shape when available so
        // channel-map presets are resolved against the *real* upstream
        // channel count. Falls back to the output's own declared shape
        // when the upstream is non-audio (e.g. RTMP/RTSP feeding the
        // audio_decode bridge — runtime resolves the actual format then).
        let (in_sr, in_bd, in_ch) = upstream_audio
            .unwrap_or((c.sample_rate, c.bit_depth, c.channels));
        validate_transcode_block(
            tj,
            in_sr,
            in_bd,
            in_ch,
            &format!("{label} output '{}' transcode", c.id),
        )?;
    }
    Ok(())
}

/// Validate a JSON `transcode` block. Range checks for every field plus the
/// preset/matrix consistency rules. Used by audio output validators.
pub(crate) fn validate_transcode_block(
    tj: &crate::engine::audio_transcode::TranscodeJson,
    in_sample_rate: u32,
    in_bit_depth: u8,
    in_channels: u8,
    context: &str,
) -> Result<()> {
    if let Some(sr) = tj.sample_rate {
        if !matches!(sr, 32_000 | 44_100 | 48_000 | 88_200 | 96_000) {
            bail!(
                "{context}.sample_rate must be one of 32000, 44100, 48000, 88200, 96000, got {sr}"
            );
        }
    }
    if let Some(bd) = tj.bit_depth {
        if !matches!(bd, 16 | 20 | 24) {
            bail!("{context}.bit_depth must be 16, 20, or 24, got {bd}");
        }
    }
    if let Some(ch) = tj.channels {
        if ch == 0 || ch > 16 {
            bail!("{context}.channels must be 1..=16, got {ch}");
        }
    }
    if let Some(pt) = tj.packet_time_us {
        if !matches!(pt, 125 | 250 | 333 | 500 | 1_000 | 4_000) {
            bail!(
                "{context}.packet_time_us must be one of 125, 250, 333, 500, 1000, 4000, got {pt}"
            );
        }
    }
    if let Some(pt) = tj.payload_type {
        if !(96..=127).contains(&pt) {
            bail!("{context}.payload_type must be in dynamic range 96-127, got {pt}");
        }
    }
    if tj.channel_map.is_some() && tj.channel_map_preset.is_some() {
        bail!(
            "{context}: channel_map and channel_map_preset are mutually exclusive"
        );
    }
    // Structural cross-checks via the resolver — this catches matrix-shape /
    // out-of-bounds / unknown-preset errors with one consistent code path.
    let in_bd = match in_bit_depth {
        16 => crate::engine::audio_transcode::BitDepth::L16,
        20 => crate::engine::audio_transcode::BitDepth::L20,
        24 => crate::engine::audio_transcode::BitDepth::L24,
        other => bail!("{context}: input bit_depth {other} not supported"),
    };
    crate::engine::audio_transcode::resolve_transcode(
        tj,
        crate::engine::audio_transcode::InputFormat {
            sample_rate: in_sample_rate,
            bit_depth: in_bd,
            channels: in_channels,
        },
    )
    .map_err(|e| anyhow::anyhow!("{context}: {e}"))?;
    Ok(())
}

fn validate_st2110_ancillary_input(c: &St2110AncillaryInputConfig) -> Result<()> {
    validate_socket_addr(&c.bind_addr, "ST 2110-40 input bind_addr")?;
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, "ST 2110-40 input interface_addr")?;
    }
    validate_rtp_payload_type(c.payload_type, "ST 2110-40 input")?;
    validate_clock_domain(c.clock_domain, "ST 2110-40 input")?;
    if let Some(ref sources) = c.allowed_sources {
        for s in sources {
            validate_ip_addr(s, "ST 2110-40 input allowed_sources")?;
        }
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.bind_addr, "ST 2110-40 input redundancy")?;
    }
    Ok(())
}

fn validate_st2110_ancillary_output(c: &St2110AncillaryOutputConfig) -> Result<()> {
    validate_id(&c.id, "ST 2110-40 output")?;
    validate_name(&c.name, "ST 2110-40 output")?;
    validate_socket_addr(&c.dest_addr, "ST 2110-40 output dest_addr")?;
    if let Some(ref bind) = c.bind_addr {
        validate_socket_addr(bind, "ST 2110-40 output bind_addr")?;
    }
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, "ST 2110-40 output interface_addr")?;
    }
    validate_rtp_payload_type(c.payload_type, &format!("ST 2110-40 output '{}'", c.id))?;
    validate_clock_domain(c.clock_domain, &format!("ST 2110-40 output '{}'", c.id))?;
    if c.dscp > 63 {
        bail!("ST 2110-40 output '{}': DSCP must be 0-63, got {}", c.id, c.dscp);
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.dest_addr, &format!("ST 2110-40 output '{}' redundancy", c.id))?;
    }
    Ok(())
}

/// Validate a generic `rtp_audio` input. Same shape as ST 2110-30 but with
/// a relaxed sample rate set (32k / 44.1k / 48k / 88.2k / 96k) and no
/// PTP / clock_domain requirement.
fn validate_rtp_audio_input(c: &RtpAudioInputConfig) -> Result<()> {
    validate_socket_addr(&c.bind_addr, "rtp_audio input bind_addr")?;
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, "rtp_audio input interface_addr")?;
    }
    validate_rtp_audio_params(
        c.sample_rate,
        c.bit_depth,
        c.channels,
        c.packet_time_us,
        "rtp_audio input",
    )?;
    validate_rtp_payload_type(c.payload_type, "rtp_audio input")?;
    if let Some(ref sources) = c.allowed_sources {
        for s in sources {
            validate_ip_addr(s, "rtp_audio input allowed_sources")?;
        }
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.bind_addr, "rtp_audio input redundancy")?;
    }
    Ok(())
}

fn validate_rtp_audio_output(
    c: &RtpAudioOutputConfig,
    upstream_audio: Option<(u32, u8, u8)>,
) -> Result<()> {
    validate_id(&c.id, "rtp_audio output")?;
    validate_name(&c.name, "rtp_audio output")?;
    validate_socket_addr(&c.dest_addr, &format!("rtp_audio output '{}' dest_addr", c.id))?;
    if let Some(ref bind) = c.bind_addr {
        validate_socket_addr(bind, &format!("rtp_audio output '{}' bind_addr", c.id))?;
    }
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, &format!("rtp_audio output '{}' interface_addr", c.id))?;
    }
    validate_rtp_audio_params(
        c.sample_rate,
        c.bit_depth,
        c.channels,
        c.packet_time_us,
        &format!("rtp_audio output '{}'", c.id),
    )?;
    validate_rtp_payload_type(c.payload_type, &format!("rtp_audio output '{}'", c.id))?;
    if c.dscp > 63 {
        bail!("rtp_audio output '{}': DSCP must be 0-63, got {}", c.id, c.dscp);
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.dest_addr, &format!("rtp_audio output '{}' redundancy", c.id))?;
    }
    if let Some(ref tj) = c.transcode {
        // See st2110_audio_output: prefer the parent flow's upstream shape
        // when known so channel-map presets resolve against the real input.
        let (in_sr, in_bd, in_ch) = upstream_audio
            .unwrap_or((c.sample_rate, c.bit_depth, c.channels));
        validate_transcode_block(
            tj,
            in_sr,
            in_bd,
            in_ch,
            &format!("rtp_audio output '{}' transcode", c.id),
        )?;
    }
    if let Some(ref tm) = c.transport_mode {
        if !matches!(tm.as_str(), "rtp" | "audio_302m") {
            bail!(
                "rtp_audio output '{}': transport_mode must be 'rtp' or 'audio_302m', got '{}'",
                c.id,
                tm
            );
        }
    }
    Ok(())
}

/// Relaxed audio parameter validator for `rtp_audio`. Permits the wider sample
/// rate set used by RFC 3551 (32 k, 44.1 k, 48 k, 88.2 k, 96 k) and the same
/// channel/packet-time/bit-depth set as ST 2110-30.
fn validate_rtp_audio_params(
    sample_rate: u32,
    bit_depth: u8,
    channels: u8,
    packet_time_us: u32,
    context: &str,
) -> Result<()> {
    if !matches!(sample_rate, 32_000 | 44_100 | 48_000 | 88_200 | 96_000) {
        bail!(
            "{context}: sample_rate must be one of 32000, 44100, 48000, 88200, 96000, got {sample_rate}"
        );
    }
    if bit_depth != 16 && bit_depth != 24 {
        bail!("{context}: bit_depth must be 16 or 24, got {bit_depth}");
    }
    if channels == 0 || channels > 16 {
        bail!("{context}: channels must be 1..=16, got {channels}");
    }
    if !matches!(packet_time_us, 125 | 250 | 333 | 500 | 1_000 | 4_000 | 20_000) {
        bail!(
            "{context}: packet_time_us must be one of 125, 250, 333, 500, 1000, 4000, 20000, got {packet_time_us}"
        );
    }
    Ok(())
}

/// Validates the second-leg bind for SMPTE 2022-7 (Red/Blue) operation.
///
/// Ensures the leg-2 address is a valid socket address, the optional interface
/// is a valid IP, the address family matches the primary leg, and (when both
/// are unicast) the IP differs from the primary so the two legs do not collapse
/// onto the same NIC by accident.
fn validate_red_blue_bind(red: &RedBlueBindConfig, primary_addr: &str, context: &str) -> Result<()> {
    validate_socket_addr(&red.addr, &format!("{context} addr"))?;
    if let Some(ref iface) = red.interface_addr {
        validate_ip_addr(iface, &format!("{context} interface_addr"))?;
    }
    let leg1: SocketAddr = primary_addr.parse()
        .map_err(|e| anyhow::anyhow!("{context}: primary addr '{primary_addr}' is not parseable: {e}"))?;
    let leg2: SocketAddr = red.addr.parse()
        .map_err(|e| anyhow::anyhow!("{context}: leg 2 addr '{}' is not parseable: {e}", red.addr))?;
    if leg1.is_ipv4() != leg2.is_ipv4() {
        bail!(
            "{context}: leg 1 ({primary_addr}) and leg 2 ({}) must use the same address family",
            red.addr
        );
    }
    if let Some(ref iface) = red.interface_addr {
        let iface_ip: std::net::IpAddr = iface.parse()
            .map_err(|_| anyhow::anyhow!("{context} interface_addr: invalid IP '{iface}'"))?;
        if leg2.is_ipv4() != iface_ip.is_ipv4() {
            bail!("{context}: leg 2 addr and interface_addr must use the same address family");
        }
    }
    // Both legs unicast and bound to the same IP defeats the purpose of 2022-7.
    let leg1_ip = leg1.ip();
    let leg2_ip = leg2.ip();
    if !leg1_ip.is_unspecified() && !leg2_ip.is_unspecified() && !leg1_ip.is_multicast()
        && !leg2_ip.is_multicast() && leg1_ip == leg2_ip
    {
        bail!(
            "{context}: leg 1 and leg 2 must use distinct IPs for SMPTE 2022-7 \
             (got {leg1_ip} for both)"
        );
    }
    Ok(())
}

fn validate_st2110_audio_params(
    profile: St2110Profile,
    sample_rate: u32,
    bit_depth: u8,
    channels: u8,
    packet_time_us: u32,
    context: &str,
) -> Result<()> {
    // ST 2110-30 supports 48 and 96 kHz; ST 2110-31 only 48 kHz in practice but
    // the wire format permits 96 kHz so accept both.
    if sample_rate != 48_000 && sample_rate != 96_000 {
        bail!("{context}: sample_rate must be 48000 or 96000, got {sample_rate}");
    }
    match profile {
        St2110Profile::Pcm => {
            if bit_depth != 16 && bit_depth != 24 {
                bail!("{context}: ST 2110-30 bit_depth must be 16 or 24, got {bit_depth}");
            }
        }
        St2110Profile::Aes3 => {
            if bit_depth != 24 {
                bail!("{context}: ST 2110-31 bit_depth must be 24 (AES3 sub-frame), got {bit_depth}");
            }
        }
    }
    if !matches!(channels, 1 | 2 | 4 | 8 | 16) {
        bail!("{context}: channels must be 1, 2, 4, 8, or 16, got {channels}");
    }
    // ST 2110-30 PM (Standard) and AM (high frame) profile packet times.
    if !matches!(packet_time_us, 125 | 250 | 333 | 500 | 1_000 | 4_000) {
        bail!(
            "{context}: packet_time_us must be 125, 250, 333, 500, 1000, or 4000, got {packet_time_us}"
        );
    }
    Ok(())
}

fn validate_rtp_payload_type(pt: u8, context: &str) -> Result<()> {
    if !(96..=127).contains(&pt) {
        bail!("{context}: payload_type must be in dynamic range 96-127, got {pt}");
    }
    Ok(())
}

fn validate_clock_domain(domain: Option<u8>, context: &str) -> Result<()> {
    if let Some(d) = domain {
        if d > 127 {
            bail!("{context}: clock_domain must be 0-127 (IEEE 1588), got {d}");
        }
    }
    Ok(())
}

/// Validates a single SMPTE ST 2110 flow group definition.
///
/// Checks ID/name length, clock domain, that there is at least one member, and
/// that every member references a real flow ID.
fn validate_flow_group(
    g: &FlowGroupConfig,
    flow_ids: &HashSet<String>,
    seen_groups: &mut HashSet<String>,
) -> Result<()> {
    validate_id(&g.id, "Flow group")?;
    validate_name(&g.name, "Flow group")?;
    validate_clock_domain(g.clock_domain, &format!("Flow group '{}'", g.id))?;
    if !seen_groups.insert(g.id.clone()) {
        bail!("Duplicate flow group ID: {}", g.id);
    }
    if g.flows.is_empty() {
        bail!("Flow group '{}' must reference at least one flow", g.id);
    }
    let mut seen_members = HashSet::new();
    for fid in &g.flows {
        if fid.len() > 64 {
            bail!("Flow group '{}': member flow ID '{}' is too long (max 64)", g.id, fid);
        }
        if !seen_members.insert(fid.clone()) {
            bail!("Flow group '{}': duplicate member flow ID '{}'", g.id, fid);
        }
        if !flow_ids.contains(fid) {
            bail!(
                "Flow group '{}': member flow ID '{}' does not reference a defined flow",
                g.id, fid
            );
        }
    }
    Ok(())
}

fn validate_id(id: &str, context: &str) -> Result<()> {
    if id.is_empty() {
        bail!("{context} ID cannot be empty");
    }
    if id.len() > 64 {
        bail!("{context} ID must be at most 64 characters");
    }
    Ok(())
}

fn validate_name(name: &str, context: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{context} name cannot be empty");
    }
    if name.len() > 256 {
        bail!("{context} name must be at most 256 characters");
    }
    Ok(())
}

/// Validate an optional MPEG-TS program_number selector. program_number 0 is
/// reserved for the NIT and never identifies a real program.
fn validate_program_number(prog: Option<u16>, context: &str) -> Result<()> {
    if let Some(n) = prog {
        if n == 0 {
            bail!("{context}: program_number must be > 0 (0 is reserved for the NIT)");
        }
    }
    Ok(())
}

/// Validate an `audio_encode` block. Each output type passes its allowed
/// codec set so we can reject invalid combinations (e.g. Opus on RTMP)
/// at config load time rather than at output start time.
fn validate_audio_encode(
    enc: &crate::config::models::AudioEncodeConfig,
    allowed_codecs: &[&str],
    context: &str,
) -> Result<()> {
    if !allowed_codecs.contains(&enc.codec.as_str()) {
        bail!(
            "{context}: audio_encode.codec '{}' is not allowed for this output; allowed: {:?}",
            enc.codec,
            allowed_codecs
        );
    }
    if let Some(br) = enc.bitrate_kbps {
        if !(16..=512).contains(&br) {
            bail!(
                "{context}: audio_encode.bitrate_kbps must be 16..=512, got {}",
                br
            );
        }
    }
    if let Some(sr) = enc.sample_rate {
        const ALLOWED_SR: &[u32] = &[8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000];
        if !ALLOWED_SR.contains(&sr) {
            bail!(
                "{context}: audio_encode.sample_rate must be one of {:?}, got {}",
                ALLOWED_SR,
                sr
            );
        }
    }
    if let Some(ch) = enc.channels {
        // fdk-aac supports up to 8 channels (7.1) for AAC-LC. HE-AAC v1/v2
        // and Opus are limited to 1-2 channels. MP2/AC-3 support up to 6.
        let max_ch: u8 = match enc.codec.as_str() {
            "aac_lc" => 8,
            "ac3" => 6,
            _ => 2, // he_aac_v1, he_aac_v2, opus, mp2
        };
        if ch == 0 || ch > max_ch {
            bail!(
                "{context}: audio_encode.channels must be 1-{max_ch} for {}, got {}",
                enc.codec, ch
            );
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
    validate_output_with_input(output, None)
}

/// Like [`validate_output`] but additionally takes the parent flow's
/// upstream audio shape (sample_rate, bit_depth, channels) when the input is
/// itself an uncompressed audio source. Used by `validate_flow` so transcode
/// channel-map presets are checked against the actual upstream channel count
/// rather than the output's own declared channels.
pub fn validate_output_with_input(
    output: &OutputConfig,
    upstream_audio: Option<(u32, u8, u8)>,
) -> Result<()> {
    match output {
        OutputConfig::Rtp(rtp) => {
            validate_id(&rtp.id, "RTP output")?;
            validate_name(&rtp.name, "RTP output")?;
            validate_program_number(rtp.program_number, &format!("RTP output '{}'", rtp.id))?;
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
            // Validate 2022-7 redundancy (leg 2)
            if let Some(ref red) = rtp.redundancy {
                validate_socket_addr(&red.dest_addr, "RTP output redundancy dest_addr")?;
                if let Some(ref bind) = red.bind_addr {
                    validate_socket_addr(bind, "RTP output redundancy bind_addr")?;
                }
                if let Some(ref iface) = red.interface_addr {
                    validate_ip_addr(iface, "RTP output redundancy interface_addr")?;
                }
                if let Some(dscp) = red.dscp {
                    if dscp > 63 {
                        bail!("RTP output '{}' redundancy: DSCP must be 0-63, got {}", rtp.id, dscp);
                    }
                }
                // Validate address family consistency between legs
                let leg1: SocketAddr = rtp.dest_addr.parse()?;
                let leg2: SocketAddr = red.dest_addr.parse()?;
                if leg1.is_ipv4() != leg2.is_ipv4() {
                    bail!("RTP output '{}' redundancy: leg 1 ({}) and leg 2 ({}) must use the same address family", rtp.id, rtp.dest_addr, red.dest_addr);
                }
            }
        }
        OutputConfig::Udp(udp) => {
            validate_id(&udp.id, "UDP output")?;
            validate_name(&udp.name, "UDP output")?;
            validate_program_number(udp.program_number, &format!("UDP output '{}'", udp.id))?;
            validate_socket_addr(&udp.dest_addr, "UDP output dest_addr")?;
            if let Some(ref bind) = udp.bind_addr {
                validate_socket_addr(bind, "UDP output bind_addr")?;
            }
            if let Some(ref iface) = udp.interface_addr {
                validate_ip_addr(iface, "UDP output interface_addr")?;
            }
            if udp.dscp > 63 {
                bail!("UDP output '{}': DSCP must be 0-63, got {}", udp.id, udp.dscp);
            }
            if let Some(ref tm) = udp.transport_mode {
                if !matches!(tm.as_str(), "ts" | "audio_302m") {
                    bail!(
                        "UDP output '{}': transport_mode must be 'ts' or 'audio_302m', got '{}'",
                        udp.id,
                        tm
                    );
                }
                if tm == "audio_302m" && udp.program_number.is_some() {
                    bail!(
                        "UDP output '{}': transport_mode 'audio_302m' is incompatible with program_number",
                        udp.id
                    );
                }
            }
        }
        OutputConfig::Srt(srt) => {
            validate_id(&srt.id, "SRT output")?;
            validate_name(&srt.name, "SRT output")?;
            validate_program_number(srt.program_number, &format!("SRT output '{}'", srt.id))?;
            validate_socket_addr(&srt.local_addr, "SRT output local_addr")?;
            validate_srt_common(
                &srt.mode, &srt.remote_addr,
                srt.passphrase.as_deref(), srt.aes_key_len, srt.crypto_mode.as_deref(),
                srt.max_rexmit_bw, srt.stream_id.as_deref(), srt.packet_filter.as_deref(),
                srt.max_bw, srt.overhead_bw, srt.flight_flag_size,
                srt.send_buffer_size, srt.recv_buffer_size, srt.ip_tos,
                srt.retransmit_algo.as_deref(), srt.send_drop_delay, srt.loss_max_ttl,
                srt.km_refresh_rate, srt.km_pre_announce, srt.payload_size,
                srt.mss, srt.ip_ttl,
                "SRT output",
            )?;
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT output")?;
            }
            if let Some(ref tm) = srt.transport_mode {
                if !matches!(tm.as_str(), "ts" | "audio_302m") {
                    bail!(
                        "SRT output '{}': transport_mode must be 'ts' or 'audio_302m', got '{}'",
                        srt.id,
                        tm
                    );
                }
                if tm == "audio_302m" {
                    if srt.packet_filter.is_some() {
                        bail!(
                            "SRT output '{}': transport_mode 'audio_302m' is incompatible with packet_filter (FEC)",
                            srt.id
                        );
                    }
                    if srt.program_number.is_some() {
                        bail!(
                            "SRT output '{}': transport_mode 'audio_302m' is incompatible with program_number",
                            srt.id
                        );
                    }
                    if srt.redundancy.is_some() {
                        bail!(
                            "SRT output '{}': transport_mode 'audio_302m' is incompatible with SMPTE 2022-7 redundancy",
                            srt.id
                        );
                    }
                }
            }
        }
        OutputConfig::Rtmp(rtmp) => {
            validate_id(&rtmp.id, "RTMP output")?;
            validate_name(&rtmp.name, "RTMP output")?;
            validate_program_number(rtmp.program_number, &format!("RTMP output '{}'", rtmp.id))?;
            if !rtmp.dest_url.starts_with("rtmp://") && !rtmp.dest_url.starts_with("rtmps://") {
                bail!("RTMP output '{}': dest_url must start with rtmp:// or rtmps://", rtmp.id);
            }
            if rtmp.dest_url.len() > 2048 {
                bail!("RTMP output '{}': dest_url must be at most 2048 characters", rtmp.id);
            }
            if rtmp.stream_key.is_empty() {
                bail!("RTMP output '{}': stream_key cannot be empty", rtmp.id);
            }
            if rtmp.stream_key.len() > 256 {
                bail!("RTMP output '{}': stream_key must be at most 256 characters", rtmp.id);
            }
            if rtmp.reconnect_delay_secs == 0 {
                bail!("RTMP output '{}': reconnect_delay_secs must be > 0", rtmp.id);
            }
            if let Some(ref enc) = rtmp.audio_encode {
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2"],
                    &format!("RTMP output '{}'", rtmp.id),
                )?;
            }
        }
        OutputConfig::Hls(hls) => {
            validate_id(&hls.id, "HLS output")?;
            validate_name(&hls.name, "HLS output")?;
            validate_program_number(hls.program_number, &format!("HLS output '{}'", hls.id))?;
            if !hls.ingest_url.starts_with("http://") && !hls.ingest_url.starts_with("https://") {
                bail!("HLS output '{}': ingest_url must start with http:// or https://", hls.id);
            }
            if hls.ingest_url.len() > 2048 {
                bail!("HLS output '{}': ingest_url must be at most 2048 characters", hls.id);
            }
            if let Some(ref token) = hls.auth_token {
                if token.len() > 4096 {
                    bail!("HLS output '{}': auth_token must be at most 4096 characters", hls.id);
                }
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
            if let Some(ref enc) = hls.audio_encode {
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"],
                    &format!("HLS output '{}'", hls.id),
                )?;
            }
        }
        OutputConfig::Webrtc(webrtc) => {
            validate_id(&webrtc.id, "WebRTC output")?;
            validate_name(&webrtc.name, "WebRTC output")?;
            validate_program_number(webrtc.program_number, &format!("WebRTC output '{}'", webrtc.id))?;
            match webrtc.mode {
                crate::config::models::WebrtcOutputMode::WhipClient => {
                    let url = webrtc.whip_url.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("WebRTC output '{}': whip_url is required for whip_client mode", webrtc.id)
                    })?;
                    if !url.starts_with("http://") && !url.starts_with("https://") {
                        bail!("WebRTC output '{}': whip_url must start with http:// or https://", webrtc.id);
                    }
                    if url.len() > 2048 {
                        bail!("WebRTC output '{}': whip_url must be at most 2048 characters", webrtc.id);
                    }
                }
                crate::config::models::WebrtcOutputMode::WhepServer => {
                    if let Some(max) = webrtc.max_viewers {
                        if max == 0 || max > 100 {
                            bail!("WebRTC output '{}': max_viewers must be 1-100, got {}", webrtc.id, max);
                        }
                    }
                }
            }
            if let Some(ref token) = webrtc.bearer_token {
                if token.len() > 4096 {
                    bail!("WebRTC output '{}': bearer_token must be at most 4096 characters", webrtc.id);
                }
            }
            if let Some(ref ip) = webrtc.public_ip {
                if ip.parse::<std::net::IpAddr>().is_err() {
                    bail!("WebRTC output '{}': invalid public_ip '{}'", webrtc.id, ip);
                }
            }
            if let Some(ref enc) = webrtc.audio_encode {
                if webrtc.video_only {
                    bail!(
                        "WebRTC output '{}': audio_encode requires video_only=false (audio MID must be negotiated in SDP)",
                        webrtc.id
                    );
                }
                validate_audio_encode(
                    enc,
                    &["opus"],
                    &format!("WebRTC output '{}'", webrtc.id),
                )?;
            }
        }
        OutputConfig::St2110_30(c) => {
            validate_st2110_audio_output(c, St2110Profile::Pcm, upstream_audio)?
        }
        OutputConfig::St2110_31(c) => {
            validate_st2110_audio_output(c, St2110Profile::Aes3, upstream_audio)?
        }
        OutputConfig::St2110_40(c) => validate_st2110_ancillary_output(c)?,
        OutputConfig::RtpAudio(c) => validate_rtp_audio_output(c, upstream_audio)?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_srt_common(
    mode: &SrtMode,
    remote_addr: &Option<String>,
    passphrase: Option<&str>,
    aes_key_len: Option<usize>,
    crypto_mode: Option<&str>,
    max_rexmit_bw: Option<i64>,
    stream_id: Option<&str>,
    packet_filter: Option<&str>,
    max_bw: Option<i64>,
    overhead_bw: Option<i32>,
    flight_flag_size: Option<u32>,
    send_buffer_size: Option<u32>,
    recv_buffer_size: Option<u32>,
    ip_tos: Option<i32>,
    retransmit_algo: Option<&str>,
    send_drop_delay: Option<i32>,
    loss_max_ttl: Option<i32>,
    km_refresh_rate: Option<u32>,
    km_pre_announce: Option<u32>,
    payload_size: Option<u32>,
    mss: Option<u32>,
    ip_ttl: Option<i32>,
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

    if let Some(cm) = crypto_mode {
        if cm != "aes-ctr" && cm != "aes-gcm" {
            bail!("{context}: crypto_mode must be \"aes-ctr\" or \"aes-gcm\", got \"{cm}\"");
        }
        if cm == "aes-gcm" {
            if let Some(24) = aes_key_len {
                bail!("{context}: AES-GCM does not support AES-192 (aes_key_len 24); use 16 or 32");
            }
        }
    }

    if let Some(bw) = max_rexmit_bw {
        if bw < -1 {
            bail!("{context}: max_rexmit_bw must be -1 (unlimited), 0 (disable), or > 0 (bytes/sec), got {bw}");
        }
    }

    if let Some(sid) = stream_id {
        if sid.len() > 512 {
            bail!("{context}: stream_id must be at most 512 characters (per SRT spec), got {}", sid.len());
        }
    }

    if let Some(pf) = packet_filter {
        if pf.len() > 512 {
            bail!("{context}: packet_filter must be at most 512 characters, got {}", pf.len());
        }
        match srt_protocol::fec::FecConfig::parse(pf) {
            Ok(fec_cfg) => {
                if fec_cfg.cols == 0 || fec_cfg.cols > 256 {
                    bail!("{context}: FEC cols must be 1-256, got {}", fec_cfg.cols);
                }
                if fec_cfg.rows == 0 || fec_cfg.rows > 256 {
                    bail!("{context}: FEC rows must be 1-256, got {}", fec_cfg.rows);
                }
            }
            Err(e) => {
                bail!("{context}: invalid packet_filter: {e}");
            }
        }
    }

    if let Some(bw) = max_bw {
        if bw < 0 {
            bail!("{context}: max_bw must be >= 0 (0 = unlimited), got {bw}");
        }
    }

    if let Some(pct) = overhead_bw {
        if pct < 5 || pct > 100 {
            bail!("{context}: overhead_bw must be 5-100 (percentage), got {pct}");
        }
    }

    if let Some(ffs) = flight_flag_size {
        if ffs < 32 {
            bail!("{context}: flight_flag_size must be >= 32 packets, got {ffs}");
        }
    }

    if let Some(s) = send_buffer_size {
        if s < 32 {
            bail!("{context}: send_buffer_size must be >= 32 packets, got {s}");
        }
    }

    if let Some(s) = recv_buffer_size {
        if s < 32 {
            bail!("{context}: recv_buffer_size must be >= 32 packets, got {s}");
        }
    }

    if let Some(t) = ip_tos {
        if !(0..=255).contains(&t) {
            bail!("{context}: ip_tos must be 0-255, got {t}");
        }
    }

    if let Some(algo) = retransmit_algo {
        if algo != "default" && algo != "reduced" {
            bail!("{context}: retransmit_algo must be \"default\" or \"reduced\", got \"{algo}\"");
        }
    }

    if let Some(d) = send_drop_delay {
        if d < -1 {
            bail!("{context}: send_drop_delay must be >= -1, got {d}");
        }
    }

    if let Some(t) = loss_max_ttl {
        if t < 0 {
            bail!("{context}: loss_max_ttl must be >= 0, got {t}");
        }
    }

    if let Some(r) = km_refresh_rate {
        if r == 0 {
            bail!("{context}: km_refresh_rate must be > 0");
        }
    }

    if let Some(r) = km_pre_announce {
        if r == 0 {
            bail!("{context}: km_pre_announce must be > 0");
        }
    }

    if let Some(s) = payload_size {
        if s < 188 || s > 1456 {
            bail!("{context}: payload_size must be 188-1456, got {s}");
        }
    }

    if let Some(s) = mss {
        if s < 76 || s > 9000 {
            bail!("{context}: mss must be 76-9000, got {s}");
        }
    }

    if let Some(t) = ip_ttl {
        if !(1..=255).contains(&t) {
            bail!("{context}: ip_ttl must be 1-255, got {t}");
        }
    }

    Ok(())
}

fn validate_srt_redundancy(red: &SrtRedundancyConfig, context: &str) -> Result<()> {
    validate_socket_addr(&red.local_addr, &format!("{context} redundancy local_addr"))?;
    validate_srt_common(
        &red.mode, &red.remote_addr,
        red.passphrase.as_deref(), red.aes_key_len, red.crypto_mode.as_deref(),
        red.max_rexmit_bw, red.stream_id.as_deref(), red.packet_filter.as_deref(),
        red.max_bw, red.overhead_bw, red.flight_flag_size,
        red.send_buffer_size, red.recv_buffer_size, red.ip_tos,
        red.retransmit_algo.as_deref(), red.send_drop_delay, red.loss_max_ttl,
        red.km_refresh_rate, red.km_pre_announce, red.payload_size,
        red.mss, red.ip_ttl,
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

/// Validates a tunnel configuration.
pub fn validate_tunnel(tunnel: &crate::tunnel::TunnelConfig) -> Result<()> {
    validate_id(&tunnel.id, "Tunnel")?;
    // Tunnel IDs must be valid UUIDs to prevent predictable v5 derivation
    if uuid::Uuid::parse_str(&tunnel.id).is_err() {
        bail!(
            "Tunnel '{}': id must be a valid UUID (e.g., '550e8400-e29b-41d4-a716-446655440000')",
            tunnel.id
        );
    }
    validate_name(&tunnel.name, "Tunnel")?;

    // Validate local address
    validate_socket_addr(&tunnel.local_addr, "Tunnel local_addr")?;

    // Mode-specific validation
    match tunnel.mode {
        crate::tunnel::config::TunnelMode::Relay => {
            let relay_addr = tunnel.relay_addr.as_deref()
                .ok_or_else(|| anyhow::anyhow!("Tunnel '{}': relay_addr is required for relay mode", tunnel.id))?;
            if relay_addr.is_empty() || relay_addr.len() > 256 {
                bail!("Tunnel '{}': relay_addr must be 1-256 characters", tunnel.id);
            }
        }
        crate::tunnel::config::TunnelMode::Direct => {
            match tunnel.direction {
                crate::tunnel::config::TunnelDirection::Egress => {
                    if tunnel.peer_addr.is_none() {
                        bail!("Tunnel '{}': peer_addr is required for direct egress", tunnel.id);
                    }
                }
                crate::tunnel::config::TunnelDirection::Ingress => {
                    if tunnel.direct_listen_addr.is_none() {
                        bail!("Tunnel '{}': direct_listen_addr is required for direct ingress", tunnel.id);
                    }
                }
            }
        }
    }

    // Validate tunnel encryption key
    if let Some(ref key) = tunnel.tunnel_encryption_key {
        if key.len() != 64 {
            bail!(
                "Tunnel '{}': tunnel_encryption_key must be exactly 64 hex characters (32 bytes), got {} chars",
                tunnel.id, key.len()
            );
        }
        if !key.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("Tunnel '{}': tunnel_encryption_key must contain only hex characters", tunnel.id);
        }
    } else if tunnel.mode == crate::tunnel::config::TunnelMode::Relay {
        bail!(
            "Tunnel '{}': tunnel_encryption_key is required for relay mode (end-to-end encryption)",
            tunnel.id
        );
    }

    // Validate tunnel bind secret (relay authentication)
    if let Some(ref key) = tunnel.tunnel_bind_secret {
        if key.len() != 64 {
            bail!(
                "Tunnel '{}': tunnel_bind_secret must be exactly 64 hex characters (32 bytes), got {} chars",
                tunnel.id, key.len()
            );
        }
        if !key.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("Tunnel '{}': tunnel_bind_secret must contain only hex characters", tunnel.id);
        }
    }

    // Validate optional string field lengths
    if let Some(ref s) = tunnel.tunnel_psk {
        if s.len() > 256 {
            bail!("Tunnel '{}': tunnel_psk must be at most 256 characters", tunnel.id);
        }
    }
    if let Some(ref addr) = tunnel.peer_addr {
        if addr.len() > 256 {
            bail!("Tunnel '{}': peer_addr must be at most 256 characters", tunnel.id);
        }
    }
    if let Some(ref addr) = tunnel.direct_listen_addr {
        validate_socket_addr(addr, &format!("Tunnel '{}' direct_listen_addr", tunnel.id))?;
    }

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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Out 1".to_string(),
                dest_addr: "127.0.0.1:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "not-an-address".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Srt(SrtInputConfig {
                mode: SrtMode::Caller,
                local_addr: "0.0.0.0:9000".to_string(),
                remote_addr: None,
                latency_ms: 120,
                peer_idle_timeout_secs: 30,
                recv_latency_ms: None,
                peer_latency_ms: None,
                passphrase: None,
                aes_key_len: None,
                crypto_mode: None,
                max_rexmit_bw: None,
                stream_id: None,
                packet_filter: None,
                max_bw: None, input_bw: None, overhead_bw: None,
                enforced_encryption: None, connect_timeout_secs: None,
                flight_flag_size: None, send_buffer_size: None, recv_buffer_size: None,
                ip_tos: None, retransmit_algo: None, send_drop_delay: None,
                loss_max_ttl: None, km_refresh_rate: None, km_pre_announce: None,
                payload_size: None, mss: None, tlpkt_drop: None, ip_ttl: None,
                redundancy: None,
                transport_mode: None,
            })),
            outputs: vec![],
        };
        assert!(validate_flow(&flow).is_err());
    }

    #[test]
    fn test_duplicate_flow_ids() {
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
            resource_limits: None,
            flows: vec![
                FlowConfig {
                    id: "same-id".to_string(),
                    name: "Flow 1".to_string(),
                    enabled: true,
                    media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
                    bandwidth_limit: None,
                    flow_group_id: None,
                    clock_domain: None,
                    input: Some(InputConfig::Rtp(RtpInputConfig {
                        bind_addr: "0.0.0.0:5000".to_string(),
                        interface_addr: None,
                        fec_decode: None,
                        allowed_sources: None,
                        allowed_payload_types: None,
                        max_bitrate_mbps: None,
                        tr07_mode: None,
                        redundancy: None,
                    })),
                    outputs: vec![],
                },
                FlowConfig {
                    id: "same-id".to_string(),
                    name: "Flow 2".to_string(),
                    enabled: true,
                    media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
                    bandwidth_limit: None,
                    flow_group_id: None,
                    clock_domain: None,
                    input: Some(InputConfig::Rtp(RtpInputConfig {
                        bind_addr: "0.0.0.0:5001".to_string(),
                        interface_addr: None,
                        fec_decode: None,
                        allowed_sources: None,
                        allowed_payload_types: None,
                        max_bitrate_mbps: None,
                        tr07_mode: None,
                        redundancy: None,
                    })),
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Srt(SrtInputConfig {
                mode: SrtMode::Listener,
                local_addr: "0.0.0.0:9000".to_string(),
                remote_addr: None,
                latency_ms: 120,
                peer_idle_timeout_secs: 30,
                recv_latency_ms: None,
                peer_latency_ms: None,
                passphrase: Some("short".to_string()),
                aes_key_len: None,
                crypto_mode: None,
                max_rexmit_bw: None,
                stream_id: None,
                packet_filter: None,
                max_bw: None, input_bw: None, overhead_bw: None,
                enforced_encryption: None, connect_timeout_secs: None,
                flight_flag_size: None, send_buffer_size: None, recv_buffer_size: None,
                ip_tos: None, retransmit_algo: None, send_drop_delay: None,
                loss_max_ttl: None, km_refresh_rate: None, km_pre_announce: None,
                payload_size: None, mss: None, tlpkt_drop: None, ip_ttl: None,
                redundancy: None,
                transport_mode: None,
            })),
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[::]:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Out 1".to_string(),
                dest_addr: "[::1]:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),
                interface_addr: Some("192.168.1.100".to_string()),
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Multicast Out".to_string(),
                dest_addr: "239.1.2.1:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("192.168.1.100".to_string()),
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[ff7e::1]:5000".to_string(),
                interface_addr: Some("::1".to_string()),
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "IPv6 Mcast Out".to_string(),
                dest_addr: "[ff7e::2]:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("::1".to_string()),
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),         // IPv4
                interface_addr: Some("::1".to_string()),          // IPv6 - mismatch!
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[::]:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Bad".to_string(),
                dest_addr: "[::1]:5004".to_string(),            // IPv6
                bind_addr: Some("0.0.0.0:0".to_string()),      // IPv4 - mismatch!
                interface_addr: None,
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Bad".to_string(),
                dest_addr: "239.1.1.1:5004".to_string(),       // IPv4
                bind_addr: None,
                interface_addr: Some("::1".to_string()),        // IPv6 - mismatch!
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
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
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
            })),
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                id: "out-1".to_string(),
                name: "Bad".to_string(),
                dest_addr: "239.1.1.1:5004".to_string(),
                bind_addr: None,
                interface_addr: Some("not-an-ip".to_string()),
                fec_encode: None,
                dscp: 46,
                redundancy: None,
                program_number: None,
            delay: None,
            })],
        };
        assert!(validate_flow(&flow).is_err());
    }

    // ─────────────── SMPTE ST 2110 validation tests ───────────────

    fn st2110_30_input(addr: &str) -> St2110AudioInputConfig {
        St2110AudioInputConfig {
            bind_addr: addr.to_string(),
            interface_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: Some(0),
            allowed_sources: None,
            max_bitrate_mbps: None,
        }
    }

    fn st2110_30_output(id: &str, dest: &str) -> St2110AudioOutputConfig {
        St2110AudioOutputConfig {
            id: id.to_string(),
            name: "Audio out".to_string(),
            dest_addr: dest.to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: Some(0),
            dscp: 46,
            ssrc: None,
            transcode: None,
        }
    }

    #[test]
    fn test_st2110_30_valid_audio_input() {
        let cfg = st2110_30_input("239.10.10.1:5004");
        validate_st2110_audio_input(&cfg, St2110Profile::Pcm).expect("valid -30 input");
    }

    #[test]
    fn test_st2110_30_invalid_sample_rate() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.sample_rate = 44_100;
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_30_invalid_bit_depth() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.bit_depth = 32;
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_31_requires_24_bit() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.bit_depth = 16; // valid for -30 but rejected for -31
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Aes3).is_err());
    }

    #[test]
    fn test_st2110_invalid_channel_count() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.channels = 3;
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_invalid_packet_time() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.packet_time_us = 750;
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_payload_type_out_of_range() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.payload_type = 50;
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_clock_domain_out_of_range() {
        let mut cfg = st2110_30_input("239.10.10.1:5004");
        cfg.clock_domain = Some(200);
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_red_blue_legs_collision_rejected() {
        let mut cfg = st2110_30_input("10.0.0.5:5004");
        cfg.redundancy = Some(RedBlueBindConfig {
            addr: "10.0.0.5:5006".to_string(),
            interface_addr: None,
        });
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_red_blue_address_family_mismatch_rejected() {
        let mut cfg = st2110_30_input("10.0.0.5:5004");
        cfg.redundancy = Some(RedBlueBindConfig {
            addr: "[::1]:5006".to_string(),
            interface_addr: None,
        });
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_red_blue_distinct_unicast_ok() {
        let mut cfg = st2110_30_input("10.0.0.5:5004");
        cfg.redundancy = Some(RedBlueBindConfig {
            addr: "10.0.1.5:5006".to_string(),
            interface_addr: None,
        });
        validate_st2110_audio_input(&cfg, St2110Profile::Pcm).expect("distinct legs");
    }

    #[test]
    fn test_st2110_30_output_valid() {
        let out = st2110_30_output("audio-1", "239.10.10.1:5004");
        validate_st2110_audio_output(&out, St2110Profile::Pcm, None).expect("valid -30 output");
    }

    #[test]
    fn test_st2110_30_output_dscp_out_of_range() {
        let mut out = st2110_30_output("audio-1", "239.10.10.1:5004");
        out.dscp = 64;
        assert!(validate_st2110_audio_output(&out, St2110Profile::Pcm, None).is_err());
    }

    #[test]
    fn test_st2110_40_input_valid() {
        let anc = St2110AncillaryInputConfig {
            bind_addr: "239.10.10.10:5006".to_string(),
            interface_addr: None,
            redundancy: None,
            payload_type: 100,
            clock_domain: Some(0),
            allowed_sources: None,
        };
        validate_st2110_ancillary_input(&anc).expect("valid -40 input");
    }

    #[test]
    fn test_st2110_40_input_payload_type_out_of_range() {
        let anc = St2110AncillaryInputConfig {
            bind_addr: "239.10.10.10:5006".to_string(),
            interface_addr: None,
            redundancy: None,
            payload_type: 50,
            clock_domain: Some(0),
            allowed_sources: None,
        };
        assert!(validate_st2110_ancillary_input(&anc).is_err());
    }

    #[test]
    fn test_flow_with_st2110_input_validates() {
        let flow = FlowConfig {
            id: "audio-flow".to_string(),
            name: "Audio".to_string(),
            enabled: true,
            media_analysis: false,
            thumbnail: false,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: Some("group-1".to_string()),
            clock_domain: Some(0),
            input: Some(InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004"))),
            outputs: vec![OutputConfig::St2110_30(st2110_30_output(
                "out-1",
                "239.10.10.2:5004",
            ))],
        };
        validate_flow(&flow).expect("valid ST 2110 flow");
    }

    #[test]
    fn test_flow_group_member_must_exist() {
        let mut config = AppConfig::default();
        config.flow_groups.push(FlowGroupConfig {
            id: "group-1".to_string(),
            name: "Group 1".to_string(),
            clock_domain: Some(0),
            flows: vec!["does-not-exist".to_string()],
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_flow_group_back_reference_must_match() {
        // Flow declares membership in group-1 but is not listed as a member.
        let mut config = AppConfig::default();
        config.flows.push(FlowConfig {
            id: "audio-flow".to_string(),
            name: "Audio".to_string(),
            enabled: true,
            media_analysis: false,
            thumbnail: false,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: Some("group-1".to_string()),
            clock_domain: Some(0),
            input: Some(InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004"))),
            outputs: vec![],
        });
        config.flow_groups.push(FlowGroupConfig {
            id: "group-1".to_string(),
            name: "Group 1".to_string(),
            clock_domain: Some(0),
            flows: vec!["something-else".to_string()],
        });
        config.flows.push(FlowConfig {
            id: "something-else".to_string(),
            name: "Other".to_string(),
            enabled: true,
            media_analysis: false,
            thumbnail: false,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input: Some(InputConfig::St2110_30(st2110_30_input("239.10.10.2:5004"))),
            outputs: vec![],
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_flow_group_round_trip_ok() {
        let mut config = AppConfig::default();
        config.flows.push(FlowConfig {
            id: "audio-flow".to_string(),
            name: "Audio".to_string(),
            enabled: true,
            media_analysis: false,
            thumbnail: false,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: Some("group-1".to_string()),
            clock_domain: Some(0),
            input: Some(InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004"))),
            outputs: vec![],
        });
        config.flow_groups.push(FlowGroupConfig {
            id: "group-1".to_string(),
            name: "Group 1".to_string(),
            clock_domain: Some(0),
            flows: vec!["audio-flow".to_string()],
        });
        validate_config(&config).expect("valid flow-group config");
    }

    /// The manager wire format uses `flow_ids` for the membership list
    /// (matching the manager's REST/DB column name `essences` historically).
    /// The edge accepts both `flow_ids` and `flows` as input via a serde
    /// alias on `FlowGroupConfig.flows` so the WS commands `add_flow_group` /
    /// `update_flow_group` deserialize correctly. Persisted output always
    /// uses the canonical `flows` field.
    #[test]
    fn test_flow_group_accepts_flow_ids_alias() {
        let json = serde_json::json!({
            "id": "main-bundle",
            "name": "Main bundle",
            "clock_domain": 0,
            "flow_ids": ["audio-flow", "anc-flow"]
        });
        let group: FlowGroupConfig = serde_json::from_value(json).expect("flow_ids alias accepted");
        assert_eq!(group.id, "main-bundle");
        assert_eq!(group.flows, vec!["audio-flow".to_string(), "anc-flow".to_string()]);
        // And the canonical field name still works.
        let json2 = serde_json::json!({
            "id": "alt",
            "name": "Alt",
            "flows": ["x"]
        });
        let group2: FlowGroupConfig = serde_json::from_value(json2).expect("flows accepted");
        assert_eq!(group2.flows, vec!["x".to_string()]);
    }

    /// Existing testbed configs (SRT/RTP/RTMP/etc) must continue to deserialize
    /// and validate unchanged after the ST 2110 additions. This is the
    /// regression guard called out in the Phase 1 plan, step 1.
    #[test]
    fn test_existing_testbed_configs_still_load() {
        use std::fs;
        let testbed_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("testbed")
            .join("configs");
        if !testbed_dir.exists() {
            // Tolerate running outside the monorepo (CI / standalone clones).
            return;
        }
        let mut checked = 0usize;
        for entry in fs::read_dir(&testbed_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Edge configs only — skip the relay config which has a different shape.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "relay.json" {
                continue;
            }
            let raw = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            // Deserialize must succeed unchanged with the new optional fields.
            let cfg: AppConfig = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("deserialize {}: {e}", path.display()));
            // Loading must validate cleanly — the new ST 2110 additions should
            // not introduce false positives on pre-existing configs.
            validate_config(&cfg)
                .unwrap_or_else(|e| panic!("validate {}: {e}", path.display()));
            checked += 1;
        }
        assert!(checked > 0, "no testbed configs were checked");
    }

    // ── audio_encode validation tests ─────────────────────────────────

    fn make_audio_encode(codec: &str) -> crate::config::models::AudioEncodeConfig {
        crate::config::models::AudioEncodeConfig {
            codec: codec.to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
        }
    }

    #[test]
    fn validate_audio_encode_accepts_aac_lc_for_rtmp() {
        let enc = make_audio_encode("aac_lc");
        assert!(validate_audio_encode(&enc, &["aac_lc", "he_aac_v1", "he_aac_v2"], "test").is_ok());
    }

    #[test]
    fn validate_audio_encode_rejects_opus_on_rtmp() {
        let enc = make_audio_encode("opus");
        let err = validate_audio_encode(&enc, &["aac_lc", "he_aac_v1", "he_aac_v2"], "RTMP test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not allowed"), "got: {err}");
    }

    #[test]
    fn validate_audio_encode_rejects_aac_on_webrtc() {
        let enc = make_audio_encode("aac_lc");
        let err = validate_audio_encode(&enc, &["opus"], "WebRTC test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not allowed"), "got: {err}");
    }

    #[test]
    fn validate_audio_encode_rejects_unknown_codec() {
        let enc = make_audio_encode("flac");
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
    }

    #[test]
    fn validate_audio_encode_bitrate_bounds() {
        let mut enc = make_audio_encode("aac_lc");
        enc.bitrate_kbps = Some(8); // too low
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
        enc.bitrate_kbps = Some(1024); // too high
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
        enc.bitrate_kbps = Some(128); // just right
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok());
        enc.bitrate_kbps = Some(16); // edge low
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok());
        enc.bitrate_kbps = Some(512); // edge high
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok());
    }

    #[test]
    fn validate_audio_encode_sample_rate_whitelist() {
        let mut enc = make_audio_encode("aac_lc");
        for sr in [8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000] {
            enc.sample_rate = Some(sr);
            assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok(), "{sr}");
        }
        enc.sample_rate = Some(96_000);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
        enc.sample_rate = Some(11_025);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
    }

    #[test]
    fn validate_audio_encode_channels_per_codec() {
        // AAC-LC: 1-8 channels (fdk-aac supports up to 7.1)
        let mut enc = make_audio_encode("aac_lc");
        enc.channels = Some(1);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok());
        enc.channels = Some(2);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok());
        enc.channels = Some(6);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok(), "AAC-LC should allow 5.1");
        enc.channels = Some(8);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_ok(), "AAC-LC should allow 7.1");
        enc.channels = Some(0);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());
        enc.channels = Some(9);
        assert!(validate_audio_encode(&enc, &["aac_lc"], "test").is_err());

        // HE-AAC v1: 1-2 channels only
        let mut enc_he = make_audio_encode("he_aac_v1");
        enc_he.channels = Some(2);
        assert!(validate_audio_encode(&enc_he, &["he_aac_v1"], "test").is_ok());
        enc_he.channels = Some(6);
        assert!(validate_audio_encode(&enc_he, &["he_aac_v1"], "test").is_err(), "HE-AAC v1 max 2ch");

        // AC-3: up to 6 channels (5.1)
        let mut enc_ac3 = make_audio_encode("ac3");
        enc_ac3.channels = Some(6);
        assert!(validate_audio_encode(&enc_ac3, &["ac3"], "test").is_ok(), "AC-3 should allow 5.1");
        enc_ac3.channels = Some(8);
        assert!(validate_audio_encode(&enc_ac3, &["ac3"], "test").is_err(), "AC-3 max 6ch");
    }

    #[test]
    fn validate_output_rtmp_with_audio_encode() {
        use crate::config::models::{AudioEncodeConfig, OutputConfig, RtmpOutputConfig};
        let make = |codec: &str| OutputConfig::Rtmp(RtmpOutputConfig {
            id: "rtmp1".into(),
            name: "rtmp 1".into(),
            dest_url: "rtmp://example.com/app".into(),
            stream_key: "abc".into(),
            reconnect_delay_secs: 5,
            max_reconnect_attempts: None,
            program_number: None,
            audio_encode: Some(AudioEncodeConfig {
                codec: codec.into(),
                bitrate_kbps: None,
                sample_rate: None,
                channels: None,
            }),
        });
        assert!(validate_output(&make("aac_lc")).is_ok());
        assert!(validate_output(&make("he_aac_v1")).is_ok());
        // Opus must fail on RTMP.
        assert!(validate_output(&make("opus")).is_err());
        // MP2 must fail on RTMP (only HLS gets MP2/AC3).
        assert!(validate_output(&make("mp2")).is_err());
    }

    #[test]
    fn validate_output_hls_audio_encode_includes_mp2_ac3() {
        use crate::config::models::{AudioEncodeConfig, HlsOutputConfig, OutputConfig};
        let make = |codec: &str| OutputConfig::Hls(HlsOutputConfig {
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: "https://example.com/hls".into(),
            segment_duration_secs: 2.0,
            auth_token: None,
            max_segments: 5,
            program_number: None,
            audio_encode: Some(AudioEncodeConfig {
                codec: codec.into(),
                bitrate_kbps: None,
                sample_rate: None,
                channels: None,
            }),
        });
        assert!(validate_output(&make("aac_lc")).is_ok());
        assert!(validate_output(&make("mp2")).is_ok());
        assert!(validate_output(&make("ac3")).is_ok());
        assert!(validate_output(&make("opus")).is_err());
    }

    #[test]
    fn validate_output_webrtc_audio_encode_only_opus() {
        use crate::config::models::{
            AudioEncodeConfig, OutputConfig, WebrtcOutputConfig, WebrtcOutputMode,
        };
        let make = |codec: &str, video_only: bool| OutputConfig::Webrtc(WebrtcOutputConfig {
            id: "wrtc1".into(),
            name: "wrtc 1".into(),
            mode: WebrtcOutputMode::WhepServer,
            whip_url: None,
            bearer_token: None,
            max_viewers: Some(10),
            public_ip: None,
            video_only,
            program_number: None,
            audio_encode: Some(AudioEncodeConfig {
                codec: codec.into(),
                bitrate_kbps: None,
                sample_rate: None,
                channels: None,
            }),
        });
        // Opus + audio MID negotiated → OK.
        assert!(validate_output(&make("opus", false)).is_ok());
        // Opus + video_only=true → must reject (no audio MID).
        let err = validate_output(&make("opus", true)).unwrap_err().to_string();
        assert!(err.contains("video_only=false"), "got: {err}");
        // AAC on WebRTC → must reject.
        assert!(validate_output(&make("aac_lc", false)).is_err());
    }
}
