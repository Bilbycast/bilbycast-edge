// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{bail, Result};

use super::models::*;

/// Validates the entire application configuration.
///
/// Performs the following checks:
/// 1. **Top-level inputs**: duplicate ID detection, per-input validation via
///    [`validate_input_definition`].
/// 2. **Top-level outputs**: duplicate ID detection, per-output validation via
///    [`validate_output`].
/// 3. **Flows**: duplicate flow ID detection, per-flow metadata validation via
///    [`validate_flow`], `input_id` / `output_ids` reference resolution against
///    the top-level definitions, and assignment uniqueness (no input or output
///    used by more than one flow).
/// 4. **Upstream-aware output re-validation**: outputs whose flow has an
///    uncompressed audio input are re-validated with the real upstream audio
///    shape so transcode channel-map presets resolve correctly.
/// 5. **Flow groups**: cross-references between flows and groups.
///
/// # Errors
///
/// Returns an error describing the first validation failure encountered.
pub fn validate_config(config: &AppConfig) -> Result<()> {
    // Schema version gate. The current schema is v2, where inputs and outputs
    // live in top-level `inputs` / `outputs` arrays and flows reference them
    // by ID. Older v1 configs nested `input` and `outputs` inside each flow;
    // serde silently ignores those unknown fields, which would produce a
    // running flow with zero inputs/outputs and no error. Fail closed instead
    // and tell the operator how to migrate.
    if config.version != 2 {
        bail!(
            "Unsupported config schema version {} — this build requires version 2. \
             If this is a legacy v1 config (with nested `input`/`outputs` inside flows), \
             run `python3 testbed/quality/migrate_v1_to_v2.py` to migrate it in place.",
            config.version
        );
    }

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
        match (auth.enabled, auth.nmos_require_auth) {
            (true, Some(false)) => {
                tracing::warn!(
                    "SECURITY: auth is enabled but nmos_require_auth was explicitly set to \
                     false — NMOS IS-04/IS-05/IS-08 will be reachable unauthenticated. \
                     Remove this field or set it to true to protect NMOS endpoints."
                );
            }
            (true, None) => {
                tracing::info!(
                    "NMOS IS-04/IS-05/IS-08 endpoints will require JWT Bearer auth by default \
                     (auth.enabled is true and nmos_require_auth is unset). Set \
                     nmos_require_auth: false to opt out."
                );
            }
            (false, Some(true)) => {
                tracing::warn!(
                    "nmos_require_auth is true but auth is disabled — NMOS endpoints will remain public"
                );
            }
            _ => {}
        }
    }

    // Validate device name if present
    if let Some(ref name) = config.device_name {
        if name.len() > 256 {
            bail!("Device name must be at most 256 characters");
        }
    }

    // Validate NMOS registration-client config if present
    if let Some(ref nr) = config.nmos_registration {
        validate_nmos_registration(nr)?;
    }

    // Validate manager config if present
    if let Some(ref mgr) = config.manager {
        if mgr.enabled {
            if mgr.urls.is_empty() {
                bail!(
                    "Manager urls[] cannot be empty when manager is enabled. \
                     Single-URL deployments should still use a one-element array."
                );
            }
            if mgr.urls.len() > 16 {
                bail!(
                    "Manager urls[] must contain at most 16 entries ({} given). \
                     Large fleets should front their cluster with a load balancer \
                     rather than enumerate every instance here.",
                    mgr.urls.len()
                );
            }
            let mut seen: HashSet<&str> = HashSet::new();
            for (i, url) in mgr.urls.iter().enumerate() {
                if url.is_empty() {
                    bail!("Manager urls[{i}] is empty");
                }
                if !url.starts_with("wss://") {
                    bail!(
                        "Manager urls[{i}] = {url:?} must start with wss:// (TLS required)"
                    );
                }
                if url.len() > 2048 {
                    bail!("Manager urls[{i}] must be at most 2048 characters");
                }
                if !seen.insert(url.as_str()) {
                    bail!("Manager urls[{i}] = {url:?} is a duplicate");
                }
            }
            if let Some(ref token) = mgr.registration_token {
                if token.len() > 4096 {
                    bail!("Manager registration token must be at most 4096 characters");
                }
            }
        }
    }

    // ── Top-level input definitions ──────────────────────────────────
    let mut input_ids: HashSet<String> = HashSet::new();
    for def in &config.inputs {
        if !input_ids.insert(def.id.clone()) {
            bail!("Duplicate top-level input ID: {}", def.id);
        }
        validate_input_definition(def)?;
    }

    // ── Top-level output definitions ──────────────────────────────────
    let mut output_ids_set: HashSet<String> = HashSet::new();
    for output in &config.outputs {
        let oid = output.id().to_string();
        if !output_ids_set.insert(oid.clone()) {
            bail!("Duplicate top-level output ID: {}", oid);
        }
        // Determine upstream audio shape from the flow that owns this output
        // (if any). At this stage we validate outputs standalone — transcode
        // channel-map presets will be resolved against the output's own declared
        // shape when no upstream is known.
        validate_output(output)?;
    }

    // ── Flow definitions ──────────────────────────────────────────────
    let mut flow_ids: HashSet<String> = HashSet::new();
    let mut assigned_inputs: HashSet<String> = HashSet::new();
    let mut assigned_outputs: HashSet<String> = HashSet::new();
    for flow in &config.flows {
        if !flow_ids.insert(flow.id.clone()) {
            bail!("Duplicate flow ID: {}", flow.id);
        }
        validate_flow(flow)?;

        // Validate input_ids references. A flow may have multiple inputs;
        // cross-flow input assignment uniqueness is still enforced (an input
        // can only belong to one flow at a time).
        let mut flow_input_ids = HashSet::new();
        let mut active_input_count = 0usize;
        for iid in &flow.input_ids {
            if !flow_input_ids.insert(iid.clone()) {
                bail!("Flow '{}': duplicate input_id reference '{}'", flow.id, iid);
            }
            if !input_ids.contains(iid) {
                bail!(
                    "Flow '{}': input_id '{}' does not reference a defined top-level input",
                    flow.id, iid
                );
            }
            if !assigned_inputs.insert(iid.clone()) {
                bail!(
                    "Flow '{}': input '{}' is already assigned to another flow",
                    flow.id, iid
                );
            }
            // Count active inputs. At most one input per flow may be active.
            if let Some(def) = config.inputs.iter().find(|d| d.id == *iid) {
                if def.active {
                    active_input_count += 1;
                }
            }
        }
        // The "at most one active input" invariant belongs to the
        // passthrough active-input switching model — only one input's
        // bytes reach `broadcast_tx` at any moment. PID-bus assembly
        // flows (`kind = spts` / `mpts`) break that model deliberately:
        // every input's `TsEsDemuxer` publishes onto the shared
        // [`FlowEsBus`] concurrently, and the assembler pulls ES from
        // whichever slots are configured. The `active` flag is unused
        // on the assembly path, so skip the count check there.
        let assembly_mode = flow
            .assembly
            .as_ref()
            .is_some_and(|a| !matches!(a.kind, crate::config::models::AssemblyKind::Passthrough));
        if !assembly_mode && active_input_count > 1 {
            bail!(
                "Flow '{}': at most one input may be active at a time, found {}",
                flow.id, active_input_count
            );
        }

        // Validate output_ids references
        let mut flow_output_ids = HashSet::new();
        for oid in &flow.output_ids {
            if !flow_output_ids.insert(oid.clone()) {
                bail!("Flow '{}': duplicate output_id reference '{}'", flow.id, oid);
            }
            if !output_ids_set.contains(oid) {
                bail!(
                    "Flow '{}': output_id '{}' does not reference a defined top-level output",
                    flow.id, oid
                );
            }
            if !assigned_outputs.insert(oid.clone()) {
                bail!(
                    "Flow '{}': output '{}' is already assigned to another flow",
                    flow.id, oid
                );
            }
        }

        // Broadcast-channel shape compatibility.
        //
        // Every input on the flow must publish the same shape onto the main
        // broadcast channel, otherwise outputs would see random format switches
        // when the active input changes. Two shapes are possible:
        //
        //   * "ts"      — MPEG-TS bytes (native TS transports, and PCM inputs
        //                 with `audio_encode` set, which mux into audio-only TS)
        //   * "pcm_rtp" — raw RTP (ST 2110-30/-31, rtp_audio, ST 2110-40)
        //
        // ST 2110-40 (ANC) is non-media and passes through regardless — it has
        // no broadcast shape of its own. We skip it in the compatibility check.
        let flow_inputs: Vec<&InputConfig> = flow
            .input_ids
            .iter()
            .filter_map(|iid| config.inputs.iter().find(|d| d.id == *iid).map(|d| &d.config))
            .filter(|c| !matches!(c, InputConfig::St2110_40(_)))
            .collect();
        if flow_inputs.len() >= 2 {
            let first_shape = flow_inputs[0].produces_ts();
            for (idx, inp) in flow_inputs.iter().enumerate().skip(1) {
                if inp.produces_ts() != first_shape {
                    bail!(
                        "Flow '{}': input #0 produces {} but input #{} produces {} — \
                         all inputs on a flow must share the same broadcast-channel shape. \
                         Add `audio_encode` to the PCM input(s) to convert them to TS, or \
                         remove `audio_encode` so the flow stays on PCM-RTP.",
                        flow.id,
                        if first_shape { "MPEG-TS" } else { "PCM-RTP" },
                        idx,
                        if inp.produces_ts() { "MPEG-TS" } else { "PCM-RTP" },
                    );
                }
            }
        }

        // If the flow broadcasts TS (either natively or because a PCM input has
        // `audio_encode` set), PCM-only outputs cannot attach: they depacketize
        // RTP PCM and would silently produce noise if fed encoded TS.
        let flow_produces_ts = flow_inputs.first().map(|c| c.produces_ts()).unwrap_or(false);
        if flow_produces_ts {
            for oid in &flow.output_ids {
                let Some(output) = config.outputs.iter().find(|o| o.id() == oid) else {
                    continue;
                };
                if output_is_pcm_only(output) {
                    bail!(
                        "Flow '{}': output '{}' expects raw PCM-RTP on the broadcast \
                         channel, but this flow's inputs produce MPEG-TS (either natively \
                         or via `audio_encode` on a PCM input). Remove the PCM-only output \
                         from this flow, or remove `audio_encode` from the PCM input(s).",
                        flow.id, oid
                    );
                }
            }
        }
    }

    // ── Upstream-aware output validation ──────────────────────────────
    // Now that we know which input feeds each output (via flows), re-validate
    // outputs that have transcode blocks with the real upstream audio shape
    // so channel-map presets resolve against the actual input channel count.
    // When a flow has multiple inputs, use the currently active one for
    // upstream shape determination.
    for flow in &config.flows {
        let active_input = flow
            .input_ids
            .iter()
            .filter_map(|iid| config.inputs.iter().find(|d| d.id == *iid))
            .find(|d| d.active);
        let upstream_audio = active_input.and_then(|d| upstream_audio_shape(&d.config));
        if upstream_audio.is_some() {
            for oid in &flow.output_ids {
                if let Some(output) = config.outputs.iter().find(|o| o.id() == oid) {
                    validate_output_with_input(output, upstream_audio)?;
                }
            }
        }
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

    // Cross-component port conflict detection
    validate_port_conflicts(config)?;

    Ok(())
}

/// Validates a single flow configuration (metadata only).
///
/// Performs the following checks in order:
/// 1. **Non-empty ID**: the flow `id` must not be an empty string (max 64 chars).
/// 2. **Non-empty name**: the flow `name` must not be an empty string (max 256 chars).
/// 3. **`input_id` / `output_ids` format**: validates string length constraints.
/// 4. **Metadata**: thumbnail_program_number, flow_group_id, clock_domain,
///    bandwidth_limit.
///
/// Input/output existence and assignment uniqueness are validated at the
/// config level in [`validate_config`].
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

    // Validate optional PID-bus assembly plan (schema-only — runtime is
    // not yet implemented and `FlowRuntime::start` refuses to bring up
    // flows carrying one until Phase 4/5 lands).
    if let Some(ref assembly) = flow.assembly {
        validate_flow_assembly(assembly, &flow.input_ids, &format!("Flow '{}'", flow.id))?;
    }

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

    // Validate input_ids reference format (length only — existence checked in validate_config)
    for iid in &flow.input_ids {
        if iid.is_empty() {
            bail!("Flow '{}': input_ids must not contain empty strings", flow.id);
        }
        if iid.len() > 64 {
            bail!("Flow '{}': input_id '{}' must be at most 64 characters", flow.id, iid);
        }
    }

    // Validate output_ids reference format
    for oid in &flow.output_ids {
        if oid.is_empty() {
            bail!("Flow '{}': output_ids must not contain empty strings", flow.id);
        }
        if oid.len() > 64 {
            bail!("Flow '{}': output_id '{}' must be at most 64 characters", flow.id, oid);
        }
    }

    // Validate optional content-analysis configuration.
    if let Some(ref ca) = flow.content_analysis {
        if let Some(hz) = ca.video_full_hz {
            if !(hz.is_finite() && hz > 0.0 && hz <= 30.0) {
                bail!(
                    "Flow '{}': content_analysis.video_full_hz must be in (0.0, 30.0], got {}",
                    flow.id, hz
                );
            }
        }
    }

    // Validate optional recording attributes (replay server, Phase 1).
    if let Some(ref rec) = flow.recording {
        validate_recording_config(rec, &flow.id)?;
    }

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

    // Note: input/output validation is done at the top level in validate_config()
    // since inputs and outputs are now independent top-level definitions referenced
    // by flows via input_id / output_ids.

    Ok(())
}

/// Returns true when this output expects raw PCM-RTP on the flow's broadcast
/// channel. Used by the flow-level shape-compatibility check: these outputs
/// cannot attach to a flow whose inputs produce MPEG-TS.
fn output_is_pcm_only(output: &OutputConfig) -> bool {
    matches!(
        output,
        OutputConfig::St2110_30(_)
            | OutputConfig::St2110_31(_)
            | OutputConfig::St2110_40(_)
            | OutputConfig::RtpAudio(_)
    )
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

/// Validates a top-level input definition.
///
/// Checks that `id` and `name` are non-empty and within length limits, then
/// delegates to [`validate_input`] for protocol-specific validation.
pub fn validate_input_definition(def: &InputDefinition) -> Result<()> {
    if def.id.is_empty() {
        bail!("Input definition ID cannot be empty");
    }
    if def.id.len() > 64 {
        bail!("Input definition ID must be at most 64 characters");
    }
    if def.name.is_empty() {
        bail!("Input definition name cannot be empty");
    }
    if def.name.len() > 256 {
        bail!("Input definition name must be at most 256 characters");
    }
    if let Some(ref group) = def.group {
        if group.len() > 64 {
            bail!(
                "Input '{}': group tag must be at most 64 characters",
                def.id
            );
        }
    }
    validate_input(&def.config)?;
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
            validate_ssm_source(&rtp.bind_addr, rtp.source_addr.as_deref(), "RTP input")?;
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
                validate_ssm_source(
                    &red.bind_addr,
                    red.source_addr.as_deref(),
                    "RTP input redundancy",
                )?;
            }
            validate_input_transcode_group_a(
                rtp.audio_encode.as_ref(),
                rtp.transcode.as_ref(),
                rtp.video_encode.as_ref(),
                "RTP input",
            )?;
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
            validate_ssm_source(&udp.bind_addr, udp.source_addr.as_deref(), "UDP input")?;
            validate_input_transcode_group_a(
                udp.audio_encode.as_ref(),
                udp.transcode.as_ref(),
                udp.video_encode.as_ref(),
                "UDP input",
            )?;
        }
        InputConfig::Srt(srt) => {
            match srt.mode {
                SrtMode::Listener | SrtMode::Rendezvous => {
                    let addr = srt.local_addr.as_deref()
                        .ok_or_else(|| anyhow::anyhow!("SRT input local_addr is required for {:?} mode", srt.mode))?;
                    validate_socket_addr(addr, "SRT input local_addr")?;
                }
                SrtMode::Caller => {
                    if let Some(ref addr) = srt.local_addr {
                        validate_socket_addr(addr, "SRT input local_addr")?;
                    }
                }
            }
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
            if srt.redundancy.is_some() && srt.bonding.is_some() {
                bail!(
                    "SRT input: `redundancy` and `bonding` are mutually exclusive (bonding is libsrt-native SMPTE 2022-7 / primary-backup; redundancy is the app-layer fallback)"
                );
            }
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT input")?;
            }
            if let Some(ref bond) = srt.bonding {
                validate_srt_bonding(bond, &srt.mode, "SRT input")?;
            }
            if let Some(ref tm) = srt.transport_mode {
                if !matches!(tm.as_str(), "ts" | "audio_302m") {
                    bail!(
                        "SRT input: transport_mode must be 'ts' or 'audio_302m', got '{}'",
                        tm
                    );
                }
            }
            validate_input_transcode_group_a(
                srt.audio_encode.as_ref(),
                srt.transcode.as_ref(),
                srt.video_encode.as_ref(),
                "SRT input",
            )?;
        }
        InputConfig::Rist(rist) => {
            validate_rist_input(rist)?;
            validate_input_transcode_group_a(
                rist.audio_encode.as_ref(),
                rist.transcode.as_ref(),
                rist.video_encode.as_ref(),
                "RIST input",
            )?;
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
            validate_input_transcode_group_a(
                rtmp.audio_encode.as_ref(),
                rtmp.transcode.as_ref(),
                rtmp.video_encode.as_ref(),
                "RTMP input",
            )?;
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
            validate_input_transcode_group_a(
                rtsp.audio_encode.as_ref(),
                rtsp.transcode.as_ref(),
                rtsp.video_encode.as_ref(),
                "RTSP input",
            )?;
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
            validate_input_transcode_group_a(
                webrtc.audio_encode.as_ref(),
                webrtc.transcode.as_ref(),
                webrtc.video_encode.as_ref(),
                "WebRTC input",
            )?;
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
            if let Some(ref fp) = whep.cert_fingerprint {
                crate::util::tls::canonicalise_fingerprint(fp)
                    .map_err(|e| anyhow::anyhow!("WHEP input: cert_fingerprint: {}", e))?;
            }
            validate_input_transcode_group_a(
                whep.audio_encode.as_ref(),
                whep.transcode.as_ref(),
                whep.video_encode.as_ref(),
                "WHEP input",
            )?;
        }
        InputConfig::St2110_30(c) => validate_st2110_audio_input(c, St2110Profile::Pcm)?,
        InputConfig::St2110_31(c) => validate_st2110_audio_input(c, St2110Profile::Aes3)?,
        InputConfig::St2110_40(c) => validate_st2110_ancillary_input(c)?,
        InputConfig::St2110_20(c) => validate_st2110_video_input(c)?,
        InputConfig::St2110_23(c) => validate_st2110_23_input(c)?,
        InputConfig::RtpAudio(c) => validate_rtp_audio_input(c)?,
        InputConfig::Bonded(c) => validate_bonded_input(c)?,
        InputConfig::TestPattern(c) => validate_test_pattern_input(c)?,
        InputConfig::MediaPlayer(c) => validate_media_player_input(c)?,
        InputConfig::Replay(c) => validate_replay_input(c)?,
    }
    Ok(())
}

/// Validate a [`crate::config::models::ReplayInputConfig`]. Checks that
/// `recording_id` and optional `clip_id` are sane identifiers; the actual
/// existence of the recording on disk is checked at input start so the
/// runtime can emit a clear `replay_clip_not_found` event the manager
/// surfaces, rather than blocking config save.
fn validate_replay_input(c: &crate::config::models::ReplayInputConfig) -> Result<()> {
    validate_replay_id(&c.recording_id, "replay input recording_id")?;
    if let Some(ref clip_id) = c.clip_id {
        validate_replay_id(clip_id, "replay input clip_id")?;
    }
    Ok(())
}

/// Strict identifier validator for replay recording / clip IDs. 1–64
/// chars, ASCII alphanumerics + `_-`. Mirrors the media-filename
/// character set minus path-friendly punctuation; replay IDs are
/// internal and don't need to round-trip through file pickers.
pub(crate) fn validate_replay_id(id: &str, label: &str) -> Result<()> {
    if id.is_empty() {
        bail!("{label}: must not be empty");
    }
    if id.len() > 64 {
        bail!("{label}: must be at most 64 characters (got {})", id.len());
    }
    for ch in id.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
        if !ok {
            bail!(
                "{label}: may only contain ASCII alphanumerics and '_-' (got '{ch}')"
            );
        }
    }
    Ok(())
}

/// Validate a [`crate::config::models::RecordingConfig`]. Bounds-check the
/// segment cadence, sanity-check the optional storage_id, and accept the
/// retention / size caps as advisory (the writer enforces them at runtime).
fn validate_recording_config(
    c: &crate::config::models::RecordingConfig,
    flow_id: &str,
) -> Result<()> {
    if c.segment_seconds < 2 || c.segment_seconds > 60 {
        bail!(
            "Flow '{flow_id}': recording.segment_seconds must be in 2..=60 (got {})",
            c.segment_seconds
        );
    }
    if let Some(ref sid) = c.storage_id {
        validate_replay_id(sid, &format!("Flow '{flow_id}' recording.storage_id"))?;
    }
    if let Some(pb) = c.pre_buffer_seconds {
        if !(1..=300).contains(&pb) {
            bail!(
                "Flow '{flow_id}': recording.pre_buffer_seconds must be in 1..=300 (got {pb})"
            );
        }
        // Pre-buffer is the *floor* of retention while disarmed — a
        // pre-buffer larger than the configured retention is incoherent
        // (the writer would prune segments inside the pre-buffer window
        // on every roll). Skip the comparison when retention is
        // unlimited (0) — pre-buffer is bounded only by max_bytes /
        // free disk in that mode.
        if c.retention_seconds != 0 && pb as u64 > c.retention_seconds {
            bail!(
                "Flow '{flow_id}': recording.pre_buffer_seconds ({pb}) cannot exceed \
                 recording.retention_seconds ({})",
                c.retention_seconds
            );
        }
    }
    if let Some(fs) = c.filmstrip_seconds {
        if !(1..=30).contains(&fs) {
            bail!(
                "Flow '{flow_id}': recording.filmstrip_seconds must be in 1..=30 (got {fs})"
            );
        }
    }
    Ok(())
}

/// Validate a synthetic test-pattern input. All fields have sensible
/// defaults; this is the sanity-bound check.
fn validate_test_pattern_input(c: &crate::config::models::TestPatternInputConfig) -> Result<()> {
    if c.width < 64 || c.width > 7680 || c.width % 2 != 0 {
        return Err(anyhow::anyhow!(
            "test-pattern: width must be an even number in 64..=7680 (got {})", c.width
        ));
    }
    if c.height < 64 || c.height > 4320 || c.height % 2 != 0 {
        return Err(anyhow::anyhow!(
            "test-pattern: height must be an even number in 64..=4320 (got {})", c.height
        ));
    }
    if c.fps < 1 || c.fps > 60 {
        return Err(anyhow::anyhow!(
            "test-pattern: fps must be in 1..=60 (got {})", c.fps
        ));
    }
    if c.video_bitrate_kbps < 100 || c.video_bitrate_kbps > 200_000 {
        return Err(anyhow::anyhow!(
            "test-pattern: video_bitrate_kbps must be in 100..=200000 (got {})",
            c.video_bitrate_kbps
        ));
    }
    if c.audio_enabled {
        if c.tone_hz < 50.0 || c.tone_hz > 8000.0 {
            return Err(anyhow::anyhow!(
                "test-pattern: tone_hz must be in 50..=8000 (got {})", c.tone_hz
            ));
        }
        if c.tone_dbfs < -60.0 || c.tone_dbfs > 0.0 {
            return Err(anyhow::anyhow!(
                "test-pattern: tone_dbfs must be in -60..=0 (got {})", c.tone_dbfs
            ));
        }
    }
    Ok(())
}

/// Validate a media-player input. Each source's `name` must be a valid
/// media-library filename (no path components, no traversal, length cap),
/// and the kind-specific bounds are sanity-checked. The actual file
/// existence check is deferred to input start so the runtime can emit a
/// clear error the manager surfaces, rather than blocking config save.
fn validate_media_player_input(c: &crate::config::models::MediaPlayerInputConfig) -> Result<()> {
    use crate::config::models::MediaPlayerSource;
    if c.sources.is_empty() {
        bail!("media-player input: at least one source is required");
    }
    if c.sources.len() > 256 {
        bail!(
            "media-player input: at most 256 sources per playlist (got {})",
            c.sources.len()
        );
    }
    for (i, src) in c.sources.iter().enumerate() {
        match src {
            MediaPlayerSource::Ts { name }
            | MediaPlayerSource::Mp4 { name }
            | MediaPlayerSource::Image { name, .. } => {
                validate_media_filename(name, &format!("media-player input sources[{i}].name"))?;
            }
        }
        if let MediaPlayerSource::Image { fps, bitrate_kbps, .. } = src {
            if *fps == 0 || *fps > 60 {
                bail!(
                    "media-player input sources[{i}]: image fps must be in 1..=60 (got {})",
                    fps
                );
            }
            if *bitrate_kbps < 50 || *bitrate_kbps > 50_000 {
                bail!(
                    "media-player input sources[{i}]: image bitrate_kbps must be in 50..=50000 (got {})",
                    bitrate_kbps
                );
            }
        }
    }
    if let Some(bps) = c.paced_bitrate_bps {
        if !(100_000..=200_000_000).contains(&bps) {
            bail!(
                "media-player input: paced_bitrate_bps must be in 100000..=200000000 (got {bps})"
            );
        }
    }
    Ok(())
}

/// Strict filename validator for media-library entries. Rejects path
/// separators, parent-directory traversal, leading dots, control chars,
/// and over-length names. The character set is intentionally narrow —
/// alphanumeric plus `._- ` — so cross-platform pickers and shells never
/// have to escape anything.
pub(crate) fn validate_media_filename(name: &str, label: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{label}: filename must not be empty");
    }
    if name.len() > 255 {
        bail!("{label}: filename must be at most 255 characters (got {})", name.len());
    }
    if name.starts_with('.') {
        bail!("{label}: filename must not start with '.'");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("{label}: filename must not contain path separators");
    }
    if name == "." || name == ".." || name.contains("..") {
        bail!("{label}: filename must not contain '..'");
    }
    for ch in name.chars() {
        let ok = ch.is_ascii_alphanumeric()
            || ch == '.'
            || ch == '_'
            || ch == '-'
            || ch == ' ';
        if !ok {
            bail!(
                "{label}: filename may only contain ASCII alphanumerics and '._- ' (got '{ch}')"
            );
        }
    }
    Ok(())
}

/// Validate a bonded input config. Checks path IDs unique, path
/// count ≥ 1, role semantics consistent with receiver side, and
/// that each path has the addresses required for its role.
fn validate_bonded_input(c: &crate::config::models::BondedInputConfig) -> Result<()> {
    if c.paths.is_empty() {
        return Err(anyhow::anyhow!("bonded input: at least one path required"));
    }
    let mut ids = std::collections::HashSet::new();
    for p in &c.paths {
        if !ids.insert(p.id) {
            return Err(anyhow::anyhow!(
                "bonded input: duplicate path id {}",
                p.id
            ));
        }
        validate_bond_path_transport(&p.transport, /* sender_mode */ false, &p.name)?;
    }
    if let Some(ms) = c.hold_ms {
        if !(10..=30_000).contains(&ms) {
            return Err(anyhow::anyhow!(
                "bonded input hold_ms must be in [10, 30000], got {ms}"
            ));
        }
    }
    Ok(())
}

/// Shared path-transport validation for bonded input/output.
fn validate_bond_path_transport(
    t: &crate::config::models::BondPathTransportConfig,
    sender_mode: bool,
    path_name: &str,
) -> Result<()> {
    use crate::config::models::{BondPathTransportConfig, BondQuicRole, BondRistRole};
    match t {
        BondPathTransportConfig::Udp {
            bind,
            remote,
            interface,
        } => {
            if let Some(b) = bind {
                validate_socket_addr(b, "bonded UDP path bind")?;
            }
            if let Some(r) = remote {
                validate_socket_addr(r, "bonded UDP path remote")?;
            }
            if sender_mode && remote.is_none() && bind.is_none() {
                return Err(anyhow::anyhow!(
                    "bonded UDP path '{path_name}' (sender) requires at least remote or bind"
                ));
            }
            if !sender_mode && bind.is_none() {
                return Err(anyhow::anyhow!(
                    "bonded UDP path '{path_name}' (receiver) requires bind"
                ));
            }
            if let Some(iface) = interface {
                // IFNAMSIZ is 16 on Linux (15 chars + NUL). Keep this
                // the portable limit so configs validate the same on
                // every platform.
                if iface.is_empty() || iface.len() > 15 {
                    return Err(anyhow::anyhow!(
                        "bonded UDP path '{path_name}' interface name must be 1..=15 chars"
                    ));
                }
                if iface.contains('\0') {
                    return Err(anyhow::anyhow!(
                        "bonded UDP path '{path_name}' interface must not contain NUL"
                    ));
                }
            }
        }
        BondPathTransportConfig::Rist {
            role,
            remote,
            local_bind,
            buffer_ms,
        } => {
            let expected_role = if sender_mode {
                BondRistRole::Sender
            } else {
                BondRistRole::Receiver
            };
            if *role != expected_role {
                return Err(anyhow::anyhow!(
                    "bonded RIST path '{path_name}' role mismatch: side is {} but role is {}",
                    if sender_mode { "output/sender" } else { "input/receiver" },
                    match role {
                        BondRistRole::Sender => "sender",
                        BondRistRole::Receiver => "receiver",
                    }
                ));
            }
            match role {
                BondRistRole::Sender => {
                    let r = remote.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("bonded RIST path '{path_name}' sender role requires remote")
                    })?;
                    validate_socket_addr(r, "bonded RIST path remote")?;
                }
                BondRistRole::Receiver => {
                    let b = local_bind.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "bonded RIST path '{path_name}' receiver role requires local_bind"
                        )
                    })?;
                    validate_socket_addr(b, "bonded RIST path local_bind")?;
                }
            }
            if let Some(ms) = buffer_ms {
                if !(50..=30_000).contains(ms) {
                    return Err(anyhow::anyhow!(
                        "bonded RIST path '{path_name}' buffer_ms must be in [50, 30000]"
                    ));
                }
            }
        }
        BondPathTransportConfig::Quic {
            role,
            addr,
            server_name,
            tls,
        } => {
            let expected = if sender_mode {
                BondQuicRole::Client
            } else {
                BondQuicRole::Server
            };
            if *role != expected {
                return Err(anyhow::anyhow!(
                    "bonded QUIC path '{path_name}' role mismatch: side expects {} but got {}",
                    match expected {
                        BondQuicRole::Client => "client",
                        BondQuicRole::Server => "server",
                    },
                    match role {
                        BondQuicRole::Client => "client",
                        BondQuicRole::Server => "server",
                    }
                ));
            }
            validate_socket_addr(addr, "bonded QUIC path addr")?;
            if matches!(role, BondQuicRole::Client) && server_name.is_empty() {
                return Err(anyhow::anyhow!(
                    "bonded QUIC client path '{path_name}' requires server_name"
                ));
            }
            validate_bond_tls(tls, path_name)?;
        }
    }
    Ok(())
}

/// Extract the local UDP port to register (receiver-side paths) so
/// the port-conflict check can spot bonded inputs colliding with
/// other services. Returns `None` for paths that don't pin a port
/// (sender-role RIST, QUIC client, UDP sender-mode with no bind).
fn bond_path_bind_addr(t: &crate::config::models::BondPathTransportConfig) -> Option<String> {
    use crate::config::models::{BondPathTransportConfig, BondQuicRole, BondRistRole};
    match t {
        BondPathTransportConfig::Udp { bind, .. } => bind.clone(),
        BondPathTransportConfig::Rist {
            role, local_bind, ..
        } if *role == BondRistRole::Receiver => local_bind.clone(),
        BondPathTransportConfig::Quic {
            role: BondQuicRole::Server,
            addr,
            ..
        } => Some(addr.clone()),
        _ => None,
    }
}

fn validate_bond_tls(
    tls: &crate::config::models::BondQuicTls,
    path_name: &str,
) -> Result<()> {
    use crate::config::models::BondQuicTls;
    match tls {
        BondQuicTls::SelfSigned => Ok(()),
        BondQuicTls::Pem {
            cert_chain_path,
            private_key_path,
            ..
        } => {
            if cert_chain_path.is_empty() {
                return Err(anyhow::anyhow!(
                    "bonded QUIC path '{path_name}' TLS pem requires cert_chain_path"
                ));
            }
            if private_key_path.is_empty() {
                return Err(anyhow::anyhow!(
                    "bonded QUIC path '{path_name}' TLS pem requires private_key_path"
                ));
            }
            Ok(())
        }
    }
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
    validate_ssm_source(&c.bind_addr, c.source_addr.as_deref(), &format!("{label} input"))?;
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

    // Transcode / audio_encode on PCM inputs.
    //
    // ST 2110-31 (AES3-transparent): both are rejected. The payload carries
    // SMPTE 337M sub-frames (possibly Dolby E); running it through a linear
    // PCM transcoder or decoder would silently destroy metadata. This matches
    // the existing output-side exclusion.
    //
    // ST 2110-30 (linear PCM): the input-shape is known at config time, so
    // `transcode` uses the eager validator. `audio_encode` is valid but changes
    // the flow's broadcast-channel shape (PCM-RTP → audio-only TS) — the
    // flow-level check in `validate_config` rejects incompatible outputs on
    // the same flow.
    match profile {
        St2110Profile::Aes3 => {
            if c.transcode.is_some() {
                bail!(
                    "{label} input: transcode is not supported on AES3-transparent inputs — \
                     the payload carries SMPTE 337M sub-frames that a linear-PCM transcoder \
                     would corrupt",
                );
            }
            // ST 2110-31 only accepts `audio_encode.codec = "s302m"`, which
            // re-packetises AES3 bytes into a SMPTE 302M private PES with
            // no decode step (bit-for-bit preservation of 337M sub-frames).
            if let Some(ref ae) = c.audio_encode {
                validate_audio_encode(
                    ae,
                    AES3_INPUT_AUDIO_CODECS,
                    &format!("{label} input"),
                )?;
            }
        }
        St2110Profile::Pcm => {
            if let Some(ref tj) = c.transcode {
                validate_transcode_block(
                    tj,
                    c.sample_rate,
                    c.bit_depth,
                    c.channels,
                    &format!("{label} input transcode"),
                )?;
            }
            if let Some(ref ae) = c.audio_encode {
                validate_audio_encode(
                    ae,
                    PCM_INPUT_AUDIO_CODECS,
                    &format!("{label} input"),
                )?;
            }
        }
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
        // When the parent flow's upstream audio shape is known, validate
        // channel-map presets against the *real* upstream channel count.
        // When the upstream isn't known (standalone output validation, or
        // upstream is non-audio like RTMP/RTSP feeding the audio_decode
        // bridge), fall back to the deferred validator: range checks only,
        // skip the input-shape-dependent preset/matrix checks. Those run
        // again at flow-resolution time when the upstream is bound.
        if let Some((in_sr, in_bd, in_ch)) = upstream_audio {
            validate_transcode_block(
                tj,
                in_sr,
                in_bd,
                in_ch,
                &format!("{label} output '{}' transcode", c.id),
            )?;
        } else {
            validate_transcode_block_deferred(
                tj,
                &format!("{label} output '{}' transcode", c.id),
            )?;
        }
    }
    if let Some(idx) = c.audio_track_index {
        if idx > 15 {
            bail!(
                "{label} output '{}': audio_track_index must be 0-15, got {}",
                c.id,
                idx
            );
        }
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
    {
        let sources = [
            tj.channel_map.is_some(),
            tj.channel_map_with_gain.is_some(),
            tj.channel_map_preset.is_some(),
        ];
        if sources.iter().filter(|&&v| v).count() > 1 {
            bail!(
                "{context}: channel_map, channel_map_with_gain, and channel_map_preset \
                 are mutually exclusive — specify at most one"
            );
        }
    }
    // Validate channel_map_with_gain entries.
    if let Some(ref map) = tj.channel_map_with_gain {
        if let Some(ch) = tj.channels {
            if map.len() != ch as usize {
                bail!(
                    "{context}: channel_map_with_gain has {} rows but channels is {}",
                    map.len(),
                    ch
                );
            }
        }
        for (row_idx, row) in map.iter().enumerate() {
            for (col_idx, pair) in row.iter().enumerate() {
                let ch_idx = pair[0];
                let gain = pair[1];
                if ch_idx < 0.0 || ch_idx != ch_idx.floor() {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][0] must be \
                         a non-negative integer channel index, got {ch_idx}"
                    );
                }
                if ch_idx as u8 >= in_channels {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}] references \
                         input channel {} but input has only {in_channels} channels",
                        ch_idx as u8
                    );
                }
                if gain < 0.0 {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][1] gain must \
                         be non-negative, got {gain}"
                    );
                }
                if gain > 10.0 {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][1] gain must \
                         be at most 10.0 (+20 dB), got {gain}"
                    );
                }
            }
        }
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

/// Validate a JSON `transcode` block without knowing the upstream input
/// format. Used on TS / RTMP / HLS / WebRTC outputs where the audio arrives
/// AAC-encoded inside the transport stream — the actual input rate and
/// channel count are reported by the decoder at runtime, so input-relative
/// checks (preset channel-count match, matrix index range) are deferred.
/// All range, enum, and mutual-exclusion checks still run here.
pub(crate) fn validate_transcode_block_deferred(
    tj: &crate::engine::audio_transcode::TranscodeJson,
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
    let sources = [
        tj.channel_map.is_some(),
        tj.channel_map_with_gain.is_some(),
        tj.channel_map_preset.is_some(),
    ];
    if sources.iter().filter(|&&v| v).count() > 1 {
        bail!(
            "{context}: channel_map, channel_map_with_gain, and channel_map_preset \
             are mutually exclusive — specify at most one"
        );
    }
    if let Some(ref map) = tj.channel_map {
        if let Some(ch) = tj.channels {
            if map.len() != ch as usize {
                bail!(
                    "{context}: channel_map has {} rows but channels is {}",
                    map.len(),
                    ch
                );
            }
        }
    }
    if let Some(ref map) = tj.channel_map_with_gain {
        if let Some(ch) = tj.channels {
            if map.len() != ch as usize {
                bail!(
                    "{context}: channel_map_with_gain has {} rows but channels is {}",
                    map.len(),
                    ch
                );
            }
        }
        for (row_idx, row) in map.iter().enumerate() {
            for (col_idx, pair) in row.iter().enumerate() {
                let ch_idx = pair[0];
                let gain = pair[1];
                if ch_idx < 0.0 || ch_idx != ch_idx.floor() {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][0] must be \
                         a non-negative integer channel index, got {ch_idx}"
                    );
                }
                if gain < 0.0 {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][1] gain must \
                         be non-negative, got {gain}"
                    );
                }
                if gain > 10.0 {
                    bail!(
                        "{context}: channel_map_with_gain[{row_idx}][{col_idx}][1] gain must \
                         be at most 10.0 (+20 dB), got {gain}"
                    );
                }
            }
        }
    }
    // Preset name validity against actual input channel count is deferred to
    // runtime (the preset → matrix resolver checks it there).
    Ok(())
}

fn validate_st2110_ancillary_input(c: &St2110AncillaryInputConfig) -> Result<()> {
    validate_socket_addr(&c.bind_addr, "ST 2110-40 input bind_addr")?;
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, "ST 2110-40 input interface_addr")?;
    }
    validate_ssm_source(&c.bind_addr, c.source_addr.as_deref(), "ST 2110-40 input")?;
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

fn validate_video_dims(width: u32, height: u32, fps_num: u32, fps_den: u32, ctx: &str) -> Result<()> {
    if !(64..=8192).contains(&width) || width % 2 != 0 {
        bail!("{ctx}: width must be 64..=8192 and even, got {width}");
    }
    if !(64..=8192).contains(&height) || height % 2 != 0 {
        bail!("{ctx}: height must be 64..=8192 and even, got {height}");
    }
    if fps_den == 0 {
        bail!("{ctx}: frame_rate_den must be > 0");
    }
    let fps = fps_num as f64 / fps_den as f64;
    if !(1.0..=240.0).contains(&fps) {
        bail!("{ctx}: frame_rate must be between 1 and 240 fps, got {fps:.3}");
    }
    Ok(())
}

fn validate_payload_budget(n: usize, ctx: &str) -> Result<()> {
    if !(512..=8952).contains(&n) {
        bail!("{ctx}: payload_budget must be 512..=8952, got {n}");
    }
    Ok(())
}

fn validate_st2110_video_input(c: &St2110VideoInputConfig) -> Result<()> {
    const LABEL: &str = "ST 2110-20 input";
    validate_socket_addr(&c.bind_addr, &format!("{LABEL} bind_addr"))?;
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, &format!("{LABEL} interface_addr"))?;
    }
    validate_ssm_source(&c.bind_addr, c.source_addr.as_deref(), LABEL)?;
    validate_video_dims(c.width, c.height, c.frame_rate_num, c.frame_rate_den, LABEL)?;
    validate_rtp_payload_type(c.payload_type, LABEL)?;
    validate_clock_domain(c.clock_domain, LABEL)?;
    if let Some(ref sources) = c.allowed_sources {
        for s in sources {
            validate_ip_addr(s, &format!("{LABEL} allowed_sources"))?;
        }
    }
    if let Some(rate) = c.max_bitrate_mbps {
        if rate <= 0.0 || rate > 100_000.0 {
            bail!("{LABEL}: max_bitrate_mbps must be > 0 and <= 100000, got {rate}");
        }
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.bind_addr, &format!("{LABEL} redundancy"))?;
    }
    validate_video_encode(&c.video_encode, LABEL)?;
    Ok(())
}

fn validate_st2110_video_output(c: &St2110VideoOutputConfig) -> Result<()> {
    let label = format!("ST 2110-20 output '{}'", c.id);
    validate_id(&c.id, "ST 2110-20 output")?;
    validate_name(&c.name, "ST 2110-20 output")?;
    validate_socket_addr(&c.dest_addr, &format!("{label} dest_addr"))?;
    if let Some(ref bind) = c.bind_addr {
        validate_socket_addr(bind, &format!("{label} bind_addr"))?;
    }
    if let Some(ref iface) = c.interface_addr {
        validate_ip_addr(iface, &format!("{label} interface_addr"))?;
    }
    validate_video_dims(c.width, c.height, c.frame_rate_num, c.frame_rate_den, &label)?;
    validate_rtp_payload_type(c.payload_type, &label)?;
    validate_clock_domain(c.clock_domain, &label)?;
    if c.dscp > 63 {
        bail!("{label}: DSCP must be 0-63, got {}", c.dscp);
    }
    if let Some(ref red) = c.redundancy {
        validate_red_blue_bind(red, &c.dest_addr, &format!("{label} redundancy"))?;
    }
    validate_payload_budget(c.payload_budget, &label)?;
    Ok(())
}

fn validate_st2110_23_input(c: &St2110_23InputConfig) -> Result<()> {
    const LABEL: &str = "ST 2110-23 input";
    if !(2..=16).contains(&c.sub_streams.len()) {
        bail!("{LABEL}: sub_streams must have 2..=16 entries, got {}", c.sub_streams.len());
    }
    for (i, s) in c.sub_streams.iter().enumerate() {
        validate_socket_addr(&s.bind_addr, &format!("{LABEL} sub_streams[{i}] bind_addr"))?;
        if let Some(ref iface) = s.interface_addr {
            validate_ip_addr(iface, &format!("{LABEL} sub_streams[{i}] interface_addr"))?;
        }
        validate_ssm_source(
            &s.bind_addr,
            s.source_addr.as_deref(),
            &format!("{LABEL} sub_streams[{i}]"),
        )?;
        validate_rtp_payload_type(s.payload_type, &format!("{LABEL} sub_streams[{i}]"))?;
        if let Some(ref red) = s.redundancy {
            validate_red_blue_bind(red, &s.bind_addr, &format!("{LABEL} sub_streams[{i}] redundancy"))?;
        }
    }
    validate_video_dims(c.width, c.height, c.frame_rate_num, c.frame_rate_den, LABEL)?;
    validate_clock_domain(c.clock_domain, LABEL)?;
    validate_video_encode(&c.video_encode, LABEL)?;
    Ok(())
}

fn validate_st2110_23_output(c: &St2110_23OutputConfig) -> Result<()> {
    let label = format!("ST 2110-23 output '{}'", c.id);
    validate_id(&c.id, "ST 2110-23 output")?;
    validate_name(&c.name, "ST 2110-23 output")?;
    if !(2..=16).contains(&c.sub_streams.len()) {
        bail!("{label}: sub_streams must have 2..=16 entries, got {}", c.sub_streams.len());
    }
    for (i, s) in c.sub_streams.iter().enumerate() {
        validate_socket_addr(&s.dest_addr, &format!("{label} sub_streams[{i}] dest_addr"))?;
        if let Some(ref bind) = s.bind_addr {
            validate_socket_addr(bind, &format!("{label} sub_streams[{i}] bind_addr"))?;
        }
        if let Some(ref iface) = s.interface_addr {
            validate_ip_addr(iface, &format!("{label} sub_streams[{i}] interface_addr"))?;
        }
        validate_rtp_payload_type(s.payload_type, &format!("{label} sub_streams[{i}]"))?;
        if let Some(ref red) = s.redundancy {
            validate_red_blue_bind(red, &s.dest_addr, &format!("{label} sub_streams[{i}] redundancy"))?;
        }
    }
    validate_video_dims(c.width, c.height, c.frame_rate_num, c.frame_rate_den, &label)?;
    validate_clock_domain(c.clock_domain, &label)?;
    if c.dscp > 63 {
        bail!("{label}: DSCP must be 0-63, got {}", c.dscp);
    }
    validate_payload_budget(c.payload_budget, &label)?;
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
    validate_ssm_source(&c.bind_addr, c.source_addr.as_deref(), "rtp_audio input")?;
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
    if let Some(ref tj) = c.transcode {
        validate_transcode_block(
            tj,
            c.sample_rate,
            c.bit_depth,
            c.channels,
            "rtp_audio input transcode",
        )?;
    }
    if let Some(ref ae) = c.audio_encode {
        validate_audio_encode(ae, PCM_INPUT_AUDIO_CODECS, "rtp_audio input")?;
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
        // See st2110_audio_output: when the upstream input is known via the
        // flow context, validate the matrix shape against the *real* input
        // channel count. Otherwise defer the input-shape-dependent checks
        // until flow resolution — falling back to the output's own channel
        // count silently rejects valid configs (e.g. stereo→mono presets).
        if let Some((in_sr, in_bd, in_ch)) = upstream_audio {
            validate_transcode_block(
                tj,
                in_sr,
                in_bd,
                in_ch,
                &format!("rtp_audio output '{}' transcode", c.id),
            )?;
        } else {
            validate_transcode_block_deferred(
                tj,
                &format!("rtp_audio output '{}' transcode", c.id),
            )?;
        }
    }
    if let Some(idx) = c.audio_track_index {
        if idx > 15 {
            bail!(
                "rtp_audio output '{}': audio_track_index must be 0-15, got {}",
                c.id,
                idx
            );
        }
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
    validate_ssm_source(&red.addr, red.source_addr.as_deref(), context)?;
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

/// Validate the optional per-flow `assembly` block. The schema captures
/// the PID-bus plan; the runtime that consumes it lands in later phases.
/// Rules enforced here:
///
/// - `kind = Spts`: exactly one program.
/// - `kind = Mpts`: one or more programs with distinct `program_number`s
///   and distinct `pmt_pid`s.
/// - `kind = Passthrough`: `programs` must be empty and `pcr_source` must
///   be absent — passthrough is a runtime hint, not a muxer plan.
/// - Every referenced `input_id` must appear in `flow.input_ids`.
/// - PIDs (`pmt_pid`, `out_pid`) must be in 0x0010..=0x1FFE.
/// - Within each program, `out_pid`s are unique and none equal the
///   program's `pmt_pid`.
/// - `program_number > 0` (NIT is reserved).
/// - `service_name` length capped at 128 chars (DVB 64-byte limit
///   leaves headroom for UTF-8).
/// - `SlotSource::Hitless` cannot nest another `Hitless`.
/// - When `kind != Passthrough`, `pcr_source` is required and its
///   `input_id` must also be on the flow.
fn validate_flow_assembly(
    assembly: &crate::config::models::FlowAssembly,
    flow_input_ids: &[String],
    context: &str,
) -> Result<()> {
    use crate::config::models::AssemblyKind;

    let input_ids: std::collections::HashSet<&str> =
        flow_input_ids.iter().map(|s| s.as_str()).collect();

    let check_input = |iid: &str| -> Result<()> {
        if !input_ids.contains(iid) {
            bail!(
                "{context}: assembly references input_id '{iid}' not present in flow.input_ids"
            );
        }
        Ok(())
    };

    let check_pid = |pid: u16, what: &str| -> Result<()> {
        if !(0x0010..=0x1FFE).contains(&pid) {
            bail!(
                "{context}: {what} PID 0x{pid:04X} out of range; must be 0x0010..=0x1FFE"
            );
        }
        Ok(())
    };

    match assembly.kind {
        AssemblyKind::Passthrough => {
            if !assembly.programs.is_empty() {
                bail!(
                    "{context}: assembly.kind = passthrough must have empty programs"
                );
            }
            if assembly.pcr_source.is_some() {
                bail!(
                    "{context}: assembly.kind = passthrough must not carry pcr_source"
                );
            }
            return Ok(());
        }
        AssemblyKind::Spts => {
            if assembly.programs.len() != 1 {
                bail!(
                    "{context}: assembly.kind = spts requires exactly one program (got {})",
                    assembly.programs.len()
                );
            }
        }
        AssemblyKind::Mpts => {
            if assembly.programs.is_empty() {
                bail!(
                    "{context}: assembly.kind = mpts requires at least one program"
                );
            }
        }
    }

    // PCR rules differ between SPTS and MPTS:
    //   - SPTS: exactly one program. The flow-level `pcr_source`
    //     *or* the single program's `pcr_source` must be set. If both,
    //     per-program wins. The cross-check that the chosen
    //     `(input_id, pid)` hits a slot is deferred to the later loop.
    //   - MPTS: every program must have an effective `pcr_source`,
    //     taken from its own field when set, otherwise from the
    //     flow-level fallback. Any program without either is rejected
    //     here with `pid_bus_mpts_pcr_source_required`.
    // The flow-level `pcr_source` is validated shape-wise when set; it
    // is no longer mandatory at the assembly level.
    if let Some(pcr) = assembly.pcr_source.as_ref() {
        check_input(&pcr.input_id)?;
        check_pid(pcr.pid, "pcr_source")?;
    }
    match assembly.kind {
        AssemblyKind::Spts => {
            let has_flow_pcr = assembly.pcr_source.is_some();
            let has_prog_pcr = assembly
                .programs
                .first()
                .and_then(|p| p.pcr_source.as_ref())
                .is_some();
            if !has_flow_pcr && !has_prog_pcr {
                bail!(
                    "{context}: pcr_source is required for spts assembly (set either \
                     FlowAssembly.pcr_source or AssembledProgram.pcr_source)"
                );
            }
        }
        AssemblyKind::Mpts => {
            for prog in &assembly.programs {
                let effective = prog.pcr_source.as_ref().or(assembly.pcr_source.as_ref());
                if effective.is_none() {
                    bail!(
                        "{context}: mpts program {} has no pcr_source and no flow-level \
                         fallback — every MPTS program must name a PCR_PID within its \
                         own ES set",
                        prog.program_number
                    );
                }
            }
        }
        AssemblyKind::Passthrough => {}
    }

    // Collect program-level uniqueness + recurse into slots. Slot
    // resolution tables are built **per-program** so the per-program
    // pcr_source cross-check respects the H.222.0 rule that a
    // program's PCR_PID must reside in that program's own ES set.
    let mut seen_pn = std::collections::HashSet::new();
    let mut seen_pmt = std::collections::HashSet::new();
    for prog in &assembly.programs {
        // Per-program slot-source tables (concrete + essence-input)
        // for the post-loop pcr_source cross-check.
        let mut prog_slot_pids: std::collections::HashSet<(&str, u16)> =
            std::collections::HashSet::new();
        let mut prog_slot_essence_inputs: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        if prog.program_number == 0 {
            bail!(
                "{context}: program_number must be > 0 (0 is reserved for the NIT)"
            );
        }
        if !seen_pn.insert(prog.program_number) {
            bail!(
                "{context}: duplicate program_number {} in assembly",
                prog.program_number
            );
        }
        check_pid(prog.pmt_pid, "pmt_pid")?;
        if !seen_pmt.insert(prog.pmt_pid) {
            bail!(
                "{context}: duplicate pmt_pid 0x{:04X} across programs",
                prog.pmt_pid
            );
        }
        if let Some(ref name) = prog.service_name {
            if name.len() > 128 {
                bail!(
                    "{context}: program {} service_name exceeds 128 chars",
                    prog.program_number
                );
            }
        }

        let mut seen_out = std::collections::HashSet::new();
        for stream in &prog.streams {
            check_pid(stream.out_pid, "stream.out_pid")?;
            if stream.out_pid == prog.pmt_pid {
                bail!(
                    "{context}: stream.out_pid 0x{:04X} collides with pmt_pid of its own program",
                    stream.out_pid
                );
            }
            if !seen_out.insert(stream.out_pid) {
                bail!(
                    "{context}: duplicate stream.out_pid 0x{:04X} within program {}",
                    stream.out_pid,
                    prog.program_number
                );
            }
            if let Some(ref label) = stream.label {
                if label.len() > 256 {
                    bail!(
                        "{context}: stream label exceeds 256 chars"
                    );
                }
            }
            validate_slot_source(&stream.source, &check_input, &check_pid, context, false)?;
            collect_slot_ref(
                &stream.source,
                &mut prog_slot_pids,
                &mut prog_slot_essence_inputs,
            );
        }

        // Per-program pcr_source cross-check. Effective PCR =
        // program-level override or flow-level fallback. For SPTS or
        // Passthrough without a configured PCR this is a no-op.
        let effective_pcr = prog.pcr_source.as_ref().or(assembly.pcr_source.as_ref());
        if let Some(pcr) = effective_pcr {
            // `pcr` may have been validated shape-wise already (flow-
            // level) but the program-level field also needs validation.
            check_input(&pcr.input_id)?;
            check_pid(pcr.pid, "pcr_source")?;
            let concrete_hit = prog_slot_pids.contains(&(pcr.input_id.as_str(), pcr.pid));
            let essence_hit = prog_slot_essence_inputs.contains(pcr.input_id.as_str());
            if !concrete_hit && !essence_hit {
                bail!(
                    "{context}: pcr_source ({}, 0x{:04X}) does not resolve to any slot in program {}",
                    pcr.input_id,
                    pcr.pid,
                    prog.program_number,
                );
            }
        }
    }

    Ok(())
}

fn collect_slot_ref<'a>(
    src: &'a crate::config::models::SlotSource,
    pids: &mut std::collections::HashSet<(&'a str, u16)>,
    essence_inputs: &mut std::collections::HashSet<&'a str>,
) {
    use crate::config::models::SlotSource;
    match src {
        SlotSource::Pid { input_id, source_pid } => {
            pids.insert((input_id.as_str(), *source_pid));
        }
        SlotSource::Essence { input_id, .. } => {
            essence_inputs.insert(input_id.as_str());
        }
        SlotSource::Hitless { primary, backup } => {
            collect_slot_ref(primary, pids, essence_inputs);
            collect_slot_ref(backup, pids, essence_inputs);
        }
    }
}

fn validate_slot_source<F, G>(
    src: &crate::config::models::SlotSource,
    check_input: &F,
    check_pid: &G,
    context: &str,
    inside_hitless: bool,
) -> Result<()>
where
    F: Fn(&str) -> Result<()>,
    G: Fn(u16, &str) -> Result<()>,
{
    use crate::config::models::SlotSource;
    match src {
        SlotSource::Pid { input_id, source_pid } => {
            check_input(input_id)?;
            check_pid(*source_pid, "slot.source_pid")?;
        }
        SlotSource::Essence { input_id, kind: _ } => {
            check_input(input_id)?;
        }
        SlotSource::Hitless { primary, backup } => {
            if inside_hitless {
                bail!(
                    "{context}: hitless slot source cannot nest another hitless"
                );
            }
            validate_slot_source(primary, check_input, check_pid, context, true)?;
            validate_slot_source(backup, check_input, check_pid, context, true)?;
        }
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

/// Validate an optional MPEG-TS PID remap table. Keys and values must be
/// real user-range PIDs (0x0010–0x1FFE — reserved slots 0x0000..=0x000F,
/// PAT/CAT, and the NULL PID 0x1FFF are refused). No two source PIDs may
/// map to the same target. Target PIDs must be unique across the map too,
/// which together with the uniqueness-of-source property guarantees that
/// two different sources can never collide on the wire. A source mapping
/// to itself is refused as a pointless no-op that obscures intent.
fn validate_pid_map(
    map: Option<&std::collections::BTreeMap<u16, u16>>,
    context: &str,
) -> Result<()> {
    let Some(map) = map else {
        return Ok(());
    };
    if map.len() > 256 {
        bail!("{context}: pid_map has {} entries; at most 256 allowed", map.len());
    }
    let mut seen_targets: std::collections::HashSet<u16> =
        std::collections::HashSet::with_capacity(map.len());
    for (src, dst) in map {
        if *src < 0x0010 || *src > 0x1FFE {
            bail!(
                "{context}: pid_map source PID 0x{src:04X} out of range; must be in 0x0010..=0x1FFE"
            );
        }
        if *dst < 0x0010 || *dst > 0x1FFE {
            bail!(
                "{context}: pid_map target PID 0x{dst:04X} out of range; must be in 0x0010..=0x1FFE"
            );
        }
        if *src == *dst {
            bail!(
                "{context}: pid_map entry 0x{src:04X} -> 0x{dst:04X} is a no-op (source equals target)"
            );
        }
        if !seen_targets.insert(*dst) {
            bail!(
                "{context}: pid_map target PID 0x{dst:04X} used more than once (targets must be unique)"
            );
        }
    }
    Ok(())
}

/// Validate a `video_encode` block. The backend allowlist is
/// output-specific (future work — for now every TS-transport output
/// accepts the same set: x264/x265/nvenc variants). Validation here
/// enforces schema-level bounds (codec name, dimensions, fps, bitrate,
/// gop, preset, profile) regardless of which backends the current build
/// actually has compiled in. Unavailable backends are surfaced as a
/// runtime error when the output starts.
fn validate_video_encode(
    enc: &crate::config::models::VideoEncodeConfig,
    context: &str,
) -> anyhow::Result<()> {
    match enc.codec.as_str() {
        "x264" | "x265" | "h264_nvenc" | "hevc_nvenc" | "h264_qsv" | "hevc_qsv" => {}
        other => bail!(
            "{context}: video_encode.codec '{other}' is not recognised; \
             expected one of x264, x265, h264_nvenc, hevc_nvenc, h264_qsv, hevc_qsv"
        ),
    }
    // Reject codecs whose backend wasn't compiled into this build. Runtime
    // would surface a Critical event and passthrough the video anyway, but
    // that's confusing UX — catch it here so the manager's command_ack
    // carries a human-readable "rebuild with ..." message straight into
    // the modal error banner.
    let backend_feature: Option<&'static str> = match enc.codec.as_str() {
        "x264" => {
            if cfg!(feature = "video-encoder-x264") { None } else { Some("video-encoder-x264") }
        }
        "x265" => {
            if cfg!(feature = "video-encoder-x265") { None } else { Some("video-encoder-x265") }
        }
        "h264_nvenc" | "hevc_nvenc" => {
            if cfg!(feature = "video-encoder-nvenc") { None } else { Some("video-encoder-nvenc") }
        }
        "h264_qsv" | "hevc_qsv" => {
            if cfg!(feature = "video-encoder-qsv") { None } else { Some("video-encoder-qsv") }
        }
        _ => None,
    };
    if let Some(feat) = backend_feature {
        bail!(
            "{context}: video_encode.codec '{codec}' requires this edge to be built \
             with the `{feat}` Cargo feature; rebuild the edge or pick a codec whose \
             backend is compiled in",
            codec = enc.codec,
        );
    }
    if let Some(w) = enc.width {
        if !(64..=7680).contains(&w) {
            bail!("{context}: video_encode.width must be 64..=7680, got {w}");
        }
    }
    if let Some(h) = enc.height {
        if !(64..=4320).contains(&h) {
            bail!("{context}: video_encode.height must be 64..=4320, got {h}");
        }
    }
    match (enc.fps_num, enc.fps_den) {
        (Some(_), None) | (None, Some(_)) => bail!(
            "{context}: video_encode.fps_num and fps_den must be set together"
        ),
        (Some(n), Some(d)) => {
            if n == 0 || d == 0 {
                bail!(
                    "{context}: video_encode.fps_num and fps_den must be non-zero (got {n}/{d})"
                );
            }
            // 1 fps up to 240 fps is the useful range for broadcast.
            let ratio = n as f64 / d as f64;
            if !(0.5..=240.0).contains(&ratio) {
                bail!("{context}: video_encode effective frame rate {ratio:.3} is out of range (0.5..=240)");
            }
        }
        (None, None) => {}
    }
    if let Some(b) = enc.bitrate_kbps {
        if !(100..=100_000).contains(&b) {
            bail!("{context}: video_encode.bitrate_kbps must be 100..=100000, got {b}");
        }
    }
    if let Some(g) = enc.gop_size {
        if !(1..=600).contains(&g) {
            bail!("{context}: video_encode.gop_size must be 1..=600, got {g}");
        }
    }
    if let Some(ref p) = enc.preset {
        match p.as_str() {
            "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "medium"
            | "slow" | "slower" | "veryslow" => {}
            other => bail!(
                "{context}: video_encode.preset '{other}' is not recognised; \
                 expected one of ultrafast, superfast, veryfast, faster, fast, \
                 medium, slow, slower, veryslow"
            ),
        }
    }
    if let Some(ref pr) = enc.profile {
        match pr.as_str() {
            "baseline" | "main" | "high" | "high10" | "high422" | "high444" | "main10" => {}
            other => bail!(
                "{context}: video_encode.profile '{other}' is not recognised; \
                 expected one of baseline, main, high, high10, high422, high444, main10"
            ),
        }
    }
    // ── chroma / bit depth ────────────────────────────────────────────
    let chroma_str: Option<&str> = enc.chroma.as_deref();
    if let Some(c) = chroma_str {
        match c {
            "yuv420p" | "yuv422p" | "yuv444p" => {}
            other => bail!(
                "{context}: video_encode.chroma '{other}' is not recognised; \
                 expected one of yuv420p, yuv422p, yuv444p"
            ),
        }
    }
    if let Some(bd) = enc.bit_depth {
        if bd != 8 && bd != 10 {
            bail!(
                "{context}: video_encode.bit_depth must be 8 or 10, got {bd}"
            );
        }
    }
    // NVENC backend restrictions — catch at validation time rather than
    // at encoder-open.
    match (enc.codec.as_str(), chroma_str, enc.bit_depth) {
        ("h264_nvenc", _, Some(10)) => bail!(
            "{context}: h264_nvenc does not support 10-bit encoding; \
             use hevc_nvenc, x264, or x265 instead"
        ),
        ("h264_nvenc" | "hevc_nvenc", Some("yuv444p"), _) => bail!(
            "{context}: NVENC backends do not support chroma=yuv444p; \
             use x264 or x265 instead"
        ),
        _ => {}
    }
    // QSV backend restrictions — same shape as NVENC. h264_qsv is 8-bit
    // only (use hevc_qsv for 10-bit on supported Intel hardware), and
    // neither QSV variant supports 4:4:4 chroma in oneVPL today.
    match (enc.codec.as_str(), chroma_str, enc.bit_depth) {
        ("h264_qsv", _, Some(10)) => bail!(
            "{context}: h264_qsv does not support 10-bit encoding; \
             use hevc_qsv, x264, or x265 instead"
        ),
        ("h264_qsv" | "hevc_qsv", Some("yuv444p"), _) => bail!(
            "{context}: QSV backends do not support chroma=yuv444p; \
             use x264 or x265 instead"
        ),
        _ => {}
    }
    // Profile ↔ chroma/bit_depth compatibility.
    match (
        enc.profile.as_deref(),
        chroma_str.unwrap_or("yuv420p"),
        enc.bit_depth.unwrap_or(8),
    ) {
        (Some("high10"), _, 8) => bail!(
            "{context}: video_encode.profile=high10 requires bit_depth=10"
        ),
        (Some("main10"), _, 8) => bail!(
            "{context}: video_encode.profile=main10 requires bit_depth=10"
        ),
        (Some("high422"), "yuv420p", _) | (Some("high422"), "yuv444p", _) => bail!(
            "{context}: video_encode.profile=high422 requires chroma=yuv422p"
        ),
        (Some("high444"), "yuv420p", _) | (Some("high444"), "yuv422p", _) => bail!(
            "{context}: video_encode.profile=high444 requires chroma=yuv444p"
        ),
        _ => {}
    }
    // ── rate control ──────────────────────────────────────────────────
    if let Some(ref rc) = enc.rate_control {
        match rc.as_str() {
            "vbr" | "cbr" | "crf" | "abr" => {}
            other => bail!(
                "{context}: video_encode.rate_control '{other}' is not recognised; \
                 expected one of vbr, cbr, crf, abr"
            ),
        }
    }
    if let Some(crf) = enc.crf {
        if crf > 51 {
            bail!(
                "{context}: video_encode.crf must be 0..=51, got {crf}"
            );
        }
    }
    if let Some(mb) = enc.max_bitrate_kbps {
        if !(100..=100_000).contains(&mb) {
            bail!(
                "{context}: video_encode.max_bitrate_kbps must be 100..=100000, got {mb}"
            );
        }
        if let Some(br) = enc.bitrate_kbps {
            if mb < br {
                bail!(
                    "{context}: video_encode.max_bitrate_kbps ({mb}) must be >= bitrate_kbps ({br})"
                );
            }
        }
    }
    // ── frame structure ───────────────────────────────────────────────
    if let Some(bf) = enc.bframes {
        if bf > 16 {
            bail!("{context}: video_encode.bframes must be 0..=16, got {bf}");
        }
    }
    if let Some(rf) = enc.refs {
        if !(1..=16).contains(&rf) {
            bail!("{context}: video_encode.refs must be 1..=16, got {rf}");
        }
    }
    if let Some(ref lv) = enc.level {
        if lv.len() > 8 {
            bail!(
                "{context}: video_encode.level string too long (max 8 chars)"
            );
        }
        if !lv.chars().all(|c| c.is_ascii_digit() || c == '.') {
            bail!(
                "{context}: video_encode.level must contain only digits and '.', got '{lv}'"
            );
        }
    }
    if let Some(ref t) = enc.tune {
        match t.as_str() {
            // Empty string = unset (encoder default).
            "" | "zerolatency" | "film" | "animation" | "grain" | "stillimage"
            | "fastdecode" | "psnr" | "ssim" => {}
            other => bail!(
                "{context}: video_encode.tune '{other}' is not recognised; \
                 expected one of (empty), zerolatency, film, animation, grain, \
                 stillimage, fastdecode, psnr, ssim"
            ),
        }
    }
    // ── colour metadata ───────────────────────────────────────────────
    if let Some(ref p) = enc.color_primaries {
        match p.as_str() {
            "" | "bt709" | "bt2020" | "smpte170m" | "smpte240m" | "bt470m" | "bt470bg" => {}
            other => bail!(
                "{context}: video_encode.color_primaries '{other}' is not recognised"
            ),
        }
    }
    if let Some(ref t) = enc.color_transfer {
        match t.as_str() {
            "" | "bt709" | "smpte170m" | "smpte2084" | "pq" | "arib-std-b67" | "hlg"
            | "bt2020-10" | "bt2020-12" => {}
            other => bail!(
                "{context}: video_encode.color_transfer '{other}' is not recognised"
            ),
        }
    }
    if let Some(ref m) = enc.color_matrix {
        match m.as_str() {
            "" | "bt709" | "bt2020nc" | "bt2020c" | "smpte170m" | "smpte240m" => {}
            other => bail!(
                "{context}: video_encode.color_matrix '{other}' is not recognised"
            ),
        }
    }
    if let Some(ref r) = enc.color_range {
        match r.as_str() {
            "" | "tv" | "limited" | "mpeg" | "pc" | "full" | "jpeg" => {}
            other => bail!(
                "{context}: video_encode.color_range '{other}' is not recognised; \
                 expected one of tv, pc"
            ),
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

    // SMPTE 302M is a lossless wrap, not an encoder — it has a different
    // field set than the compressed codecs and must be checked apart.
    if enc.codec == "s302m" {
        if enc.bitrate_kbps.is_some() {
            bail!(
                "{context}: audio_encode.bitrate_kbps is not applicable to s302m \
                 (302M is a lossless PCM wrap, not a compressed codec)"
            );
        }
        if let Some(sr) = enc.sample_rate {
            if sr != 48_000 {
                bail!(
                    "{context}: audio_encode.sample_rate must be 48000 for s302m, got {sr}"
                );
            }
        }
        if let Some(ch) = enc.channels {
            if !matches!(ch, 2 | 4 | 6 | 8) {
                bail!(
                    "{context}: audio_encode.channels must be 2, 4, 6, or 8 for s302m \
                     (SMPTE 302M-2007), got {ch}"
                );
            }
        }
        return Ok(());
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

/// TS-carrying inputs accept the same audio codec set as TS outputs — Opus
/// has no standard MPEG-TS mapping, so it is excluded.
const TS_INPUT_AUDIO_CODECS: &[&str] = &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"];

/// Codecs a PCM-only input (ST 2110-30, `rtp_audio`) may set on
/// `audio_encode` to convert its PCM-RTP into MPEG-TS. Same set as
/// [`TS_INPUT_AUDIO_CODECS`] plus `s302m` — SMPTE 302M-in-TS is a
/// lossless PCM-preserving wrap, not really an "encoder", but it shares
/// the same `audio_encode` config slot because the semantics (PCM in →
/// TS out) are identical from the operator's point of view.
const PCM_INPUT_AUDIO_CODECS: &[&str] =
    &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3", "s302m"];

/// ST 2110-31 (AES3 transparent) carries SMPTE 337M sub-frames that cannot
/// be decoded to linear PCM without a 337M parser. The only valid
/// `audio_encode` on -31 is `s302m`, which wraps the AES3 bytes in a
/// SMPTE 302M private PES bit-for-bit — no decode, no re-encode.
const AES3_INPUT_AUDIO_CODECS: &[&str] = &["s302m"];

/// Apply the standard Group-A (TS-carrying input) transcoding validation:
/// video_encode + audio_encode + transcode. All three are optional; any
/// combination may be set or unset. The upstream audio shape is unknown at
/// config-load time (codec and channel count come from the live stream), so
/// transcode validation uses the deferred variant.
fn validate_input_transcode_group_a(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    transcode: Option<&crate::engine::audio_transcode::TranscodeJson>,
    video_encode: Option<&crate::config::models::VideoEncodeConfig>,
    label: &str,
) -> Result<()> {
    if let Some(ve) = video_encode {
        validate_video_encode(ve, label)?;
    }
    if let Some(ae) = audio_encode {
        validate_audio_encode(ae, TS_INPUT_AUDIO_CODECS, label)?;
    }
    if let Some(tj) = transcode {
        validate_transcode_block_deferred(tj, &format!("{label} transcode"))?;
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

/// Validates the optional `group` tag on an output. Returns an error if the
/// group string exceeds 64 characters. Used by [`validate_output_with_input`].
fn validate_output_group(group: Option<&str>, output_id: &str) -> Result<()> {
    if let Some(g) = group {
        if g.len() > 64 {
            bail!(
                "Output '{}': group tag must be at most 64 characters",
                output_id
            );
        }
    }
    Ok(())
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
    validate_output_group(output.group(), output.id())?;
    match output {
        OutputConfig::Rtp(rtp) => {
            validate_id(&rtp.id, "RTP output")?;
            validate_name(&rtp.name, "RTP output")?;
            validate_program_number(rtp.program_number, &format!("RTP output '{}'", rtp.id))?;
            validate_pid_map(rtp.pid_map.as_ref(), &format!("RTP output '{}'", rtp.id))?;
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
            if let Some(ref enc) = rtp.audio_encode {
                if rtp.redundancy.is_some() {
                    bail!(
                        "RTP output '{}': audio_encode is not yet supported with SMPTE 2022-7 redundancy",
                        rtp.id
                    );
                }
                if rtp.fec_encode.is_some() {
                    bail!(
                        "RTP output '{}': audio_encode is not yet supported with SMPTE 2022-1 FEC encode",
                        rtp.id
                    );
                }
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"],
                    &format!("RTP output '{}'", rtp.id),
                )?;
            }
            if let Some(ref tj) = rtp.transcode {
                if rtp.audio_encode.is_none() {
                    bail!(
                        "RTP output '{}': transcode has no effect without audio_encode set",
                        rtp.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("RTP output '{}' transcode", rtp.id),
                )?;
            }
            if let Some(ref enc) = rtp.video_encode {
                if rtp.redundancy.is_some() {
                    bail!(
                        "RTP output '{}': video_encode is not yet supported with SMPTE 2022-7 redundancy",
                        rtp.id
                    );
                }
                if rtp.fec_encode.is_some() {
                    bail!(
                        "RTP output '{}': video_encode is not yet supported with SMPTE 2022-1 FEC encode",
                        rtp.id
                    );
                }
                validate_video_encode(enc, &format!("RTP output '{}'", rtp.id))?;
            }
        }
        OutputConfig::Udp(udp) => {
            validate_id(&udp.id, "UDP output")?;
            validate_name(&udp.name, "UDP output")?;
            validate_program_number(udp.program_number, &format!("UDP output '{}'", udp.id))?;
            validate_pid_map(udp.pid_map.as_ref(), &format!("UDP output '{}'", udp.id))?;
            if udp.pid_map.is_some() && udp.transport_mode.as_deref() == Some("audio_302m") {
                bail!(
                    "UDP output '{}': pid_map is incompatible with transport_mode 'audio_302m'",
                    udp.id
                );
            }
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
            if let Some(ref enc) = udp.audio_encode {
                if udp.transport_mode.as_deref() == Some("audio_302m") {
                    bail!(
                        "UDP output '{}': audio_encode is incompatible with transport_mode 'audio_302m'",
                        udp.id
                    );
                }
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"],
                    &format!("UDP output '{}'", udp.id),
                )?;
            }
            if let Some(ref tj) = udp.transcode {
                if udp.audio_encode.is_none() {
                    bail!(
                        "UDP output '{}': transcode has no effect without audio_encode set",
                        udp.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("UDP output '{}' transcode", udp.id),
                )?;
            }
            if let Some(ref enc) = udp.video_encode {
                if udp.transport_mode.as_deref() == Some("audio_302m") {
                    bail!(
                        "UDP output '{}': video_encode is incompatible with transport_mode 'audio_302m'",
                        udp.id
                    );
                }
                validate_video_encode(enc, &format!("UDP output '{}'", udp.id))?;
            }
        }
        OutputConfig::Srt(srt) => {
            validate_id(&srt.id, "SRT output")?;
            validate_name(&srt.name, "SRT output")?;
            validate_program_number(srt.program_number, &format!("SRT output '{}'", srt.id))?;
            validate_pid_map(srt.pid_map.as_ref(), &format!("SRT output '{}'", srt.id))?;
            if srt.pid_map.is_some() && srt.transport_mode.as_deref() == Some("audio_302m") {
                bail!(
                    "SRT output '{}': pid_map is incompatible with transport_mode 'audio_302m'",
                    srt.id
                );
            }
            match srt.mode {
                SrtMode::Listener | SrtMode::Rendezvous => {
                    let addr = srt.local_addr.as_deref()
                        .ok_or_else(|| anyhow::anyhow!("SRT output local_addr is required for {:?} mode", srt.mode))?;
                    validate_socket_addr(addr, "SRT output local_addr")?;
                }
                SrtMode::Caller => {
                    if let Some(ref addr) = srt.local_addr {
                        validate_socket_addr(addr, "SRT output local_addr")?;
                    }
                }
            }
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
            if srt.redundancy.is_some() && srt.bonding.is_some() {
                bail!(
                    "SRT output '{}': `redundancy` and `bonding` are mutually exclusive (bonding is libsrt-native SMPTE 2022-7 / primary-backup; redundancy is the app-layer fallback)",
                    srt.id
                );
            }
            if let Some(ref red) = srt.redundancy {
                validate_srt_redundancy(red, "SRT output")?;
            }
            if let Some(ref bond) = srt.bonding {
                validate_srt_bonding(bond, &srt.mode, &format!("SRT output '{}'", srt.id))?;
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
            if let Some(ref enc) = srt.audio_encode {
                if srt.transport_mode.as_deref() == Some("audio_302m") {
                    bail!(
                        "SRT output '{}': audio_encode is incompatible with transport_mode 'audio_302m'",
                        srt.id
                    );
                }
                if srt.redundancy.is_some() {
                    bail!(
                        "SRT output '{}': audio_encode is not yet supported with SMPTE 2022-7 redundancy",
                        srt.id
                    );
                }
                if srt.packet_filter.is_some() {
                    bail!(
                        "SRT output '{}': audio_encode is not yet supported with SRT FEC (packet_filter)",
                        srt.id
                    );
                }
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"],
                    &format!("SRT output '{}'", srt.id),
                )?;
            }
            if let Some(ref tj) = srt.transcode {
                if srt.audio_encode.is_none() {
                    bail!(
                        "SRT output '{}': transcode has no effect without audio_encode set",
                        srt.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("SRT output '{}' transcode", srt.id),
                )?;
            }
            if let Some(ref enc) = srt.video_encode {
                if srt.transport_mode.as_deref() == Some("audio_302m") {
                    bail!(
                        "SRT output '{}': video_encode is incompatible with transport_mode 'audio_302m'",
                        srt.id
                    );
                }
                if srt.redundancy.is_some() {
                    bail!(
                        "SRT output '{}': video_encode is not yet supported with SMPTE 2022-7 redundancy",
                        srt.id
                    );
                }
                if srt.packet_filter.is_some() {
                    bail!(
                        "SRT output '{}': video_encode is not yet supported with SRT FEC (packet_filter)",
                        srt.id
                    );
                }
                validate_video_encode(enc, &format!("SRT output '{}'", srt.id))?;
            }
        }
        OutputConfig::Rist(rist) => {
            validate_rist_output(rist)?;
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
            ::url::Url::parse(&rtmp.dest_url).map_err(|e| {
                anyhow::anyhow!("RTMP output '{}': dest_url is malformed: {e}", rtmp.id)
            })?;
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
            if let Some(ref tj) = rtmp.transcode {
                if rtmp.audio_encode.is_none() {
                    bail!(
                        "RTMP output '{}': transcode has no effect without audio_encode set",
                        rtmp.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("RTMP output '{}' transcode", rtmp.id),
                )?;
            }
            if let Some(ref enc) = rtmp.video_encode {
                validate_video_encode(enc, &format!("RTMP output '{}'", rtmp.id))?;
            }
        }
        OutputConfig::Hls(hls) => {
            validate_id(&hls.id, "HLS output")?;
            validate_name(&hls.name, "HLS output")?;
            validate_program_number(hls.program_number, &format!("HLS output '{}'", hls.id))?;
            validate_pid_map(hls.pid_map.as_ref(), &format!("HLS output '{}'", hls.id))?;
            if !hls.ingest_url.starts_with("http://") && !hls.ingest_url.starts_with("https://") {
                bail!("HLS output '{}': ingest_url must start with http:// or https://", hls.id);
            }
            if hls.ingest_url.len() > 2048 {
                bail!("HLS output '{}': ingest_url must be at most 2048 characters", hls.id);
            }
            // HLS segments are pushed via raw HTTP PUT with the URL inlined into the
            // request line. CRLF or other ASCII control bytes would allow header
            // injection / request smuggling against the ingest server.
            if hls.ingest_url.chars().any(|c| c.is_ascii_control()) {
                bail!(
                    "HLS output '{}': ingest_url must not contain ASCII control characters",
                    hls.id
                );
            }
            ::url::Url::parse(&hls.ingest_url).map_err(|e| {
                anyhow::anyhow!("HLS output '{}': ingest_url is malformed: {e}", hls.id)
            })?;
            if let Some(ref token) = hls.auth_token {
                if token.len() > 4096 {
                    bail!("HLS output '{}': auth_token must be at most 4096 characters", hls.id);
                }
                // Bearer tokens are inlined into the Authorization header — reject
                // control or whitespace bytes that could inject extra headers.
                if token.chars().any(|c| c.is_ascii_control() || c.is_whitespace()) {
                    bail!(
                        "HLS output '{}': auth_token must not contain control or whitespace characters",
                        hls.id
                    );
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
            if let Some(ref tj) = hls.transcode {
                if hls.audio_encode.is_none() {
                    bail!(
                        "HLS output '{}': transcode has no effect without audio_encode set",
                        hls.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("HLS output '{}' transcode", hls.id),
                )?;
            }
        }
        OutputConfig::Cmaf(cmaf) => {
            fn validate_cenc_block(
                cenc: &crate::config::models::CencConfig,
                ctx: &str,
            ) -> anyhow::Result<()> {
                fn check_hex(s: &str, expected: usize, label: &str, ctx: &str) -> anyhow::Result<()> {
                    if s.len() != expected {
                        anyhow::bail!(
                            "{ctx}: encryption.{label} must be exactly {expected} hex characters, got {}",
                            s.len()
                        );
                    }
                    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
                        anyhow::bail!("{ctx}: encryption.{label} must be hex-only");
                    }
                    Ok(())
                }
                check_hex(&cenc.key_id, 32, "key_id", ctx)?;
                check_hex(&cenc.key, 32, "key", ctx)?;
                match cenc.scheme.as_str() {
                    "cenc" | "cbcs" => {}
                    _ => anyhow::bail!(
                        "{ctx}: encryption.scheme must be \"cenc\" or \"cbcs\", got \"{}\"",
                        cenc.scheme
                    ),
                }
                for (i, pssh_hex) in cenc.pssh_boxes.iter().enumerate() {
                    if pssh_hex.len() % 2 != 0
                        || !pssh_hex.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        anyhow::bail!("{ctx}: encryption.pssh_boxes[{i}] must be even-length hex");
                    }
                    let bytes_len = pssh_hex.len() / 2;
                    if bytes_len < 32 || bytes_len > 4096 {
                        anyhow::bail!(
                            "{ctx}: encryption.pssh_boxes[{i}] must decode to 32..=4096 bytes"
                        );
                    }
                    // Sanity-check fourcc at bytes 4..8.
                    let b4 = u8::from_str_radix(&pssh_hex[8..10], 16).unwrap_or(0);
                    let b5 = u8::from_str_radix(&pssh_hex[10..12], 16).unwrap_or(0);
                    let b6 = u8::from_str_radix(&pssh_hex[12..14], 16).unwrap_or(0);
                    let b7 = u8::from_str_radix(&pssh_hex[14..16], 16).unwrap_or(0);
                    if &[b4, b5, b6, b7] != b"pssh" {
                        anyhow::bail!(
                            "{ctx}: encryption.pssh_boxes[{i}] is not a valid pssh box (fourcc mismatch)"
                        );
                    }
                }
                Ok(())
            }
            validate_id(&cmaf.id, "CMAF output")?;
            validate_name(&cmaf.name, "CMAF output")?;
            validate_program_number(cmaf.program_number, &format!("CMAF output '{}'", cmaf.id))?;
            if !cmaf.ingest_url.starts_with("http://") && !cmaf.ingest_url.starts_with("https://") {
                bail!(
                    "CMAF output '{}': ingest_url must start with http:// or https://",
                    cmaf.id
                );
            }
            if cmaf.ingest_url.len() > 2048 {
                bail!(
                    "CMAF output '{}': ingest_url must be at most 2048 characters",
                    cmaf.id
                );
            }
            // The URL is inlined into the HTTP request — reject CRLF / other
            // control bytes that would permit header injection.
            if cmaf.ingest_url.chars().any(|c| c.is_ascii_control()) {
                bail!(
                    "CMAF output '{}': ingest_url must not contain ASCII control characters",
                    cmaf.id
                );
            }
            if let Some(ref token) = cmaf.auth_token {
                if token.len() > 4096 {
                    bail!(
                        "CMAF output '{}': auth_token must be at most 4096 characters",
                        cmaf.id
                    );
                }
                if token.chars().any(|c| c.is_ascii_control() || c.is_whitespace()) {
                    bail!(
                        "CMAF output '{}': auth_token must not contain control or whitespace characters",
                        cmaf.id
                    );
                }
            }
            if cmaf.segment_duration_secs < 1.0 || cmaf.segment_duration_secs > 10.0 {
                bail!(
                    "CMAF output '{}': segment_duration_secs must be 1.0-10.0, got {}",
                    cmaf.id, cmaf.segment_duration_secs
                );
            }
            if cmaf.max_segments == 0 || cmaf.max_segments > 30 {
                bail!(
                    "CMAF output '{}': max_segments must be 1-30, got {}",
                    cmaf.id, cmaf.max_segments
                );
            }
            if cmaf.manifests.is_empty() {
                bail!(
                    "CMAF output '{}': manifests must be a non-empty subset of [\"hls\",\"dash\"]",
                    cmaf.id
                );
            }
            let mut seen_manifests = std::collections::HashSet::new();
            for m in &cmaf.manifests {
                if !seen_manifests.insert(m.as_str()) {
                    bail!(
                        "CMAF output '{}': duplicate manifest entry '{}'",
                        cmaf.id, m
                    );
                }
                match m.as_str() {
                    "hls" | "dash" => {}
                    other => {
                        bail!(
                            "CMAF output '{}': manifest entry '{}' must be \"hls\" or \"dash\"",
                            cmaf.id, other
                        );
                    }
                }
            }
            if cmaf.low_latency
                && (cmaf.chunk_duration_ms < 100 || cmaf.chunk_duration_ms > 2000)
            {
                bail!(
                    "CMAF output '{}': chunk_duration_ms must be 100-2000 when low_latency is true, got {}",
                    cmaf.id, cmaf.chunk_duration_ms
                );
            }
            if let Some(ref cenc) = cmaf.encryption {
                validate_cenc_block(cenc, &format!("CMAF output '{}'", cmaf.id))?;
            }
            if let Some(ref enc) = cmaf.audio_encode {
                validate_audio_encode(
                    enc,
                    &["aac_lc", "he_aac_v1", "he_aac_v2"],
                    &format!("CMAF output '{}'", cmaf.id),
                )?;
            }
            if let Some(ref tj) = cmaf.transcode {
                if cmaf.audio_encode.is_none() {
                    bail!(
                        "CMAF output '{}': transcode has no effect without audio_encode set",
                        cmaf.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("CMAF output '{}' transcode", cmaf.id),
                )?;
            }
            if let Some(ref ve) = cmaf.video_encode {
                validate_video_encode(ve, &format!("CMAF output '{}'", cmaf.id))?;
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
            if let Some(ref fp) = webrtc.cert_fingerprint {
                crate::util::tls::canonicalise_fingerprint(fp).map_err(|e| {
                    anyhow::anyhow!("WebRTC output '{}': cert_fingerprint: {}", webrtc.id, e)
                })?;
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
            if let Some(ref tj) = webrtc.transcode {
                if webrtc.audio_encode.is_none() {
                    bail!(
                        "WebRTC output '{}': transcode has no effect without audio_encode set",
                        webrtc.id
                    );
                }
                validate_transcode_block_deferred(
                    tj,
                    &format!("WebRTC output '{}' transcode", webrtc.id),
                )?;
            }
            if let Some(ref ve) = webrtc.video_encode {
                // Reject HEVC encoders *before* the generic validator so
                // operators don't get a misleading "rebuild with x265"
                // suggestion — WebRTC browsers can't decode HEVC at all,
                // so rebuilding wouldn't help.
                match ve.codec.as_str() {
                    "x265" | "hevc_nvenc" | "hevc_qsv" => bail!(
                        "WebRTC output '{}': video_encode.codec '{}' is not supported — WebRTC browsers only decode H.264 (use 'x264', 'h264_nvenc', or 'h264_qsv')",
                        webrtc.id, ve.codec,
                    ),
                    _ => {}
                }
                validate_video_encode(
                    ve,
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
        OutputConfig::St2110_20(c) => validate_st2110_video_output(c)?,
        OutputConfig::St2110_23(c) => validate_st2110_23_output(c)?,
        OutputConfig::RtpAudio(c) => validate_rtp_audio_output(c, upstream_audio)?,
        OutputConfig::Bonded(c) => validate_bonded_output(c)?,
    }
    Ok(())
}

fn validate_bonded_output(c: &crate::config::models::BondedOutputConfig) -> Result<()> {
    if c.id.is_empty() || c.id.len() > 64 {
        return Err(anyhow::anyhow!(
            "bonded output id length must be in [1, 64]"
        ));
    }
    if c.name.len() > 256 {
        return Err(anyhow::anyhow!("bonded output name too long (>256 chars)"));
    }
    validate_output_group(c.group.as_deref(), &c.id)?;
    if c.paths.is_empty() {
        return Err(anyhow::anyhow!(
            "bonded output '{}': at least one path required",
            c.id
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for p in &c.paths {
        if !ids.insert(p.id) {
            return Err(anyhow::anyhow!(
                "bonded output '{}': duplicate path id {}",
                c.id,
                p.id
            ));
        }
        validate_bond_path_transport(&p.transport, /* sender_mode */ true, &p.name)?;
    }
    if let Some(pn) = c.program_number {
        if pn == 0 {
            return Err(anyhow::anyhow!(
                "bonded output '{}' program_number must be > 0",
                c.id
            ));
        }
    }
    validate_pid_map(c.pid_map.as_ref(), &format!("bonded output '{}'", c.id))?;
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
        // libsrt 1.5.5 does not support negotiating the `SRT_CMD_FILTER`
        // extension in rendezvous mode — both peers INDUCTION simultaneously
        // and neither is the leader that advertises filter settings first.
        // The handshake gets rejected with `ERROR:FILTER` and the connection
        // silently loops on retry. Reject the combination at config load so
        // operators don't deploy it unknowingly. (Observed in the 2026-04-20
        // SRT interop run scenario C8; see
        // `testbed/test_reports/srt_full_interop_2026-04-20/`.)
        if matches!(mode, SrtMode::Rendezvous) {
            bail!(
                "{context}: SRT packet_filter (FEC) is not supported in rendezvous mode — libsrt rejects the handshake. Use listener/caller pair instead."
            );
        }

        // FEC layered on top of encryption has a known ordering bug in the
        // pure-Rust SRT backend: parity is computed in a different order
        // than libsrt 1.5.5 expects, so C++ peers fail to recover. The
        // libsrt-rs backend routes through the vendored C library and is
        // interop-safe. Reject the combination only on the pure-Rust
        // backend so existing libsrt deployments are unaffected.
        if passphrase.is_some() && srt_transport::BACKEND_NAME == "pure-rust" {
            bail!(
                "{context}: SRT packet_filter (FEC) combined with passphrase (encryption) is not interop-safe on the pure-Rust SRT backend. \
                 Either remove the passphrase, remove the packet_filter, or switch to the libsrt backend (see bilbycast-edge/Cargo.toml `── SRT backend ──`)."
            );
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

fn validate_srt_bonding(bond: &SrtBondingConfig, mode: &SrtMode, context: &str) -> Result<()> {
    // Rendezvous mode has no socket-group wire handshake in libsrt.
    if matches!(mode, SrtMode::Rendezvous) {
        bail!("{context}: bonding is not supported in rendezvous mode");
    }
    let n = bond.endpoints.len();
    if n < 2 {
        bail!("{context}: bonding requires at least 2 endpoints, got {n}");
    }
    if n > 8 {
        bail!("{context}: bonding supports at most 8 endpoints, got {n}");
    }
    let mut seen = HashSet::new();
    for (i, ep) in bond.endpoints.iter().enumerate() {
        validate_socket_addr(&ep.addr, &format!("{context} bonding endpoint[{i}] addr"))?;
        if !seen.insert(ep.addr.clone()) {
            bail!("{context}: bonding endpoint[{i}] addr {} is a duplicate", ep.addr);
        }
        if let Some(ref la) = ep.local_addr {
            validate_socket_addr(la, &format!("{context} bonding endpoint[{i}] local_addr"))?;
            if matches!(mode, SrtMode::Listener) {
                bail!(
                    "{context}: bonding endpoint[{i}] local_addr is only valid in caller mode"
                );
            }
        }
        if let Some(w) = ep.weight {
            if matches!(bond.mode, SrtBondingMode::Broadcast) && w != 0 {
                bail!(
                    "{context}: bonding endpoint[{i}] weight is only meaningful in backup mode"
                );
            }
        }
    }
    Ok(())
}

fn validate_srt_redundancy(red: &SrtRedundancyConfig, context: &str) -> Result<()> {
    match red.mode {
        SrtMode::Listener | SrtMode::Rendezvous => {
            let addr = red.local_addr.as_deref()
                .ok_or_else(|| anyhow::anyhow!("{context} redundancy local_addr is required for {:?} mode", red.mode))?;
            validate_socket_addr(addr, &format!("{context} redundancy local_addr"))?;
        }
        SrtMode::Caller => {
            if let Some(ref addr) = red.local_addr {
                validate_socket_addr(addr, &format!("{context} redundancy local_addr"))?;
            }
        }
    }
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

/// Validates a RIST address: must parse as SocketAddr and the port must be
/// even (RIST uses port P for RTP and P+1 for RTCP).
fn validate_rist_addr(addr: &str, context: &str) -> Result<()> {
    validate_socket_addr(addr, context)?;
    let sa: SocketAddr = addr.parse().unwrap();
    if sa.port() != 0 && sa.port() % 2 != 0 {
        bail!(
            "{context}: RIST port must be even (RTCP binds on port+1), got {}",
            sa.port()
        );
    }
    Ok(())
}

fn validate_rist_common_knobs(
    buffer_ms: Option<u32>,
    max_nack_retries: Option<u32>,
    cname: Option<&str>,
    rtcp_interval_ms: Option<u32>,
    context: &str,
) -> Result<()> {
    if let Some(b) = buffer_ms {
        if !(50..=30_000).contains(&b) {
            bail!("{context}: buffer_ms must be 50-30000, got {b}");
        }
    }
    if let Some(r) = max_nack_retries {
        if r > 50 {
            bail!("{context}: max_nack_retries must be ≤ 50, got {r}");
        }
    }
    if let Some(c) = cname {
        if c.len() > 256 {
            bail!("{context}: cname must be at most 256 characters");
        }
    }
    if let Some(i) = rtcp_interval_ms {
        if i == 0 || i > 1000 {
            bail!("{context}: rtcp_interval_ms must be 1-1000 (TR-06-1 prefers ≤100), got {i}");
        }
    }
    Ok(())
}

fn validate_rist_input(rist: &RistInputConfig) -> Result<()> {
    validate_rist_addr(&rist.bind_addr, "RIST input bind_addr")?;
    validate_rist_common_knobs(
        rist.buffer_ms,
        rist.max_nack_retries,
        rist.cname.as_deref(),
        rist.rtcp_interval_ms,
        "RIST input",
    )?;
    if let Some(ref red) = rist.redundancy {
        validate_rist_addr(&red.bind_addr, "RIST input redundancy bind_addr")?;
        if red.bind_addr == rist.bind_addr {
            bail!("RIST input redundancy: leg 2 bind_addr must differ from leg 1");
        }
        let leg1: SocketAddr = rist.bind_addr.parse()?;
        let leg2: SocketAddr = red.bind_addr.parse()?;
        if leg1.is_ipv4() != leg2.is_ipv4() {
            bail!("RIST input redundancy: leg 1 and leg 2 must use the same address family");
        }
    }
    Ok(())
}

fn validate_rist_output(rist: &RistOutputConfig) -> Result<()> {
    validate_id(&rist.id, "RIST output")?;
    validate_name(&rist.name, "RIST output")?;
    validate_output_group(rist.group.as_deref(), &rist.id)?;
    validate_program_number(rist.program_number, &format!("RIST output '{}'", rist.id))?;
    validate_pid_map(rist.pid_map.as_ref(), &format!("RIST output '{}'", rist.id))?;
    validate_rist_addr(&rist.remote_addr, &format!("RIST output '{}' remote_addr", rist.id))?;
    if let Some(ref local) = rist.local_addr {
        validate_rist_addr(local, &format!("RIST output '{}' local_addr", rist.id))?;
    }
    validate_rist_common_knobs(
        rist.buffer_ms,
        None,
        rist.cname.as_deref(),
        rist.rtcp_interval_ms,
        &format!("RIST output '{}'", rist.id),
    )?;
    if let Some(cap) = rist.retransmit_buffer_capacity {
        if cap < 64 || cap > 65_536 {
            bail!(
                "RIST output '{}': retransmit_buffer_capacity must be 64-65536, got {cap}",
                rist.id
            );
        }
    }
    if let Some(ref red) = rist.redundancy {
        validate_rist_addr(
            &red.remote_addr,
            &format!("RIST output '{}' redundancy remote_addr", rist.id),
        )?;
        if red.remote_addr == rist.remote_addr {
            bail!(
                "RIST output '{}': redundancy leg 2 remote_addr must differ from leg 1",
                rist.id
            );
        }
        if let Some(ref local) = red.local_addr {
            validate_rist_addr(
                local,
                &format!("RIST output '{}' redundancy local_addr", rist.id),
            )?;
        }
        let leg1: SocketAddr = rist.remote_addr.parse()?;
        let leg2: SocketAddr = red.remote_addr.parse()?;
        if leg1.is_ipv4() != leg2.is_ipv4() {
            bail!(
                "RIST output '{}': redundancy leg 1 and leg 2 must use the same address family",
                rist.id
            );
        }
    }
    if let Some(ref enc) = rist.audio_encode {
        if rist.redundancy.is_some() {
            bail!(
                "RIST output '{}': audio_encode is not yet supported with SMPTE 2022-7 redundancy",
                rist.id
            );
        }
        validate_audio_encode(
            enc,
            &["aac_lc", "he_aac_v1", "he_aac_v2", "mp2", "ac3"],
            &format!("RIST output '{}'", rist.id),
        )?;
    }
    if let Some(ref tj) = rist.transcode {
        if rist.audio_encode.is_none() {
            bail!(
                "RIST output '{}': transcode has no effect without audio_encode set",
                rist.id
            );
        }
        validate_transcode_block_deferred(
            tj,
            &format!("RIST output '{}' transcode", rist.id),
        )?;
    }
    if let Some(ref enc) = rist.video_encode {
        if rist.redundancy.is_some() {
            bail!(
                "RIST output '{}': video_encode is not yet supported with SMPTE 2022-7 redundancy",
                rist.id
            );
        }
        validate_video_encode(enc, &format!("RIST output '{}'", rist.id))?;
    }
    Ok(())
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

/// Validates a source-specific multicast (SSM, RFC 3678) source address.
///
/// `source_addr` is a no-op when `None`. When `Some`, the bind address must be
/// multicast, the source must parse as an IP, must be unicast (not multicast),
/// and must share the address family of the bind. We deliberately don't enforce
/// the IANA SSM ranges (232.0.0.0/8 / ff3x::/32) — many real broadcast plants
/// run SSM joins on legacy ASM groups and the kernel accepts it.
fn validate_ssm_source(
    bind_addr: &str,
    source_addr: Option<&str>,
    context: &str,
) -> Result<()> {
    let Some(src) = source_addr else { return Ok(()) };
    let bind: SocketAddr = bind_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("{context}: bind_addr '{bind_addr}' is not parseable: {e}"))?;
    let src_ip: std::net::IpAddr = src
        .parse()
        .map_err(|e| anyhow::anyhow!("{context}: source_addr '{src}' is not parseable: {e}"))?;
    if !bind.ip().is_multicast() {
        bail!("{context}: source_addr is only valid when bind_addr is multicast (got bind_addr '{bind_addr}')");
    }
    if src_ip.is_multicast() {
        bail!("{context}: source_addr must be a unicast address (got multicast '{src}')");
    }
    if bind.is_ipv4() != matches!(src_ip, std::net::IpAddr::V4(_)) {
        bail!(
            "{context}: source_addr '{src}' must be the same address family as bind_addr '{bind_addr}'"
        );
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

/// Validates an [`NmosRegistrationConfig`] block.
///
/// Length-bounded URL + token, parseable scheme, supported API version, and
/// sensible heartbeat / timeout ranges. Disabled blocks are still validated so
/// re-enabling at runtime does not surface a fresh error.
pub fn validate_nmos_registration(c: &crate::config::models::NmosRegistrationConfig) -> Result<()> {
    if c.registry_url.is_empty() {
        bail!("nmos_registration.registry_url cannot be empty");
    }
    if c.registry_url.len() > 2048 {
        bail!("nmos_registration.registry_url must be at most 2048 characters");
    }
    if !(c.registry_url.starts_with("http://") || c.registry_url.starts_with("https://")) {
        bail!(
            "nmos_registration.registry_url must start with http:// or https:// (got {:?})",
            c.registry_url
        );
    }
    // Reject trailing slashes / explicit path components — the client owns the
    // `/x-nmos/registration/<api_version>/...` suffix.
    if c.registry_url.trim_end_matches('/').contains("/x-nmos") {
        bail!(
            "nmos_registration.registry_url should be the registry's base URL (scheme://host[:port]); \
             do not include /x-nmos/... — got {:?}",
            c.registry_url
        );
    }
    match c.api_version.as_str() {
        "v1.3" => {}
        other => bail!(
            "nmos_registration.api_version: only 'v1.3' is supported in this release (got {:?})",
            other
        ),
    }
    if !(1..=60).contains(&c.heartbeat_interval_secs) {
        bail!(
            "nmos_registration.heartbeat_interval_secs must be between 1 and 60 (got {})",
            c.heartbeat_interval_secs
        );
    }
    if !(1..=30).contains(&c.request_timeout_secs) {
        bail!(
            "nmos_registration.request_timeout_secs must be between 1 and 30 (got {})",
            c.request_timeout_secs
        );
    }
    if let Some(ref t) = c.bearer_token {
        if t.len() > 4096 {
            bail!("nmos_registration.bearer_token must be at most 4096 characters");
        }
        if t.is_empty() {
            bail!(
                "nmos_registration.bearer_token must be omitted (or null) when not in use, not empty"
            );
        }
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
            let relays = tunnel.effective_relays();
            if relays.is_empty() {
                bail!(
                    "Tunnel '{}': at least one relay address is required for relay mode (set `relay_addrs`)",
                    tunnel.id
                );
            }
            if relays.len() > crate::tunnel::config::MAX_RELAY_ADDRS {
                bail!(
                    "Tunnel '{}': at most {} relay addresses are allowed (primary + backup), got {}",
                    tunnel.id,
                    crate::tunnel::config::MAX_RELAY_ADDRS,
                    relays.len()
                );
            }
            let mut seen: std::collections::HashSet<std::net::SocketAddr> =
                std::collections::HashSet::new();
            for (idx, addr) in relays.iter().enumerate() {
                if addr.is_empty() || addr.len() > 256 {
                    bail!(
                        "Tunnel '{}': relay_addrs[{}] must be 1-256 characters",
                        tunnel.id,
                        idx
                    );
                }
                let parsed: std::net::SocketAddr = addr.parse().map_err(|e| {
                    anyhow::anyhow!(
                        "Tunnel '{}': relay_addrs[{}] is not a valid socket address: {}",
                        tunnel.id,
                        idx,
                        e
                    )
                })?;
                if !seen.insert(parsed) {
                    bail!(
                        "Tunnel '{}': relay_addrs contains duplicate address '{}'",
                        tunnel.id,
                        addr
                    );
                }
            }

            // Failback RTT gate: only meaningful with a backup relay, and must
            // be a sensible value (0..=5000 ms).
            if let Some(gate) = tunnel.max_rtt_failback_increase_ms
                && gate > 5000
            {
                bail!(
                    "Tunnel '{}': max_rtt_failback_increase_ms must be <= 5000 (got {})",
                    tunnel.id,
                    gate
                );
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

// ── Port conflict detection ──────────────────────────────────────────

/// Protocol family for port conflict grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proto {
    Tcp,
    Udp,
}

impl std::fmt::Display for Proto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Proto::Tcp => write!(f, "TCP"),
            Proto::Udp => write!(f, "UDP"),
        }
    }
}

/// A port that a component intends to bind.
struct BoundPort {
    port: u16,
    proto: Proto,
    /// True when bound to 0.0.0.0 / :: (conflicts with any specific IP on same port).
    wildcard: bool,
    ip: std::net::IpAddr,
    label: String,
}

/// Check whether two bound-port entries conflict.
///
/// Two entries conflict when they share the same port and protocol AND either
/// one is a wildcard bind or both specify the same IP.
fn ports_conflict(a: &BoundPort, b: &BoundPort) -> bool {
    a.port == b.port
        && a.proto == b.proto
        && (a.wildcard || b.wildcard || a.ip == b.ip)
}

/// Validates that no two components in the config try to bind the same local
/// port.  Called at the end of [`validate_config`] after individual component
/// validation has already passed.
fn validate_port_conflicts(config: &AppConfig) -> Result<()> {
    let mut ports: Vec<BoundPort> = Vec::new();

    // Helper: parse a socket address and register it, checking for conflicts.
    let mut register = |addr_str: &str, proto: Proto, label: String| -> Result<()> {
        let addr: SocketAddr = match addr_str.parse() {
            Ok(a) => a,
            Err(_) => return Ok(()), // unparseable addresses are caught by other validators
        };

        // Skip ephemeral ports (port 0) — the OS picks a random port at bind time.
        if addr.port() == 0 {
            return Ok(());
        }

        // Skip multicast addresses — multiple binds with SO_REUSEADDR are valid.
        if addr.ip().is_multicast() {
            return Ok(());
        }

        let entry = BoundPort {
            port: addr.port(),
            proto,
            wildcard: addr.ip().is_unspecified(),
            ip: addr.ip(),
            label,
        };

        for existing in &ports {
            if ports_conflict(existing, &entry) {
                bail!(
                    "Port conflict: {} and {} both bind {} port {}. \
                     Change one of their addresses to avoid the conflict.",
                    existing.label,
                    entry.label,
                    proto,
                    addr.port()
                );
            }
        }

        ports.push(entry);
        Ok(())
    };

    // ── Edge API server ──
    let server_addr = format!("{}:{}", config.server.listen_addr, config.server.listen_port);
    register(&server_addr, Proto::Tcp, "Edge API server".to_string())?;

    // ── Monitor dashboard ──
    if let Some(ref monitor) = config.monitor {
        let monitor_addr = format!("{}:{}", monitor.listen_addr, monitor.listen_port);
        register(&monitor_addr, Proto::Tcp, "Monitor dashboard".to_string())?;
    }

    // ── Tunnels ──
    for tunnel in &config.tunnels {
        if !tunnel.enabled {
            continue;
        }
        let proto = match tunnel.protocol {
            crate::tunnel::config::TunnelProtocol::Tcp => Proto::Tcp,
            crate::tunnel::config::TunnelProtocol::Udp => Proto::Udp,
        };
        // Egress tunnels bind local_addr to listen for local traffic.
        // Ingress tunnels use local_addr as a forward destination (no local bind).
        if tunnel.direction == crate::tunnel::config::TunnelDirection::Egress {
            register(
                &tunnel.local_addr,
                proto,
                format!("Tunnel '{}' ({} egress)", tunnel.name, proto),
            )?;
        }
        // Direct mode ingress binds a QUIC listener on direct_listen_addr.
        if tunnel.mode == crate::tunnel::config::TunnelMode::Direct
            && tunnel.direction == crate::tunnel::config::TunnelDirection::Ingress
        {
            if let Some(ref addr) = tunnel.direct_listen_addr {
                register(
                    addr,
                    Proto::Udp, // QUIC runs over UDP
                    format!("Tunnel '{}' (direct QUIC listener)", tunnel.name),
                )?;
            }
        }
    }

    // ── Inputs ──
    for input in &config.inputs {
        let label_prefix = format!("Input '{}' [{}]", input.name, input.id);
        match &input.config {
            InputConfig::Rtp(cfg) => {
                register(&cfg.bind_addr, Proto::Udp, format!("{label_prefix} (RTP)"))?;
                if let Some(ref red) = cfg.redundancy {
                    register(
                        &red.bind_addr,
                        Proto::Udp,
                        format!("{label_prefix} (RTP redundancy leg 2)"),
                    )?;
                }
            }
            InputConfig::Udp(cfg) => {
                register(&cfg.bind_addr, Proto::Udp, format!("{label_prefix} (UDP)"))?;
            }
            InputConfig::Srt(cfg) => {
                // Only listener and rendezvous modes bind a specific port.
                if cfg.mode != SrtMode::Caller {
                    if let Some(ref addr) = cfg.local_addr {
                        register(addr, Proto::Udp, format!("{label_prefix} (SRT listener)"))?;
                    }
                }
                if let Some(ref red) = cfg.redundancy {
                    if red.mode != SrtMode::Caller {
                        if let Some(ref addr) = red.local_addr {
                            register(
                                addr,
                                Proto::Udp,
                                format!("{label_prefix} (SRT redundancy leg 2)"),
                            )?;
                        }
                    }
                }
            }
            InputConfig::Rist(cfg) => {
                // RIST binds RTP on the even port and RTCP on port+1. Register
                // both so a clashing input is caught at config load time.
                register(&cfg.bind_addr, Proto::Udp, format!("{label_prefix} (RIST RTP)"))?;
                if let Ok(sa) = cfg.bind_addr.parse::<SocketAddr>() {
                    let rtcp = SocketAddr::new(sa.ip(), sa.port().wrapping_add(1));
                    register(&rtcp.to_string(), Proto::Udp, format!("{label_prefix} (RIST RTCP)"))?;
                }
                if let Some(ref red) = cfg.redundancy {
                    register(
                        &red.bind_addr,
                        Proto::Udp,
                        format!("{label_prefix} (RIST leg 2 RTP)"),
                    )?;
                    if let Ok(sa) = red.bind_addr.parse::<SocketAddr>() {
                        let rtcp = SocketAddr::new(sa.ip(), sa.port().wrapping_add(1));
                        register(
                            &rtcp.to_string(),
                            Proto::Udp,
                            format!("{label_prefix} (RIST leg 2 RTCP)"),
                        )?;
                    }
                }
            }
            InputConfig::Rtmp(cfg) => {
                register(
                    &cfg.listen_addr,
                    Proto::Tcp,
                    format!("{label_prefix} (RTMP server)"),
                )?;
            }
            InputConfig::St2110_30(cfg) | InputConfig::St2110_31(cfg) => {
                register(&cfg.bind_addr, Proto::Udp, format!("{label_prefix} (ST 2110)"))?;
                if let Some(ref red) = cfg.redundancy {
                    register(
                        &red.addr,
                        Proto::Udp,
                        format!("{label_prefix} (ST 2110 redundancy leg 2)"),
                    )?;
                }
            }
            InputConfig::St2110_40(cfg) => {
                register(
                    &cfg.bind_addr,
                    Proto::Udp,
                    format!("{label_prefix} (ST 2110-40)"),
                )?;
                if let Some(ref red) = cfg.redundancy {
                    register(
                        &red.addr,
                        Proto::Udp,
                        format!("{label_prefix} (ST 2110-40 redundancy leg 2)"),
                    )?;
                }
            }
            InputConfig::RtpAudio(cfg) => {
                register(
                    &cfg.bind_addr,
                    Proto::Udp,
                    format!("{label_prefix} (RTP audio)"),
                )?;
                if let Some(ref red) = cfg.redundancy {
                    register(
                        &red.addr,
                        Proto::Udp,
                        format!("{label_prefix} (RTP audio redundancy leg 2)"),
                    )?;
                }
            }
            InputConfig::St2110_20(cfg) => {
                register(&cfg.bind_addr, Proto::Udp, format!("{label_prefix} (ST 2110-20)"))?;
                if let Some(ref red) = cfg.redundancy {
                    register(&red.addr, Proto::Udp, format!("{label_prefix} (ST 2110-20 leg 2)"))?;
                }
            }
            InputConfig::St2110_23(cfg) => {
                for (i, s) in cfg.sub_streams.iter().enumerate() {
                    register(
                        &s.bind_addr,
                        Proto::Udp,
                        format!("{label_prefix} (ST 2110-23 sub_streams[{i}])"),
                    )?;
                    if let Some(ref red) = s.redundancy {
                        register(
                            &red.addr,
                            Proto::Udp,
                            format!("{label_prefix} (ST 2110-23 sub_streams[{i}] leg 2)"),
                        )?;
                    }
                }
            }
            // RTSP, WebRTC, and WHEP inputs don't bind specific local ports.
            InputConfig::Rtsp(_) | InputConfig::Webrtc(_) | InputConfig::Whep(_) => {}
            // Bonded inputs bind per-path — register each receiver-role
            // path's local port. Client / sender-role paths don't
            // occupy a specific local port so there's nothing to
            // pre-register for conflict detection.
            InputConfig::Bonded(cfg) => {
                for (i, p) in cfg.paths.iter().enumerate() {
                    if let Some(bind) = bond_path_bind_addr(&p.transport) {
                        register(
                            &bind,
                            Proto::Udp,
                            format!("{label_prefix} (bonded path[{i}] {})", p.name),
                        )?;
                    }
                }
            }
            // Synthetic input doesn't bind a socket.
            InputConfig::TestPattern(_) => {}
            // Media player reads from local disk — no socket bind.
            InputConfig::MediaPlayer(_) => {}
            // Replay reads from the local replay store — no socket bind.
            InputConfig::Replay(_) => {}
        }
    }

    // ── Outputs (only those that bind specific local ports) ──
    //
    // Outputs that don't accept a `bind_addr` (push-style: RTMP/HLS/CMAF/WebRTC)
    // and outputs whose bind is left ephemeral (`bind_addr` unset, defaulting to
    // `0.0.0.0:0`) are skipped — they cannot collide because the OS picks a
    // free port. When the operator pins `bind_addr` to a specific port we DO
    // register it so a collision with another input/output/server is caught.
    for output in &config.outputs {
        let label_prefix = format!("Output '{}' [{}]", output.name(), output.id());
        match output {
            OutputConfig::Srt(cfg) => {
                if cfg.mode != SrtMode::Caller
                    && let Some(ref addr) = cfg.local_addr
                {
                    register(addr, Proto::Udp, format!("{label_prefix} (SRT listener)"))?;
                }
                if let Some(ref red) = cfg.redundancy
                    && red.mode != SrtMode::Caller
                    && let Some(ref addr) = red.local_addr
                {
                    register(
                        addr,
                        Proto::Udp,
                        format!("{label_prefix} (SRT redundancy leg 2)"),
                    )?;
                }
            }
            OutputConfig::Rtp(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (RTP source bind)"))?;
                }
                if let Some(ref red) = cfg.redundancy
                    && let Some(ref addr) = red.bind_addr
                {
                    register(
                        addr,
                        Proto::Udp,
                        format!("{label_prefix} (RTP source bind, redundancy leg 2)"),
                    )?;
                }
            }
            OutputConfig::Udp(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (UDP source bind)"))?;
                }
            }
            OutputConfig::Rist(cfg) => {
                // RIST sender binds RTP on even port P, RTCP on P+1. When the
                // operator pins `local_addr`, both sockets matter for collision
                // detection.
                if let Some(ref addr) = cfg.local_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (RIST RTP source bind)"))?;
                    if let Ok(sa) = addr.parse::<SocketAddr>() {
                        let rtcp = SocketAddr::new(sa.ip(), sa.port().wrapping_add(1));
                        register(
                            &rtcp.to_string(),
                            Proto::Udp,
                            format!("{label_prefix} (RIST RTCP source bind)"),
                        )?;
                    }
                }
                if let Some(ref red) = cfg.redundancy
                    && let Some(ref addr) = red.local_addr
                {
                    register(
                        addr,
                        Proto::Udp,
                        format!("{label_prefix} (RIST RTP source bind, leg 2)"),
                    )?;
                    if let Ok(sa) = addr.parse::<SocketAddr>() {
                        let rtcp = SocketAddr::new(sa.ip(), sa.port().wrapping_add(1));
                        register(
                            &rtcp.to_string(),
                            Proto::Udp,
                            format!("{label_prefix} (RIST RTCP source bind, leg 2)"),
                        )?;
                    }
                }
            }
            OutputConfig::St2110_30(cfg) | OutputConfig::St2110_31(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (ST 2110 audio source bind)"))?;
                }
            }
            OutputConfig::St2110_40(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (ST 2110-40 source bind)"))?;
                }
            }
            OutputConfig::St2110_20(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (ST 2110-20 source bind)"))?;
                }
            }
            OutputConfig::St2110_23(cfg) => {
                for (i, s) in cfg.sub_streams.iter().enumerate() {
                    if let Some(ref addr) = s.bind_addr {
                        register(
                            addr,
                            Proto::Udp,
                            format!("{label_prefix} (ST 2110-23 sub_streams[{i}] source bind)"),
                        )?;
                    }
                }
            }
            OutputConfig::RtpAudio(cfg) => {
                if let Some(ref addr) = cfg.bind_addr {
                    register(addr, Proto::Udp, format!("{label_prefix} (RTP audio source bind)"))?;
                }
            }
            OutputConfig::Bonded(cfg) => {
                for (i, p) in cfg.paths.iter().enumerate() {
                    if let Some(bind) = bond_path_bind_addr(&p.transport) {
                        register(
                            &bind,
                            Proto::Udp,
                            format!("{label_prefix} (bonded path[{i}] {})", p.name),
                        )?;
                    }
                }
            }
            // Push-style outputs (RTMP/HLS/CMAF/WebRTC) don't bind a local port.
            OutputConfig::Rtmp(_) | OutputConfig::Hls(_) | OutputConfig::Cmaf(_) | OutputConfig::Webrtc(_) => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{
        AssembledProgram, AssembledStream, AssemblyKind, EssenceKind, FlowAssembly, PcrSource,
        SlotSource,
    };

    fn spts_assembly(input_id: &str) -> FlowAssembly {
        FlowAssembly {
            kind: AssemblyKind::Spts,
            pcr_source: Some(PcrSource {
                input_id: input_id.to_string(),
                pid: 0x100,
            }),
            programs: vec![AssembledProgram {
                program_number: 1,
                service_name: None,
                pmt_pid: 0x1000,
                pcr_source: None,
                streams: vec![AssembledStream {
                    source: SlotSource::Pid {
                        input_id: input_id.to_string(),
                        source_pid: 0x100,
                    },
                    out_pid: 0x100,
                    stream_type: 0x1B,
                    label: None,
                }],
            }],
        }
    }

    #[test]
    fn assembly_passthrough_rejects_non_empty_programs() {
        let mut a = spts_assembly("in-a");
        a.kind = AssemblyKind::Passthrough;
        let err = validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect_err("must reject passthrough with programs");
        assert!(err.to_string().contains("passthrough"));
    }

    #[test]
    fn assembly_spts_requires_pcr_source() {
        let mut a = spts_assembly("in-a");
        a.pcr_source = None;
        let err = validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect_err("must reject without pcr_source");
        assert!(err.to_string().contains("pcr_source"));
    }

    #[test]
    fn assembly_rejects_duplicate_program_numbers() {
        let mut a = spts_assembly("in-a");
        a.kind = AssemblyKind::Mpts;
        a.programs.push(a.programs[0].clone()); // same program_number
        a.programs[1].pmt_pid = 0x1001;
        let err = validate_flow_assembly(&a, &["in-a".into()], "test").unwrap_err();
        assert!(err.to_string().contains("duplicate program_number"));
    }

    #[test]
    fn assembly_rejects_duplicate_pmt_pid() {
        let mut a = spts_assembly("in-a");
        a.kind = AssemblyKind::Mpts;
        let mut p2 = a.programs[0].clone();
        p2.program_number = 2;
        a.programs.push(p2); // same pmt_pid
        let err = validate_flow_assembly(&a, &["in-a".into()], "test").unwrap_err();
        assert!(err.to_string().contains("duplicate pmt_pid"));
    }

    #[test]
    fn assembly_rejects_unknown_input_id() {
        let a = spts_assembly("in-unknown");
        let err = validate_flow_assembly(&a, &["in-a".into()], "test").unwrap_err();
        assert!(err.to_string().contains("in-unknown"));
    }

    #[test]
    fn assembly_rejects_out_pid_colliding_with_pmt_pid() {
        let mut a = spts_assembly("in-a");
        a.programs[0].streams[0].out_pid = 0x1000;
        let err = validate_flow_assembly(&a, &["in-a".into()], "test").unwrap_err();
        assert!(err.to_string().contains("collides with pmt_pid"));
    }

    #[test]
    fn assembly_rejects_nested_hitless() {
        let inner = SlotSource::Hitless {
            primary: Box::new(SlotSource::Pid {
                input_id: "in-a".into(),
                source_pid: 0x100,
            }),
            backup: Box::new(SlotSource::Essence {
                input_id: "in-a".into(),
                kind: EssenceKind::Video,
            }),
        };
        let mut a = spts_assembly("in-a");
        a.programs[0].streams[0].source = SlotSource::Hitless {
            primary: Box::new(inner),
            backup: Box::new(SlotSource::Pid {
                input_id: "in-a".into(),
                source_pid: 0x101,
            }),
        };
        let err = validate_flow_assembly(&a, &["in-a".into()], "test").unwrap_err();
        assert!(err.to_string().contains("nest"));
    }

    #[test]
    fn assembly_valid_spts_passes() {
        let a = spts_assembly("in-a");
        validate_flow_assembly(&a, &["in-a".into()], "test").expect("valid");
    }

    #[test]
    fn assembly_rejects_pcr_source_not_on_any_slot() {
        let mut a = spts_assembly("in-a");
        // Keep the one slot on PID 0x100, but point PCR at a different PID
        // on the same input — no slot carries it, so resolution must fail.
        a.pcr_source = Some(PcrSource {
            input_id: "in-a".into(),
            pid: 0x200,
        });
        let err = validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect_err("must reject unresolved pcr_source");
        let msg = err.to_string();
        assert!(msg.contains("pcr_source"), "message was: {msg}");
        assert!(msg.contains("does not resolve"), "message was: {msg}");
    }

    #[test]
    fn assembly_mpts_rejects_program_without_pcr_source() {
        // Two programs; no flow-level pcr, no per-program pcr on program 2.
        let mut a = spts_assembly("in-a");
        a.kind = AssemblyKind::Mpts;
        a.pcr_source = None;
        a.programs[0].pcr_source = Some(PcrSource { input_id: "in-a".into(), pid: 0x100 });
        let mut p2 = a.programs[0].clone();
        p2.program_number = 2;
        p2.pmt_pid = 0x1001;
        p2.pcr_source = None; // missing — no flow-level fallback either
        a.programs.push(p2);
        let err = validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect_err("must reject MPTS program without pcr_source");
        assert!(err.to_string().contains("mpts program 2"));
    }

    #[test]
    fn assembly_mpts_flow_level_pcr_applies_as_fallback() {
        let mut a = spts_assembly("in-a");
        a.kind = AssemblyKind::Mpts;
        // Flow-level pcr already set by spts_assembly(). Program 2 has no
        // per-program override and must inherit from the flow level.
        let mut p2 = a.programs[0].clone();
        p2.program_number = 2;
        p2.pmt_pid = 0x1001;
        p2.pcr_source = None;
        a.programs.push(p2);
        validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect("MPTS with flow-level pcr fallback must pass");
    }

    #[test]
    fn assembly_spts_accepts_per_program_pcr_only() {
        // Per-program pcr_source is enough for SPTS — flow-level not
        // required when the single program carries its own.
        let mut a = spts_assembly("in-a");
        a.pcr_source = None;
        a.programs[0].pcr_source = Some(PcrSource { input_id: "in-a".into(), pid: 0x100 });
        validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect("SPTS with program-level pcr must pass");
    }

    #[test]
    fn assembly_accepts_pcr_source_on_essence_slot() {
        // PCR points at a specific PID, but the slot is Essence — resolution
        // is deferred to runtime, so validation must accept it.
        let mut a = spts_assembly("in-a");
        a.programs[0].streams[0].source = SlotSource::Essence {
            input_id: "in-a".into(),
            kind: EssenceKind::Video,
        };
        a.pcr_source = Some(PcrSource {
            input_id: "in-a".into(),
            pid: 0x321,
        });
        validate_flow_assembly(&a, &["in-a".into()], "test")
            .expect("essence slot defers pcr resolution to runtime");
    }

    /// Helper: build a minimal valid AppConfig with one flow referencing one
    /// top-level input and one top-level output.
    fn make_config_with_rtp(
        input_bind: &str,
        output_dest: &str,
    ) -> AppConfig {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: input_bind.to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "Out 1".to_string(),
            dest_addr: output_dest.to_string(),
            bind_addr: None,
            interface_addr: None,
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        config
    }

    /// Helper: build an AppConfig with a single RTP input (no output) in a flow.
    fn make_config_input_only(input_config: InputConfig) -> AppConfig {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: input_config,
        });
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        config
    }

    #[test]
    fn test_valid_rtp_flow() {
        let config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:5004");
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_invalid_bind_addr() {
        let config = make_config_input_only(InputConfig::Rtp(RtpInputConfig {
            bind_addr: "not-an-address".to_string(),
            interface_addr: None,
            source_addr: None,
            fec_decode: None,
            allowed_sources: None,
            allowed_payload_types: None,
            max_bitrate_mbps: None,
            tr07_mode: None,
            redundancy: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_srt_caller_missing_remote() {
        let config = make_config_input_only(InputConfig::Srt(SrtInputConfig {
            mode: SrtMode::Caller,
            local_addr: Some("0.0.0.0:9000".to_string()),
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
            bonding: None,
            transport_mode: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_srt_rendezvous_with_fec_is_rejected() {
        // Rationale: libsrt 1.5.5 does not negotiate `SRT_CMD_FILTER` in
        // rendezvous mode — both peers reject the handshake with ERROR:FILTER.
        // The config validator must hard-fail so operators don't deploy it.
        let config = make_config_input_only(InputConfig::Srt(SrtInputConfig {
            mode: SrtMode::Rendezvous,
            local_addr: Some("127.0.0.1:9000".to_string()),
            remote_addr: Some("127.0.0.1:9001".to_string()),
            latency_ms: 300,
            peer_idle_timeout_secs: 30,
            recv_latency_ms: None,
            peer_latency_ms: None,
            passphrase: None,
            aes_key_len: None,
            crypto_mode: None,
            max_rexmit_bw: None,
            stream_id: None,
            packet_filter: Some("fec,cols:10,rows:5,layout:staircase,arq:onreq".to_string()),
            max_bw: None, input_bw: None, overhead_bw: None,
            enforced_encryption: None, connect_timeout_secs: None,
            flight_flag_size: None, send_buffer_size: None, recv_buffer_size: None,
            ip_tos: None, retransmit_algo: None, send_drop_delay: None,
            loss_max_ttl: None, km_refresh_rate: None, km_pre_announce: None,
            payload_size: None, mss: None, tlpkt_drop: None, ip_ttl: None,
            redundancy: None,
            bonding: None,
            transport_mode: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        let err = validate_config(&config).expect_err("rendezvous+FEC must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("rendezvous"),
            "error should mention rendezvous: {msg}"
        );
    }

    #[test]
    fn test_duplicate_flow_ids() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-2".to_string(),
            name: "Input 2".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5001".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.flows.push(FlowConfig {
            id: "same-id".to_string(),
            name: "Flow 1".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        config.flows.push(FlowConfig {
            id: "same-id".to_string(),
            name: "Flow 2".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-2".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_passphrase_length() {
        let config = make_config_input_only(InputConfig::Srt(SrtInputConfig {
            mode: SrtMode::Listener,
            local_addr: Some("0.0.0.0:9000".to_string()),
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
            bonding: None,
            transport_mode: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        assert!(validate_config(&config).is_err());
    }

    // --- IPv6 address validation tests ---

    #[test]
    fn test_valid_ipv6_unicast_rtp_flow() {
        let config = make_config_with_rtp("[::]:5000", "[::1]:5004");
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_valid_ipv4_multicast_rtp_flow() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),
                interface_addr: Some("192.168.1.100".to_string()),
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "Multicast Out".to_string(),
            dest_addr: "239.1.2.1:5004".to_string(),
            bind_addr: None,
            interface_addr: Some("192.168.1.100".to_string()),
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "IPv4 Multicast".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_valid_ipv6_multicast_rtp_flow() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[ff7e::1]:5000".to_string(),
                interface_addr: Some("::1".to_string()),
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "IPv6 Mcast Out".to_string(),
            dest_addr: "[ff7e::2]:5004".to_string(),
            bind_addr: None,
            interface_addr: Some("::1".to_string()),
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "IPv6 Multicast".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_ok());
    }

    // --- Address family mismatch tests ---

    #[test]
    fn test_rtp_input_mismatched_addr_family() {
        let config = make_config_input_only(InputConfig::Rtp(RtpInputConfig {
            bind_addr: "239.1.1.1:5000".to_string(),         // IPv4
            interface_addr: Some("::1".to_string()),          // IPv6 - mismatch!
            source_addr: None,
            fec_decode: None,
            allowed_sources: None,
            allowed_payload_types: None,
            max_bitrate_mbps: None,
            tr07_mode: None,
            redundancy: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_rtp_output_mismatched_dest_bind_family() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "[::]:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "Bad".to_string(),
            dest_addr: "[::1]:5004".to_string(),            // IPv6
            bind_addr: Some("0.0.0.0:0".to_string()),      // IPv4 - mismatch!
            interface_addr: None,
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "Mismatched".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_rtp_output_mismatched_dest_iface_family() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "Bad".to_string(),
            dest_addr: "239.1.1.1:5004".to_string(),       // IPv4
            bind_addr: None,
            interface_addr: Some("::1".to_string()),        // IPv6 - mismatch!
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "Mismatched".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_rtp_output_invalid_interface_addr() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Input 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "0.0.0.0:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.outputs.push(OutputConfig::Rtp(RtpOutputConfig {
            active: true,
            group: None,
            id: "out-1".to_string(),
            name: "Bad".to_string(),
            dest_addr: "239.1.1.1:5004".to_string(),
            bind_addr: None,
            interface_addr: Some("not-an-ip".to_string()),
            fec_encode: None,
            dscp: 46,
            redundancy: None,
            program_number: None,
            pid_map: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.flows.push(FlowConfig {
            id: "f1".to_string(),
            name: "Bad iface".to_string(),
            enabled: true,
            media_analysis: true,
            thumbnail: true,
            thumbnail_program_number: None,
            bandwidth_limit: None,
            flow_group_id: None,
            clock_domain: None,
            input_ids: vec!["in-1".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_err());
    }

    // ─────────────── SMPTE ST 2110 validation tests ───────────────

    fn st2110_30_input(addr: &str) -> St2110AudioInputConfig {
        St2110AudioInputConfig {
            bind_addr: addr.to_string(),
            interface_addr: None,
            source_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: Some(0),
            allowed_sources: None,
            max_bitrate_mbps: None,
            audio_encode: None,
            transcode: None,
        }
    }

    fn st2110_30_output(id: &str, dest: &str) -> St2110AudioOutputConfig {
        St2110AudioOutputConfig {
            active: true,
            group: None,
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
            audio_track_index: None,
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
            source_addr: None,
        });
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_red_blue_address_family_mismatch_rejected() {
        let mut cfg = st2110_30_input("10.0.0.5:5004");
        cfg.redundancy = Some(RedBlueBindConfig {
            addr: "[::1]:5006".to_string(),
            interface_addr: None,
            source_addr: None,
        });
        assert!(validate_st2110_audio_input(&cfg, St2110Profile::Pcm).is_err());
    }

    #[test]
    fn test_st2110_red_blue_distinct_unicast_ok() {
        let mut cfg = st2110_30_input("10.0.0.5:5004");
        cfg.redundancy = Some(RedBlueBindConfig {
            addr: "10.0.1.5:5006".to_string(),
            interface_addr: None,
            source_addr: None,
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
            source_addr: None,
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
            source_addr: None,
            redundancy: None,
            payload_type: 50,
            clock_domain: Some(0),
            allowed_sources: None,
        };
        assert!(validate_st2110_ancillary_input(&anc).is_err());
    }

    #[test]
    fn test_flow_with_st2110_input_validates() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-audio".to_string(),
            name: "Audio In".to_string(),
            config: InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004")),
        });
        config.outputs.push(OutputConfig::St2110_30(st2110_30_output(
            "out-1",
            "239.10.10.2:5004",
        )));
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
            input_ids: vec!["in-audio".to_string()],
            output_ids: vec!["out-1".to_string()],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        config.flow_groups.push(FlowGroupConfig {
            id: "group-1".to_string(),
            name: "Group 1".to_string(),
            clock_domain: Some(0),
            flows: vec!["audio-flow".to_string()],
        });
        validate_config(&config).expect("valid ST 2110 flow");
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
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "In 1".to_string(),
            config: InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004")),
        });
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-2".to_string(),
            name: "In 2".to_string(),
            config: InputConfig::St2110_30(st2110_30_input("239.10.10.2:5004")),
        });
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
            input_ids: vec!["in-1".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
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
            input_ids: vec!["in-2".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
        });
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_flow_group_round_trip_ok() {
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "In 1".to_string(),
            config: InputConfig::St2110_30(st2110_30_input("239.10.10.1:5004")),
        });
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
            input_ids: vec!["in-1".to_string()],
            output_ids: vec![],
            assembly: None,
            content_analysis: None,
                recording: None,
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
    ///
    /// Some configs in `testbed/configs/` deliberately exercise
    /// feature-gated codecs (e.g. `video-encode-rtmp-hevc-edge.json`
    /// uses `x265`). In the default `cargo test` build — which does
    /// **not** enable `video-encoder-x264` / `-x265` / `-nvenc` —
    /// those fail validation with a "requires Cargo feature" error,
    /// and that's the correct behaviour. Treat that specific error
    /// as a skip, not a test failure.
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
        let mut skipped_feature_gated = 0usize;
        for entry in fs::read_dir(&testbed_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Edge configs only — skip the relay config (different
            // shape) and the encrypted secrets file (opaque v1: blob,
            // not JSON).
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "relay.json" || name == "secrets.json" {
                continue;
            }
            let raw = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            // Deserialize must succeed unchanged with the new optional fields.
            let cfg: AppConfig = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("deserialize {}: {e}", path.display()));
            // Validation must succeed OR fail only because a referenced
            // codec needs a Cargo feature that wasn't compiled in for
            // this test run. Any other error is a genuine regression.
            match validate_config(&cfg) {
                Ok(()) => checked += 1,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("requires this edge to be built with the")
                        && msg.contains("Cargo feature")
                    {
                        skipped_feature_gated += 1;
                    } else {
                        panic!("validate {}: {e}", path.display());
                    }
                }
            }
        }
        assert!(
            checked + skipped_feature_gated > 0,
            "no testbed configs were checked"
        );
        assert!(
            checked > 0,
            "every testbed config required an uncompiled codec feature — build with \
             --features video-encoder-x264 to exercise the full matrix"
        );
    }

    // ── audio_encode validation tests ─────────────────────────────────

    fn make_audio_encode(codec: &str) -> crate::config::models::AudioEncodeConfig {
        crate::config::models::AudioEncodeConfig {
            codec: codec.to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback: false,
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
    fn audio_encode_silent_fallback_round_trips() {
        // Explicit true survives a JSON round-trip and is honoured by validation.
        let json = r#"{"codec":"aac_lc","silent_fallback":true}"#;
        let parsed: crate::config::models::AudioEncodeConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.silent_fallback);
        assert!(validate_audio_encode(&parsed, &["aac_lc"], "test").is_ok());

        // Absent field defaults to false via `#[serde(default)]`.
        let json = r#"{"codec":"aac_lc"}"#;
        let parsed: crate::config::models::AudioEncodeConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.silent_fallback);

        // `skip_serializing_if` hides the default — keeps existing configs
        // byte-identical when the operator never touched this field.
        let emitted = serde_json::to_string(&parsed).unwrap();
        assert!(
            !emitted.contains("silent_fallback"),
            "default-false must not serialise, got: {emitted}"
        );
    }

    #[test]
    fn validate_output_rtmp_with_audio_encode() {
        use crate::config::models::{AudioEncodeConfig, OutputConfig, RtmpOutputConfig};
        let make = |codec: &str| OutputConfig::Rtmp(RtmpOutputConfig {
            active: true,
            group: None,
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
                silent_fallback: false,
            }),
            transcode: None,
            video_encode: None,
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
            active: true,
            group: None,
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: "https://example.com/hls".into(),
            segment_duration_secs: 2.0,
            auth_token: None,
            max_segments: 5,
            program_number: None,
            pid_map: None,
            audio_encode: Some(AudioEncodeConfig {
                codec: codec.into(),
                bitrate_kbps: None,
                sample_rate: None,
                channels: None,
                silent_fallback: false,
            }),
            transcode: None,
        });
        assert!(validate_output(&make("aac_lc")).is_ok());
        assert!(validate_output(&make("mp2")).is_ok());
        assert!(validate_output(&make("ac3")).is_ok());
        assert!(validate_output(&make("opus")).is_err());
    }

    #[test]
    fn validate_output_hls_rejects_crlf_in_ingest_url() {
        use crate::config::models::{HlsOutputConfig, OutputConfig};
        let make = |url: &str| OutputConfig::Hls(HlsOutputConfig {
            active: true,
            group: None,
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: url.into(),
            segment_duration_secs: 2.0,
            auth_token: None,
            max_segments: 5,
            program_number: None,
            pid_map: None,
            audio_encode: None,
            transcode: None,
        });
        assert!(validate_output(&make("https://example.com/hls")).is_ok());
        let err = validate_output(&make("https://example.com/hls\r\nX-Injected: 1"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ASCII control"), "unexpected error: {err}");
        assert!(
            validate_output(&make("https://example.com/hls\nfoo")).is_err()
        );
        assert!(
            validate_output(&make("https://example.com/hls\0null")).is_err()
        );
    }

    #[test]
    fn validate_output_hls_accepts_ipv6_ingest_url() {
        use crate::config::models::{HlsOutputConfig, OutputConfig};
        let make = |url: &str| OutputConfig::Hls(HlsOutputConfig {
            active: true,
            group: None,
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: url.into(),
            segment_duration_secs: 2.0,
            auth_token: None,
            max_segments: 5,
            program_number: None,
            pid_map: None,
            audio_encode: None,
            transcode: None,
        });
        assert!(validate_output(&make("http://[::1]:8080/hls")).is_ok());
        assert!(validate_output(&make("https://[2001:db8::1]/hls")).is_ok());
        assert!(validate_output(&make("https://[2001:db8::1]:8443/hls/seg.ts?token=abc")).is_ok());
    }

    #[test]
    fn validate_output_hls_rejects_malformed_url() {
        use crate::config::models::{HlsOutputConfig, OutputConfig};
        let make = |url: &str| OutputConfig::Hls(HlsOutputConfig {
            active: true,
            group: None,
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: url.into(),
            segment_duration_secs: 2.0,
            auth_token: None,
            max_segments: 5,
            program_number: None,
            pid_map: None,
            audio_encode: None,
            transcode: None,
        });
        let err = validate_output(&make("http://[::1/hls")).unwrap_err().to_string();
        assert!(err.contains("malformed"), "unexpected error: {err}");
    }

    #[test]
    fn validate_output_rtmp_accepts_ipv6_dest_url() {
        use crate::config::models::{OutputConfig, RtmpOutputConfig};
        let make = |url: &str| OutputConfig::Rtmp(RtmpOutputConfig {
            active: true,
            group: None,
            id: "rtmp1".into(),
            name: "rtmp 1".into(),
            dest_url: url.into(),
            stream_key: "abc".into(),
            reconnect_delay_secs: 5,
            max_reconnect_attempts: None,
            program_number: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        });
        assert!(validate_output(&make("rtmp://[::1]:1935/live")).is_ok());
        assert!(validate_output(&make("rtmps://[2001:db8::1]/app")).is_ok());
        let err = validate_output(&make("rtmp://[::1/live")).unwrap_err().to_string();
        assert!(err.contains("malformed"), "unexpected error: {err}");
    }

    #[test]
    fn validate_output_hls_rejects_crlf_in_auth_token() {
        use crate::config::models::{HlsOutputConfig, OutputConfig};
        let make = |token: &str| OutputConfig::Hls(HlsOutputConfig {
            active: true,
            group: None,
            id: "hls1".into(),
            name: "hls 1".into(),
            ingest_url: "https://example.com/hls".into(),
            segment_duration_secs: 2.0,
            auth_token: Some(token.into()),
            max_segments: 5,
            program_number: None,
            pid_map: None,
            audio_encode: None,
            transcode: None,
        });
        assert!(validate_output(&make("abc123DEF")).is_ok());
        let err = validate_output(&make("Bearer xxx\r\nX-Injected: 1"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("control or whitespace"),
            "unexpected error: {err}"
        );
        assert!(validate_output(&make("has space")).is_err());
        assert!(validate_output(&make("tab\there")).is_err());
    }

    #[test]
    fn validate_output_webrtc_audio_encode_only_opus() {
        use crate::config::models::{
            AudioEncodeConfig, OutputConfig, WebrtcOutputConfig, WebrtcOutputMode,
        };
        let make = |codec: &str, video_only: bool| OutputConfig::Webrtc(WebrtcOutputConfig {
            active: true,
            group: None,
            id: "wrtc1".into(),
            name: "wrtc 1".into(),
            mode: WebrtcOutputMode::WhepServer,
            whip_url: None,
            bearer_token: None,
            accept_self_signed_cert: None,
            cert_fingerprint: None,
            max_viewers: Some(10),
            public_ip: None,
            video_only,
            program_number: None,
            audio_encode: Some(AudioEncodeConfig {
                codec: codec.into(),
                bitrate_kbps: None,
                sample_rate: None,
                channels: None,
                silent_fallback: false,
            }),
            transcode: None,
            video_encode: None,
        });
        // Opus + audio MID negotiated → OK.
        assert!(validate_output(&make("opus", false)).is_ok());
        // Opus + video_only=true → must reject (no audio MID).
        let err = validate_output(&make("opus", true)).unwrap_err().to_string();
        assert!(err.contains("video_only=false"), "got: {err}");
        // AAC on WebRTC → must reject.
        assert!(validate_output(&make("aac_lc", false)).is_err());
    }

    #[test]
    fn validate_output_webrtc_video_encode_rejects_hevc() {
        // WebRTC browsers don't decode HEVC; the validator must reject
        // H.265-targeting encoders so operators get a clear error at
        // save time rather than a silently-broken viewer.
        use crate::config::models::{
            OutputConfig, VideoEncodeConfig, WebrtcOutputConfig, WebrtcOutputMode,
        };
        let make = |codec: &str| OutputConfig::Webrtc(WebrtcOutputConfig {
            active: true,
            group: None,
            id: "wrtc1".into(),
            name: "wrtc 1".into(),
            mode: WebrtcOutputMode::WhepServer,
            whip_url: None,
            bearer_token: None,
            accept_self_signed_cert: None,
            cert_fingerprint: None,
            max_viewers: Some(10),
            public_ip: None,
            video_only: false,
            program_number: None,
            audio_encode: None,
            transcode: None,
            video_encode: Some(VideoEncodeConfig {
                codec: codec.into(),
                width: None,
                height: None,
                fps_num: None,
                fps_den: None,
                bitrate_kbps: Some(4000),
                gop_size: None,
                preset: None,
                profile: None,
                chroma: None,
                bit_depth: None,
                rate_control: None,
                crf: None,
                max_bitrate_kbps: None,
                bframes: None,
                refs: None,
                level: None,
                tune: None,
                color_primaries: None,
                color_transfer: None,
                color_matrix: None,
                color_range: None,
            }),
        });
        // H.264 backends are accepted only when the feature is compiled
        // in; on the default build this still passes through `validate_video_encode`
        // cleanly when the backend matches the compile-time features.
        // The HEVC rejection is purely a WebRTC rule, so we can exercise
        // it regardless of which backend features are enabled — the HEVC
        // rule triggers before backend-feature validation.
        let err = validate_output(&make("x265")).unwrap_err().to_string();
        assert!(
            err.contains("WebRTC browsers only decode H.264"),
            "expected HEVC rejection, got: {err}"
        );
        let err = validate_output(&make("hevc_nvenc")).unwrap_err().to_string();
        assert!(
            err.contains("WebRTC browsers only decode H.264"),
            "expected HEVC rejection, got: {err}"
        );
        let err = validate_output(&make("hevc_qsv")).unwrap_err().to_string();
        assert!(
            err.contains("WebRTC browsers only decode H.264"),
            "expected HEVC rejection, got: {err}"
        );
    }

    #[test]
    fn validate_output_webrtc_cert_fingerprint_format() {
        use crate::config::models::{OutputConfig, WebrtcOutputConfig, WebrtcOutputMode};
        let make = |fp: Option<String>| OutputConfig::Webrtc(WebrtcOutputConfig {
            active: true,
            group: None,
            id: "wrtc1".into(),
            name: "wrtc 1".into(),
            mode: WebrtcOutputMode::WhipClient,
            whip_url: Some("https://example.com/whip".into()),
            bearer_token: None,
            accept_self_signed_cert: Some(false),
            cert_fingerprint: fp,
            max_viewers: None,
            public_ip: None,
            video_only: true,
            program_number: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        });
        // Valid colon-separated 32-byte hex.
        let valid_fp = "ab:cd:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab".to_string();
        assert!(validate_output(&make(Some(valid_fp))).is_ok());
        // Bare hex form should also be accepted.
        let valid_bare = "abcd012345678 9abcdef0123456789abcdef0123456789abcdef0123456789ab".to_string();
        assert!(validate_output(&make(Some(valid_bare))).is_ok());
        // Wrong length → reject.
        assert!(validate_output(&make(Some("ab:cd:ef".into()))).is_err());
        // Non-hex → reject.
        assert!(
            validate_output(&make(Some("zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz:zz".into()))).is_err()
        );
        // None → OK (field is optional).
        assert!(validate_output(&make(None)).is_ok());
    }

    #[test]
    fn validate_video_encode_qsv_recognised() {
        // The codec parser must accept `h264_qsv` and `hevc_qsv` as valid
        // names. The exact error path depends on the build:
        //   - feature off (default) → feature-gate error mentioning
        //     `video-encoder-qsv`
        //   - feature on → either accept or surface a QSV-specific
        //     restriction (10-bit on h264_qsv, yuv444p on either)
        // What must NEVER happen is the codec being treated as unknown.
        use crate::config::models::VideoEncodeConfig;
        let make = |codec: &str, bit_depth: Option<u8>, chroma: Option<&str>| VideoEncodeConfig {
            codec: codec.into(),
            width: None,
            height: None,
            fps_num: None,
            fps_den: None,
            bitrate_kbps: Some(4000),
            gop_size: None,
            preset: None,
            profile: None,
            chroma: chroma.map(str::to_string),
            bit_depth,
            rate_control: None,
            crf: None,
            max_bitrate_kbps: None,
            bframes: None,
            refs: None,
            level: None,
            tune: None,
            color_primaries: None,
            color_transfer: None,
            color_matrix: None,
            color_range: None,
        };
        let assert_recognised = |result: Result<(), anyhow::Error>, label: &str| match result {
            Ok(_) => {} // feature on, params OK — fine
            Err(e) => {
                let s = e.to_string();
                assert!(
                    !s.contains("is not recognised"),
                    "{label}: codec must be a recognised name, got: {s}"
                );
            }
        };
        assert_recognised(
            validate_video_encode(&make("h264_qsv", Some(8), Some("yuv420p")), "ctx"),
            "h264_qsv 8-bit yuv420p",
        );
        assert_recognised(
            validate_video_encode(&make("hevc_qsv", Some(8), Some("yuv420p")), "ctx"),
            "hevc_qsv 8-bit yuv420p",
        );
        // When the QSV feature is on, the QSV-specific restrictions
        // should fire on bad params. Skip this branch in the default
        // (feature-off) build because the feature-gate fires first.
        #[cfg(feature = "video-encoder-qsv")]
        {
            let err = validate_video_encode(&make("h264_qsv", Some(10), None), "ctx")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("h264_qsv does not support 10-bit"),
                "expected QSV 10-bit rejection, got: {err}"
            );
            let err = validate_video_encode(&make("hevc_qsv", None, Some("yuv444p")), "ctx")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("QSV backends do not support chroma=yuv444p"),
                "expected QSV chroma rejection, got: {err}"
            );
        }
    }

    // ── Port conflict tests ──────────────────────────────────────────

    /// Helper: build a minimal config with tunnels only (no flows).
    /// Uses a non-conflicting server port (18888) so tunnel port tests are isolated.
    fn make_tunnel_config(tunnels: Vec<crate::tunnel::TunnelConfig>) -> AppConfig {
        let mut config = AppConfig::default();
        config.server.listen_port = 18888;
        config.tunnels = tunnels;
        config
    }

    fn make_test_tunnel(
        name: &str,
        local_addr: &str,
        protocol: crate::tunnel::config::TunnelProtocol,
        direction: crate::tunnel::config::TunnelDirection,
    ) -> crate::tunnel::TunnelConfig {
        crate::tunnel::TunnelConfig {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            enabled: true,
            protocol,
            mode: crate::tunnel::config::TunnelMode::Relay,
            direction,
            local_addr: local_addr.to_string(),
            relay_addrs: vec!["127.0.0.1:4433".to_string()],
            relay_addr: None,
            max_rtt_failback_increase_ms: None,
            tunnel_encryption_key: None,
            tunnel_bind_secret: None,
            peer_addr: None,
            direct_listen_addr: None,
            tunnel_psk: None,
            tls_cert_pem: None,
            tls_key_pem: None,
        }
    }

    #[test]
    fn port_conflict_two_tunnels_same_port() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        let config = make_tunnel_config(vec![
            make_test_tunnel("t1", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
            make_test_tunnel("t2", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
        ]);
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
        assert!(err.contains("8080"), "got: {err}");
    }

    #[test]
    fn port_conflict_tunnel_vs_server() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        let mut config = AppConfig::default();
        config.server.listen_port = 8080;
        config.server.listen_addr = "0.0.0.0".to_string();
        config.tunnels.push(make_test_tunnel(
            "t1",
            "0.0.0.0:8080",
            TunnelProtocol::Tcp,
            TunnelDirection::Egress,
        ));
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
        assert!(err.contains("Edge API server"), "got: {err}");
    }

    #[test]
    fn port_conflict_tunnel_vs_srt_input() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        let mut config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:6000");
        // Replace the RTP input with an SRT listener on port 9000
        config.inputs[0] = InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "SRT In".to_string(),
            config: InputConfig::Srt(SrtInputConfig {
                mode: SrtMode::Listener,
                local_addr: Some("0.0.0.0:9000".to_string()),
                remote_addr: None,
                latency_ms: 200,
                recv_latency_ms: None,
                peer_latency_ms: None,
                peer_idle_timeout_secs: 30,
                passphrase: None,
                aes_key_len: None,
                crypto_mode: None,
                max_rexmit_bw: None,
                stream_id: None,
                packet_filter: None,
                max_bw: None,
                input_bw: None,
                overhead_bw: None,
                enforced_encryption: None,
                connect_timeout_secs: None,
                flight_flag_size: None,
                send_buffer_size: None,
                recv_buffer_size: None,
                ip_tos: None,
                retransmit_algo: None,
                send_drop_delay: None,
                loss_max_ttl: None,
                km_refresh_rate: None,
                km_pre_announce: None,
                payload_size: None,
                mss: None,
                tlpkt_drop: None,
                ip_ttl: None,
                redundancy: None,
                bonding: None,
                transport_mode: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        };
        // Add a UDP tunnel egress on the same port
        config.tunnels.push(make_test_tunnel(
            "t1",
            "0.0.0.0:9000",
            TunnelProtocol::Udp,
            TunnelDirection::Egress,
        ));
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
        assert!(err.contains("9000"), "got: {err}");
    }

    #[test]
    fn no_port_conflict_different_protocols() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        // TCP tunnel on 8080 + UDP tunnel on 8080 — different protocols, no conflict
        let config = make_tunnel_config(vec![
            make_test_tunnel("t1", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
            make_test_tunnel("t2", "0.0.0.0:8080", TunnelProtocol::Udp, TunnelDirection::Egress),
        ]);
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn no_port_conflict_multicast_addresses() {
        // Two RTP inputs on the same multicast address — valid with SO_REUSEADDR
        let mut config = AppConfig::default();
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-1".to_string(),
            name: "Mcast 1".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        config.inputs.push(InputDefinition {
            active: true,
            group: None,
            id: "in-2".to_string(),
            name: "Mcast 2".to_string(),
            config: InputConfig::Rtp(RtpInputConfig {
                bind_addr: "239.1.1.1:5000".to_string(),
                interface_addr: None,
                source_addr: None,
                fec_decode: None,
                allowed_sources: None,
                allowed_payload_types: None,
                max_bitrate_mbps: None,
                tr07_mode: None,
                redundancy: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            }),
        });
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn no_port_conflict_disabled_tunnel() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        let mut t2 = make_test_tunnel("t2", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress);
        t2.enabled = false; // disabled tunnels should be skipped
        let config = make_tunnel_config(vec![
            make_test_tunnel("t1", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
            t2,
        ]);
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn no_port_conflict_ingress_tunnel_does_not_bind_local_addr() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        // Ingress tunnels forward to local_addr (connect, not bind), so no conflict
        // with an egress tunnel on the same port.
        let config = make_tunnel_config(vec![
            make_test_tunnel("egress", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
            make_test_tunnel("ingress", "127.0.0.1:8080", TunnelProtocol::Tcp, TunnelDirection::Ingress),
        ]);
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn port_conflict_wildcard_vs_specific_ip() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};

        // 0.0.0.0:8080 conflicts with 192.168.1.1:8080
        let config = make_tunnel_config(vec![
            make_test_tunnel("t1", "0.0.0.0:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
            make_test_tunnel("t2", "192.168.1.1:8080", TunnelProtocol::Tcp, TunnelDirection::Egress),
        ]);
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
    }

    /// RTP output `bind_addr` (when explicitly pinned) must be checked against
    /// other listeners. Regression for the gap where `_ => {}` skipped every
    /// non-SRT output in `validate_port_conflicts`.
    #[test]
    fn port_conflict_rtp_output_bind_collides_with_input() {
        let mut config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:5004");
        if let OutputConfig::Rtp(o) = &mut config.outputs[0] {
            o.bind_addr = Some("0.0.0.0:5000".to_string());
        }
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
        assert!(err.contains("UDP"), "should mention protocol: {err}");
        assert!(err.contains("5000"), "should mention port: {err}");
    }

    /// UDP output with `bind_addr` set to the same port as another input must
    /// be flagged.
    #[test]
    fn port_conflict_udp_output_bind_vs_other_udp_output_bind() {
        let mut config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:5004");
        config.outputs.push(OutputConfig::Udp(UdpOutputConfig {
            id: "out-udp-a".into(),
            name: "Udp A".into(),
            active: true,
            group: None,
            dest_addr: "127.0.0.1:6000".into(),
            bind_addr: Some("0.0.0.0:7777".into()),
            interface_addr: None,
            dscp: 46,
            program_number: None,
            pid_map: None,
            transport_mode: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        config.outputs.push(OutputConfig::Udp(UdpOutputConfig {
            id: "out-udp-b".into(),
            name: "Udp B".into(),
            active: true,
            group: None,
            dest_addr: "127.0.0.1:6001".into(),
            bind_addr: Some("0.0.0.0:7777".into()),
            interface_addr: None,
            dscp: 46,
            program_number: None,
            pid_map: None,
            transport_mode: None,
            delay: None,
            audio_encode: None,
            transcode: None,
            video_encode: None,
        }));
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("Port conflict"), "got: {err}");
        assert!(err.contains("7777"), "got: {err}");
    }

    /// Ephemeral RTP output bind (defaulting `bind_addr = None` or port 0)
    /// must NOT collide with anything — confirms the early-return on port 0.
    #[test]
    fn no_port_conflict_for_ephemeral_output_bind() {
        let mut config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:5004");
        if let OutputConfig::Rtp(o) = &mut config.outputs[0] {
            o.bind_addr = Some("0.0.0.0:0".to_string());
        }
        validate_config(&config).expect("ephemeral output bind must not conflict");
    }

    /// Error message must include the entity ID so the manager UI can
    /// attribute the conflict to a specific input/output.
    #[test]
    fn port_conflict_message_includes_entity_id() {
        let mut config = make_config_with_rtp("0.0.0.0:5000", "127.0.0.1:5004");
        if let OutputConfig::Rtp(o) = &mut config.outputs[0] {
            o.bind_addr = Some("0.0.0.0:5000".to_string());
        }
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("[in-1]"), "should include input id: {err}");
        assert!(err.contains("[out-1]"), "should include output id: {err}");
    }

    // ── Dual-relay validation tests ──

    fn make_dual_relay_tunnel(relay_addrs: Vec<&str>) -> crate::tunnel::TunnelConfig {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};
        let mut t = make_test_tunnel(
            "dual",
            "127.0.0.1:9000",
            TunnelProtocol::Udp,
            TunnelDirection::Ingress,
        );
        t.relay_addrs = relay_addrs.into_iter().map(str::to_string).collect();
        t.relay_addr = None;
        // encryption key required for relay mode
        t.tunnel_encryption_key = Some("a".repeat(64));
        t
    }

    #[test]
    fn dual_relay_accepts_two_valid_addresses() {
        let t = make_dual_relay_tunnel(vec!["127.0.0.1:4433", "127.0.0.1:4434"]);
        validate_tunnel(&t).unwrap();
    }

    #[test]
    fn dual_relay_accepts_single_address() {
        let t = make_dual_relay_tunnel(vec!["127.0.0.1:4433"]);
        validate_tunnel(&t).unwrap();
    }

    #[test]
    fn dual_relay_rejects_empty_list() {
        let t = make_dual_relay_tunnel(vec![]);
        let err = validate_tunnel(&t).unwrap_err().to_string();
        assert!(err.contains("at least one relay address"), "got: {err}");
    }

    #[test]
    fn dual_relay_rejects_three_addresses() {
        let t =
            make_dual_relay_tunnel(vec!["127.0.0.1:4433", "127.0.0.1:4434", "127.0.0.1:4435"]);
        let err = validate_tunnel(&t).unwrap_err().to_string();
        assert!(err.contains("at most 2 relay addresses"), "got: {err}");
    }

    #[test]
    fn dual_relay_rejects_duplicate_addresses() {
        let t = make_dual_relay_tunnel(vec!["127.0.0.1:4433", "127.0.0.1:4433"]);
        let err = validate_tunnel(&t).unwrap_err().to_string();
        assert!(err.contains("duplicate address"), "got: {err}");
    }

    #[test]
    fn dual_relay_rejects_invalid_socket_addr() {
        let t = make_dual_relay_tunnel(vec!["127.0.0.1:4433", "not-a-host:port"]);
        let err = validate_tunnel(&t).unwrap_err().to_string();
        assert!(err.contains("not a valid socket address"), "got: {err}");
    }

    #[test]
    fn dual_relay_rejects_out_of_range_rtt_gate() {
        let mut t = make_dual_relay_tunnel(vec!["127.0.0.1:4433", "127.0.0.1:4434"]);
        t.max_rtt_failback_increase_ms = Some(10_000);
        let err = validate_tunnel(&t).unwrap_err().to_string();
        assert!(
            err.contains("max_rtt_failback_increase_ms must be <= 5000"),
            "got: {err}"
        );
    }

    #[test]
    fn dual_relay_accepts_rtt_gate_at_bound() {
        let mut t = make_dual_relay_tunnel(vec!["127.0.0.1:4433", "127.0.0.1:4434"]);
        t.max_rtt_failback_increase_ms = Some(5000);
        validate_tunnel(&t).unwrap();
    }

    #[test]
    fn legacy_relay_addr_still_accepted() {
        use crate::tunnel::config::{TunnelDirection, TunnelProtocol};
        let mut t = make_test_tunnel(
            "legacy",
            "127.0.0.1:9000",
            TunnelProtocol::Udp,
            TunnelDirection::Ingress,
        );
        t.relay_addrs = vec![];
        t.relay_addr = Some("127.0.0.1:4433".to_string());
        t.tunnel_encryption_key = Some("a".repeat(64));
        validate_tunnel(&t).unwrap();
    }

    // --- SSM (RFC 3678) source-specific multicast helper tests ---

    #[test]
    fn ssm_source_none_is_always_ok() {
        assert!(validate_ssm_source("239.1.1.1:5000", None, "test").is_ok());
        assert!(validate_ssm_source("0.0.0.0:5000", None, "test").is_ok());
        assert!(validate_ssm_source("[ff7e::1]:5000", None, "test").is_ok());
    }

    #[test]
    fn ssm_v4_unicast_source_accepted() {
        validate_ssm_source("232.1.2.3:5000", Some("10.0.0.5"), "v4-ssm").unwrap();
    }

    #[test]
    fn ssm_v6_unicast_source_accepted() {
        validate_ssm_source("[ff3e::1]:5000", Some("2001:db8::5"), "v6-ssm").unwrap();
    }

    #[test]
    fn ssm_source_rejected_on_unicast_bind() {
        let err = validate_ssm_source("0.0.0.0:5000", Some("10.0.0.5"), "ctx")
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid when bind_addr is multicast"), "got: {err}");
    }

    #[test]
    fn ssm_multicast_source_rejected() {
        let err = validate_ssm_source("232.1.2.3:5000", Some("232.1.2.4"), "ctx")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a unicast address"), "got: {err}");
    }

    #[test]
    fn ssm_family_mismatch_rejected() {
        let err = validate_ssm_source("232.1.2.3:5000", Some("2001:db8::5"), "ctx")
            .unwrap_err()
            .to_string();
        assert!(err.contains("same address family"), "got: {err}");
    }
}
