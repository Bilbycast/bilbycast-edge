// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

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
    /// Top-level input definitions. Each input is independently configurable
    /// and can be referenced by flows via `input_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputDefinition>,
    /// Top-level output definitions. Each output is independently configurable
    /// and can be referenced by flows via `output_ids`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputConfig>,
    /// Flows connect one input to one or more outputs by reference.
    #[serde(default)]
    pub flows: Vec<FlowConfig>,
    /// Optional system resource monitoring thresholds (CPU, RAM).
    /// When configured, the node samples system resources and fires events
    /// when thresholds are exceeded. See [`ResourceLimitConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<ResourceLimitConfig>,
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
            version: 2,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            resource_limits: None,
            flows: Vec::new(),
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
        }
    }
}

/// A standalone input definition, referenceable by flows.
///
/// Wraps an [`InputConfig`] with an ID and human-readable name so that
/// inputs can be created, listed, and managed independently of flows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDefinition {
    /// Unique identifier for this input (max 64 chars).
    pub id: String,
    /// Human-readable name (max 256 chars).
    pub name: String,
    /// Whether this input is currently active. Within a flow, at most one
    /// input may be active at a time; activating one automatically passivates
    /// the others. All inputs run regardless of `active` (warm passive), but
    /// only the active input publishes packets to the flow's broadcast
    /// channel. Defaults to `true` for convenience when creating single-input
    /// flows.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature. Inputs with the same group can be
    /// activated together across multiple edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The protocol-specific input configuration (flattened into the same JSON object).
    #[serde(flatten)]
    pub config: InputConfig,
}

/// A fully resolved flow — all references replaced with concrete definitions.
///
/// This is what the engine layer receives. The engine never sees bare ID
/// references — only concrete `InputDefinition` / `OutputConfig` values.
#[derive(Debug, Clone)]
pub struct ResolvedFlow {
    /// The flow configuration (metadata only — `input_ids` / `output_ids` are references).
    pub config: FlowConfig,
    /// The resolved input definitions, in the order declared on the flow.
    /// At most one is marked `active = true`. An empty vector means the
    /// flow has no inputs (output-only / test flow).
    pub inputs: Vec<InputDefinition>,
    /// The resolved output configurations (all of them, including passive).
    pub outputs: Vec<OutputConfig>,
}

impl ResolvedFlow {
    /// Return the currently active input, if any. A flow is allowed to have
    /// zero active inputs (idle) — the engine keeps outputs running but the
    /// broadcast channel stays empty until an operator activates one.
    pub fn active_input(&self) -> Option<&InputDefinition> {
        self.inputs.iter().find(|i| i.active)
    }

}

impl AppConfig {
    /// Resolve a flow's `input_ids` and `output_ids` references into concrete configs.
    ///
    /// Returns an error if any referenced input or output ID does not exist.
    pub fn resolve_flow(&self, flow: &FlowConfig) -> anyhow::Result<ResolvedFlow> {
        let mut inputs = Vec::with_capacity(flow.input_ids.len());
        for input_id in &flow.input_ids {
            let def = self
                .inputs
                .iter()
                .find(|i| i.id == *input_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Flow '{}' references input '{}' which does not exist",
                        flow.id,
                        input_id
                    )
                })?;
            inputs.push(def.clone());
        }

        let mut outputs = Vec::with_capacity(flow.output_ids.len());
        for output_id in &flow.output_ids {
            let out = self
                .outputs
                .iter()
                .find(|o| o.id() == output_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Flow '{}' references output '{}' which does not exist",
                        flow.id,
                        output_id
                    )
                })?;
            outputs.push(out.clone());
        }

        Ok(ResolvedFlow {
            config: flow.clone(),
            inputs,
            outputs,
        })
    }

    /// Find the flow (if any) that references the given input ID.
    pub fn flow_using_input(&self, input_id: &str) -> Option<&FlowConfig> {
        self.flows
            .iter()
            .find(|f| f.input_ids.iter().any(|id| id == input_id))
    }

    /// Find the flow (if any) that references the given output ID.
    pub fn flow_using_output(&self, output_id: &str) -> Option<&FlowConfig> {
        self.flows
            .iter()
            .find(|f| f.output_ids.iter().any(|id| id == output_id))
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

/// A Flow connects one or more inputs to one or more outputs by reference.
///
/// Flows contain only references (`input_ids`, `output_ids`) plus flow-level
/// metadata. The actual input and output configurations live in `AppConfig.inputs`
/// and `AppConfig.outputs` respectively. A flow may have several inputs but at
/// any moment at most one is active (publishing packets to the broadcast
/// channel); activating one automatically passivates its siblings. All active
/// outputs run in parallel; passive outputs exist in the config but have no
/// running task.
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
    /// Enable thumbnail generation for visual flow preview.
    /// Default: true. Uses in-process libavcodec (no external ffmpeg required).
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
    /// References to top-level input definitions by ID. A flow may have
    /// multiple inputs but at most one is active (publishing to the broadcast
    /// channel) at a time. Empty means the flow has no inputs (output-only /
    /// test flow).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_ids: Vec<String>,
    /// References to top-level output definitions by ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_ids: Vec<String>,
    /// Optional per-output PID-bus assembly plan (Phase 3+ schema).
    /// When set, every TS output on the flow emits an MPTS/SPTS assembled
    /// from elementary streams pulled off any of the flow's inputs, rather
    /// than forwarding the active input's stream as a whole. Absent on
    /// legacy flows, which fall through to today's passthrough runtime.
    ///
    /// The runtime that consumes this field lands in phases 4–5; until
    /// then a flow carrying a non-`None` assembly is refused at start
    /// time with a Critical event (no silent misbehaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly: Option<FlowAssembly>,
}

/// Per-flow assembly plan. Describes how each of the flow's TS outputs
/// should build its egress TS from elementary streams pulled off any of
/// the flow's inputs. Non-TS outputs (RTMP/WebRTC/CMAF) ignore this
/// block and continue to pull ES by codec family from the bus.
///
/// `kind = Passthrough` is the simple case and exists so the "just take
/// input A and forward it" flow doesn't need any assembly structure at
/// all — the runtime treats it identically to a legacy flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowAssembly {
    /// Shape of the egress TS.
    pub kind: AssemblyKind,
    /// PCR reference. For `Spts`, this is the authoritative (or sole)
    /// reference — required unless the single program carries its own
    /// `pcr_source`. For `Mpts`, this is a **convenience default**
    /// applied to any program that doesn't set
    /// [`AssembledProgram::pcr_source`] itself. Ignored for
    /// `Passthrough` (PCR is carried through from the input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_source: Option<PcrSource>,
    /// Programs to synthesise. For `Spts`/`Passthrough` this must contain
    /// exactly one program; for `Mpts` one or more. Program numbers must
    /// be unique within the flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<AssembledProgram>,
}

/// Egress TS shape selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssemblyKind {
    /// Single-program TS. Exactly one program in `programs`.
    Spts,
    /// Multi-program TS. One or more programs in `programs`.
    Mpts,
    /// Legacy mode — no assembly, the flow's outputs forward the active
    /// input's bytes. Runtime-equivalent to `assembly = None`.
    Passthrough,
}

/// PCR reference input. The runtime forwards this PID's PCR packets
/// from the named input, remapped onto the output PMT's PCR_PID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PcrSource {
    /// Flow-local input ID (must be in `FlowConfig.input_ids`).
    pub input_id: String,
    /// Source PID carrying PCR on that input.
    pub pid: u16,
}

/// One program in the assembled egress TS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssembledProgram {
    /// MPEG-TS program_number (1..=65535; 0 is reserved for the NIT).
    pub program_number: u16,
    /// Optional SDT `service_name` — shipped as a short label in the
    /// manager UI and (in a later phase) synthesised into an SDT table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    /// PMT PID for this program on the egress side. Must be unique
    /// across programs within the assembly (PAT/PMT constraint).
    pub pmt_pid: u16,
    /// Optional per-program PCR reference. Required for `Mpts`
    /// assemblies (each program's PMT must name a `PCR_PID` within
    /// its own ES set per H.222.0). For `Spts`, when unset, the
    /// flow-level [`FlowAssembly::pcr_source`] is the fallback. The
    /// referenced `(input_id, pid)` must resolve to one of this
    /// program's slots post-essence-resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_source: Option<PcrSource>,
    /// Elementary streams composing the program. Each slot is
    /// independently switchable at runtime (Phase 7).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<AssembledStream>,
}

/// One elementary-stream slot within an assembled program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssembledStream {
    /// Where to pull bytes from.
    pub source: SlotSource,
    /// Output PID for this ES on the egress side. Must be unique within
    /// the program. Use 0x0010..=0x1FFE.
    pub out_pid: u16,
    /// Output PMT `stream_type`. When sourced from a PID explicitly, the
    /// assembler will cross-check against the source PMT and log a
    /// warning on mismatch; when sourced by essence kind, the assembler
    /// picks the stream_type from the source PMT.
    pub stream_type: u8,
    /// Optional human label surfaced in UI + events (e.g. `"English"`,
    /// `"Isolated Camera 2"`). Round-tripped only; runtime doesn't use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Where the bytes for a slot come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SlotSource {
    /// Explicit PID on a named input. Used when the operator has picked
    /// a concrete PID from that input's PSI catalogue.
    Pid {
        input_id: String,
        source_pid: u16,
    },
    /// Broad essence selector — the assembler picks the first stream of
    /// the given kind from the named input's PMT. Useful when the input
    /// is single-program and the operator just wants "its video".
    Essence {
        input_id: String,
        kind: EssenceKind,
    },
    /// Hitless SMPTE 2022-7-style pair. Both legs carry the same ES;
    /// the merger downstream dedupes by RTP sequence. Either nested
    /// source may itself be another `Pid` or `Essence` — nested
    /// `Hitless` is rejected at validation time.
    Hitless {
        primary: Box<SlotSource>,
        backup: Box<SlotSource>,
    },
}

/// Broad kind of essence on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EssenceKind {
    Video,
    Audio,
    Subtitle,
    Data,
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

/// System resource monitoring and threshold configuration.
///
/// When configured at the node level, the edge periodically samples CPU and RAM
/// usage and fires events when thresholds are exceeded. Optionally gates new
/// flow creation when resources are critical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimitConfig {
    /// CPU usage warning threshold (percent, 0-100). Default: 80.
    #[serde(default = "default_cpu_warning")]
    pub cpu_warning_percent: f64,
    /// CPU usage critical threshold (percent, 0-100). Default: 95.
    #[serde(default = "default_cpu_critical")]
    pub cpu_critical_percent: f64,
    /// RAM usage warning threshold (percent, 0-100). Default: 80.
    #[serde(default = "default_ram_warning")]
    pub ram_warning_percent: f64,
    /// RAM usage critical threshold (percent, 0-100). Default: 95.
    #[serde(default = "default_ram_critical")]
    pub ram_critical_percent: f64,
    /// Action when critical threshold is exceeded.
    #[serde(default)]
    pub critical_action: ResourceLimitAction,
    /// Seconds the metric must continuously exceed the threshold before
    /// triggering. Default: 10 seconds.
    #[serde(default = "default_resource_grace")]
    pub grace_period_secs: u32,
}

/// Action to take when a system resource critical threshold is exceeded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimitAction {
    /// Raise events only (default). Flows continue operating normally.
    #[default]
    Alarm,
    /// Prevent new flows from starting while any resource is critical.
    GateFlows,
}

fn default_cpu_warning() -> f64 {
    80.0
}
fn default_cpu_critical() -> f64 {
    95.0
}
fn default_ram_warning() -> f64 {
    80.0
}
fn default_ram_critical() -> f64 {
    95.0
}
fn default_resource_grace() -> u32 {
    10
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
    /// Receive RIST Simple Profile (TR-06-1:2020) — reliable RTP transport
    /// with RTCP NACK-based retransmission. Interoperable with librist
    /// `ristsender` / `ristreceiver`. Binds an even RTP port P and RTCP
    /// port P+1 locally; learns the peer's RTCP address dynamically.
    #[serde(rename = "rist")]
    Rist(RistInputConfig),
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
    /// Receive SMPTE ST 2110-20 uncompressed video over RTP (RFC 4175).
    /// Ingress frames are decoded from the wire, fed into an in-process
    /// H.264/HEVC encoder (configured via `video_encode`), and published as
    /// MPEG-TS packets onto the flow's broadcast channel. An encoder backend
    /// feature (`video-encoder-x264` / `-x265` / `-nvenc`) must be compiled
    /// in; validation rejects the input otherwise.
    #[serde(rename = "st2110_20")]
    St2110_20(St2110VideoInputConfig),
    /// Receive SMPTE ST 2110-23 (single video essence across multiple -20
    /// sub-streams). Binds N receivers, reassembles the full frame per the
    /// configured partition mode, then funnels through the same encode path
    /// as ST 2110-20.
    #[serde(rename = "st2110_23")]
    St2110_23(St2110_23InputConfig),
    /// Receive RFC 3551 PCM audio over RTP/UDP without ST 2110 constraints.
    /// No PTP requirement, no RFC 7273 timing reference, no clock_domain
    /// advertising in NMOS. Useful for radio contribution feeds, talkback,
    /// and any general PCM-over-RTP source where ST 2110 is overkill.
    #[serde(rename = "rtp_audio")]
    RtpAudio(RtpAudioInputConfig),
    /// Receive a bonded flow across N paths (UDP / QUIC / RIST), reassemble
    /// in bond-sequence order, and publish payloads onto the flow's
    /// broadcast channel. Intended for aggregating cellular / satellite /
    /// ethernet links like a Peplink SpeedFusion tunnel, but media-aware.
    #[serde(rename = "bonded")]
    Bonded(BondedInputConfig),
    /// Synthetic test-pattern input: generates SMPTE 75% colour bars +
    /// optional 1 kHz tone in-process, encodes to H.264 + AAC, and
    /// publishes as MPEG-TS onto the flow broadcast channel. Useful for
    /// pre-live validation (confirm the pipeline works before the real
    /// source is patched in), downstream troubleshooting (isolate whether
    /// the problem is upstream of the edge), and idle fill.
    #[serde(rename = "test_pattern")]
    TestPattern(TestPatternInputConfig),
}

/// Configuration for an in-process synthetic test-pattern input.
///
/// Requires the edge binary to be built with `video-encoder-x264` (H.264
/// encoding of the bars frame) and `fdk-aac` (AAC encoding of the tone)
/// features. If either is missing the input fails to start with a clear
/// event rather than silently degrading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestPatternInputConfig {
    /// Video width in pixels. Default 1280. Must be divisible by 2.
    #[serde(default = "default_tp_width")]
    pub width: u16,
    /// Video height in pixels. Default 720. Must be divisible by 2.
    #[serde(default = "default_tp_height")]
    pub height: u16,
    /// Frame rate in frames per second. Default 25. Range 1..=60.
    #[serde(default = "default_tp_fps")]
    pub fps: u16,
    /// Target video bitrate in kbit/s. Default 2000.
    #[serde(default = "default_tp_video_bitrate")]
    pub video_bitrate_kbps: u32,
    /// When true, generate and encode a 1 kHz sine tone. When false,
    /// emit a video-only TS stream.
    #[serde(default = "default_true")]
    pub audio_enabled: bool,
    /// Audio tone frequency in Hz. Default 1000. Range 50..=8000.
    #[serde(default = "default_tp_tone_hz")]
    pub tone_hz: f32,
    /// Audio level in dBFS (negative). Default -20 dBFS (broadcast reference).
    #[serde(default = "default_tp_tone_dbfs")]
    pub tone_dbfs: f32,
}

fn default_tp_width() -> u16 { 1280 }
fn default_tp_height() -> u16 { 720 }
fn default_tp_fps() -> u16 { 25 }
fn default_tp_video_bitrate() -> u32 { 2000 }
fn default_tp_tone_hz() -> f32 { 1000.0 }
fn default_tp_tone_dbfs() -> f32 { -20.0 }

impl InputConfig {
    /// Returns the type name string (e.g. "srt", "rtp", "udp").
    pub fn type_name(&self) -> &'static str {
        match self {
            InputConfig::Rtp(_) => "rtp",
            InputConfig::Udp(_) => "udp",
            InputConfig::Srt(_) => "srt",
            InputConfig::Rist(_) => "rist",
            InputConfig::Rtmp(_) => "rtmp",
            InputConfig::Rtsp(_) => "rtsp",
            InputConfig::Webrtc(_) => "webrtc",
            InputConfig::Whep(_) => "whep",
            InputConfig::St2110_30(_) => "st2110_30",
            InputConfig::St2110_31(_) => "st2110_31",
            InputConfig::St2110_40(_) => "st2110_40",
            InputConfig::St2110_20(_) => "st2110_20",
            InputConfig::St2110_23(_) => "st2110_23",
            InputConfig::RtpAudio(_) => "rtp_audio",
            InputConfig::Bonded(_) => "bonded",
            InputConfig::TestPattern(_) => "test_pattern",
        }
    }

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
            | InputConfig::Rist(_)
            | InputConfig::Rtmp(_)
            | InputConfig::Rtsp(_)
            | InputConfig::Webrtc(_)
            | InputConfig::Whep(_)
            // ST 2110-20/-23 decode RFC 4175 on ingress and publish the
            // flow as encoded H.264/HEVC MPEG-TS into the broadcast channel,
            // so downstream consumers treat them as TS carriers.
            | InputConfig::St2110_20(_)
            | InputConfig::St2110_23(_)
            // Bonded inputs carry whatever the sender bonded — in the
            // common broadcast case that's MPEG-TS, so treat as a TS
            // carrier. Downstream analysers can still be turned off via
            // `media_analysis: false` on the flow.
            | InputConfig::Bonded(_)
            // Test pattern publishes encoded H.264 + AAC in MPEG-TS.
            | InputConfig::TestPattern(_) => true,
            // PCM-only inputs become TS carriers when `audio_encode` is set —
            // the input task muxes the encoded audio into an audio-only TS.
            InputConfig::St2110_30(c) => c.audio_encode.is_some(),
            InputConfig::RtpAudio(c) => c.audio_encode.is_some(),
            InputConfig::St2110_31(_) | InputConfig::St2110_40(_) => false,
        }
    }

    /// Returns true when the input produces MPEG-TS on the broadcast channel.
    ///
    /// This is a synonym of [`Self::is_ts_carrier`] kept as a separate name so
    /// call sites that check shape compatibility for a flow (where the question
    /// is "does this input *produce* TS?") read clearly. Both methods return
    /// the same value.
    pub fn produces_ts(&self) -> bool {
        self.is_ts_carrier()
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
    /// Optional audio re-encode applied at ingress. When set, the input task
    /// decodes the source audio ES, optionally transcodes planar PCM via
    /// `transcode`, re-encodes to the configured codec, and re-muxes the
    /// replacement ES back into the broadcast TS before fan-out. Identical
    /// semantics to the output-side `audio_encode` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode (channel shuffle / sample-rate / bit-depth)
    /// applied between decode and `audio_encode`. Ignored when `audio_encode`
    /// is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video re-encode applied at ingress. Decodes the source video
    /// ES, re-encodes via the configured backend, and re-muxes the output TS.
    /// Feature-gated (`video-encoder-x264` / `-x265` / `-nvenc`) — validation
    /// fails early if the backend is not compiled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtInputConfig {
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address, e.g. "0.0.0.0:9000".
    /// Required for listener/rendezvous. Optional for caller (defaults to "0.0.0.0:0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
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
    /// Optional: enable native libsrt SRT bonding (socket groups) on input.
    /// Mutually exclusive with `redundancy`. Only supported with the libsrt
    /// backend; the pure-Rust backend does not expose socket groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bonding: Option<SrtBondingConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Send via RIST Simple Profile (TR-06-1:2020) — reliable RTP transport
    /// with RTCP NACK-based retransmission. Interoperable with librist
    /// `ristreceiver`. Binds an even RTP port P and RTCP port P+1 locally;
    /// transmits to `remote_addr` (peer's RTP port).
    #[serde(rename = "rist")]
    Rist(RistOutputConfig),
    /// Publish to RTMP/RTMPS server (e.g. Twitch, YouTube)
    #[serde(rename = "rtmp")]
    Rtmp(RtmpOutputConfig),
    /// Send TS segments via HLS ingest (e.g. YouTube HLS)
    #[serde(rename = "hls")]
    Hls(HlsOutputConfig),
    /// Send fragmented-MP4 (CMAF / CMAF-LL) segments with HLS + DASH manifests,
    /// optionally encrypted via ClearKey CENC. See [`CmafOutputConfig`].
    #[serde(rename = "cmaf")]
    Cmaf(CmafOutputConfig),
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
    /// Send SMPTE ST 2110-20 uncompressed video over RTP (RFC 4175).
    /// The output decodes the flow's source H.264/HEVC TS, scales/converts
    /// into planar 4:2:2 at the configured bit depth, then RFC 4175
    /// packetizes onto the wire. Requires the `video-thumbnail` feature
    /// for in-process decoding (default on).
    #[serde(rename = "st2110_20")]
    St2110_20(St2110VideoOutputConfig),
    /// Send SMPTE ST 2110-23 — one video essence split across N ST 2110-20
    /// sub-streams per the configured partition mode.
    #[serde(rename = "st2110_23")]
    St2110_23(St2110_23OutputConfig),
    /// Send PCM audio via RFC 3551 RTP/UDP without ST 2110 constraints
    /// (no PTP, no RFC 7273 timing, no NMOS clock_domain advertising).
    /// Supports the same `transcode` block as ST 2110-30 outputs and
    /// optionally MPEG-TS / SMPTE 302M wrapping via `transport_mode`.
    #[serde(rename = "rtp_audio")]
    RtpAudio(RtpAudioOutputConfig),
    /// Send the flow across N bonded paths (UDP / QUIC / RIST) with a
    /// media-aware scheduler that duplicates IDR frames across the two
    /// best paths. Pair with a `bonded` input at the far end.
    #[serde(rename = "bonded")]
    Bonded(BondedOutputConfig),
}

impl OutputConfig {
    /// Returns the unique identifier of this output, regardless of its concrete type.
    pub fn id(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.id,
            OutputConfig::Udp(c) => &c.id,
            OutputConfig::Srt(c) => &c.id,
            OutputConfig::Rist(c) => &c.id,
            OutputConfig::Rtmp(c) => &c.id,
            OutputConfig::Hls(c) => &c.id,
            OutputConfig::Cmaf(c) => &c.id,
            OutputConfig::Webrtc(c) => &c.id,
            OutputConfig::St2110_30(c) => &c.id,
            OutputConfig::St2110_31(c) => &c.id,
            OutputConfig::St2110_40(c) => &c.id,
            OutputConfig::St2110_20(c) => &c.id,
            OutputConfig::St2110_23(c) => &c.id,
            OutputConfig::RtpAudio(c) => &c.id,
            OutputConfig::Bonded(c) => &c.id,
        }
    }

    /// Returns the human-readable name of this output.
    pub fn name(&self) -> &str {
        match self {
            OutputConfig::Rtp(c) => &c.name,
            OutputConfig::Udp(c) => &c.name,
            OutputConfig::Srt(c) => &c.name,
            OutputConfig::Rist(c) => &c.name,
            OutputConfig::Rtmp(c) => &c.name,
            OutputConfig::Hls(c) => &c.name,
            OutputConfig::Cmaf(c) => &c.name,
            OutputConfig::Webrtc(c) => &c.name,
            OutputConfig::St2110_30(c) => &c.name,
            OutputConfig::St2110_31(c) => &c.name,
            OutputConfig::St2110_40(c) => &c.name,
            OutputConfig::St2110_20(c) => &c.name,
            OutputConfig::St2110_23(c) => &c.name,
            OutputConfig::RtpAudio(c) => &c.name,
            OutputConfig::Bonded(c) => &c.name,
        }
    }

    /// Returns the type name string (e.g. "srt", "rtp", "udp").
    pub fn type_name(&self) -> &'static str {
        match self {
            OutputConfig::Rtp(_) => "rtp",
            OutputConfig::Udp(_) => "udp",
            OutputConfig::Srt(_) => "srt",
            OutputConfig::Rist(_) => "rist",
            OutputConfig::Rtmp(_) => "rtmp",
            OutputConfig::Hls(_) => "hls",
            OutputConfig::Cmaf(_) => "cmaf",
            OutputConfig::Webrtc(_) => "webrtc",
            OutputConfig::St2110_30(_) => "st2110_30",
            OutputConfig::St2110_31(_) => "st2110_31",
            OutputConfig::St2110_40(_) => "st2110_40",
            OutputConfig::St2110_20(_) => "st2110_20",
            OutputConfig::St2110_23(_) => "st2110_23",
            OutputConfig::RtpAudio(_) => "rtp_audio",
            OutputConfig::Bonded(_) => "bonded",
        }
    }

    /// Returns `true` if this output is currently marked active. Passive
    /// outputs are persisted in config but not spawned by the engine.
    pub fn active(&self) -> bool {
        match self {
            OutputConfig::Rtp(c) => c.active,
            OutputConfig::Udp(c) => c.active,
            OutputConfig::Srt(c) => c.active,
            OutputConfig::Rist(c) => c.active,
            OutputConfig::Rtmp(c) => c.active,
            OutputConfig::Hls(c) => c.active,
            OutputConfig::Cmaf(c) => c.active,
            OutputConfig::Webrtc(c) => c.active,
            OutputConfig::St2110_30(c) => c.active,
            OutputConfig::St2110_31(c) => c.active,
            OutputConfig::St2110_40(c) => c.active,
            OutputConfig::St2110_20(c) => c.active,
            OutputConfig::St2110_23(c) => c.active,
            OutputConfig::RtpAudio(c) => c.active,
            OutputConfig::Bonded(c) => c.active,
        }
    }

    /// Sets the `active` flag on this output. Used by API handlers to toggle
    /// an output between active and passive without republishing the full
    /// output config.
    pub fn set_active(&mut self, active: bool) {
        match self {
            OutputConfig::Rtp(c) => c.active = active,
            OutputConfig::Udp(c) => c.active = active,
            OutputConfig::Srt(c) => c.active = active,
            OutputConfig::Rist(c) => c.active = active,
            OutputConfig::Rtmp(c) => c.active = active,
            OutputConfig::Hls(c) => c.active = active,
            OutputConfig::Cmaf(c) => c.active = active,
            OutputConfig::Webrtc(c) => c.active = active,
            OutputConfig::St2110_30(c) => c.active = active,
            OutputConfig::St2110_31(c) => c.active = active,
            OutputConfig::St2110_40(c) => c.active = active,
            OutputConfig::St2110_20(c) => c.active = active,
            OutputConfig::St2110_23(c) => c.active = active,
            OutputConfig::RtpAudio(c) => c.active = active,
            OutputConfig::Bonded(c) => c.active = active,
        }
    }

    /// Returns the optional free-form group tag for this output.
    pub fn group(&self) -> Option<&str> {
        match self {
            OutputConfig::Rtp(c) => c.group.as_deref(),
            OutputConfig::Udp(c) => c.group.as_deref(),
            OutputConfig::Srt(c) => c.group.as_deref(),
            OutputConfig::Rist(c) => c.group.as_deref(),
            OutputConfig::Rtmp(c) => c.group.as_deref(),
            OutputConfig::Hls(c) => c.group.as_deref(),
            OutputConfig::Cmaf(c) => c.group.as_deref(),
            OutputConfig::Webrtc(c) => c.group.as_deref(),
            OutputConfig::St2110_30(c) => c.group.as_deref(),
            OutputConfig::St2110_31(c) => c.group.as_deref(),
            OutputConfig::St2110_40(c) => c.group.as_deref(),
            OutputConfig::St2110_20(c) => c.group.as_deref(),
            OutputConfig::St2110_23(c) => c.group.as_deref(),
            OutputConfig::RtpAudio(c) => c.group.as_deref(),
            OutputConfig::Bonded(c) => c.group.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpOutputConfig {
    /// Unique output ID within this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine; they can be toggled on via the
    /// API without re-creating the output. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Optional PID remapping applied after `program_number` filtering and
    /// any audio/video re-encode. Keys are source PIDs, values are target
    /// PIDs (both 0x0010..=0x1FFE). PAT `program_map_PID` and PMT
    /// `PCR_PID` + elementary-PID fields are rewritten automatically;
    /// section CRCs are recomputed. See [`ts_pid_remapper`](crate::engine::ts_pid_remapper).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
    /// Optional output delay for stream synchronization.
    /// See [`OutputDelay`] for the available modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<OutputDelay>,
    /// Optional audio encode block. When set, the output runs its incoming
    /// MPEG-TS through a streaming audio-ES replacer: the source AAC-LC
    /// audio is decoded, re-encoded into the configured codec, and muxed
    /// back into the output TS (PMT is rewritten with the new stream_type).
    /// Video and other elementary streams pass through untouched. RTP
    /// carries TS, so every codec supported by the in-process encoder is
    /// valid here (`aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`;
    /// `opus` is rejected because there is no standard TS mapping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage (e.g. set only `channel_map_preset`
    /// to shuffle channels without resampling). Ignored when
    /// `audio_encode` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block. When set, the output decodes the
    /// incoming H.264 / HEVC video elementary stream, optionally
    /// re-scales, re-encodes via the configured backend, and muxes the
    /// resulting bitstream back into the output TS. Availability of each
    /// backend depends on the Cargo features enabled at build time
    /// (`video-encoder-x264`, `video-encoder-x265`, `video-encoder-nvenc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

/// Configurable output delay for stream synchronization.
///
/// Supports three modes:
/// - **Fixed**: adds a constant delay in milliseconds on top of the base latency.
/// - **TargetMs**: sets a target end-to-end latency in milliseconds. The system
///   holds each packet until `recv_time_us + target` has elapsed, so the total
///   output latency converges on the target regardless of base latency variations.
/// - **TargetFrames**: like `TargetMs` but the target is expressed in video frames.
///   Converted to milliseconds using the detected video frame rate. Falls back to
///   `fallback_ms` if no frame rate is detected within 5 seconds of output start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum OutputDelay {
    /// Fixed delay added on top of the existing processing latency.
    #[serde(rename = "fixed")]
    Fixed {
        /// Delay in milliseconds (0–10000).
        ms: u64,
    },
    /// Target end-to-end latency. Each packet is released exactly this many
    /// milliseconds after it was received at the input, dynamically absorbing
    /// base latency variations.
    #[serde(rename = "target_ms")]
    TargetMs {
        /// Target end-to-end latency in milliseconds (1–10000).
        ms: u64,
    },
    /// Target end-to-end latency expressed in video frames. Requires a
    /// detected video frame rate (H.264/H.265 SPS).
    #[serde(rename = "target_frames")]
    TargetFrames {
        /// Number of frames of end-to-end latency (0.01–300.0).
        frames: f64,
        /// Fallback delay in milliseconds if no frame rate is detected
        /// within 5 seconds of output start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback_ms: Option<u64>,
    },
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
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Optional PID remapping. See [`RtpOutputConfig::pid_map`] for semantics.
    /// Incompatible with `transport_mode = "audio_302m"` (the 302M path
    /// synthesizes its own TS and owns the PID assignments).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
    /// Transport mode for this UDP output. `"ts"` (default) sends raw
    /// MPEG-TS as the existing path does. `"audio_302m"` runs the per-output
    /// transcode + 302M-in-MPEG-TS pipeline and sends the resulting
    /// 7×188-byte TS chunks as plain UDP datagrams — useful for legacy
    /// hardware decoders that expect raw MPEG-TS over UDP carrying SMPTE
    /// 302M LPCM audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
    /// Optional output delay for stream synchronization.
    /// Incompatible with `transport_mode: "audio_302m"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<OutputDelay>,
    /// Optional audio encode block. See [`RtpOutputConfig::audio_encode`]
    /// for the semantics. Incompatible with `transport_mode = "audio_302m"`
    /// (the 302M path already owns the TS stream and carries no video).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage. Ignored when `audio_encode` is
    /// not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block. See [`RtpOutputConfig::video_encode`]
    /// for the semantics. Incompatible with `transport_mode = "audio_302m"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtOutputConfig {
    /// Unique output ID within this flow
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address.
    /// Required for listener/rendezvous. Optional for caller (defaults to "0.0.0.0:0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
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
    /// Optional: enable native libsrt SRT bonding (socket groups) on output.
    /// Mutually exclusive with `redundancy`. Only supported with the libsrt
    /// backend; the pure-Rust backend does not expose socket groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bonding: Option<SrtBondingConfig>,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// MPEG-TS program before sending. Default: passthrough (full MPTS).
    /// Must be > 0; program_number 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional PID remapping. See [`RtpOutputConfig::pid_map`] for semantics.
    /// Incompatible with `transport_mode = "audio_302m"`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
    /// Transport mode for this SRT output. `"ts"` (default) sends whatever
    /// the upstream broadcast channel produces (RTP-wrapped or raw TS).
    /// `"audio_302m"` runs the per-output transcode + 302M packetizer
    /// + TS muxer pipeline and ships 7×188-byte SMPTE 302M-in-MPEG-TS
    /// datagrams over SRT. Interoperable with `ffmpeg -c:a s302m`,
    /// `srt-live-transmit`, and broadcast hardware decoders that expect
    /// 302M LPCM in MPEG-TS over SRT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
    /// Optional output delay for stream synchronization.
    /// Incompatible with `transport_mode: "audio_302m"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<OutputDelay>,
    /// Optional audio encode block. See [`RtpOutputConfig::audio_encode`]
    /// for the semantics. Incompatible with `transport_mode = "audio_302m"`
    /// (the 302M path already owns the TS stream and carries no video).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage. Ignored when `audio_encode` is
    /// not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block. See [`RtpOutputConfig::video_encode`]
    /// for the semantics. Incompatible with `transport_mode = "audio_302m"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

/// SRT connection mode, determining which side initiates the handshake.
///
/// The mode affects which address fields are required in the configuration:
/// - `Caller` and `Rendezvous` require a `remote_addr`.
/// - `Listener` only needs a `local_addr` to bind on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Local bind address for leg 2.
    /// Required for listener/rendezvous. Optional for caller (defaults to "0.0.0.0:0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
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

/// Native libsrt SRT bonding mode (socket-group type).
///
/// Wire-compatible with libsrt's `SRT_GTYPE_*` socket groups — interoperable
/// with `srt-live-transmit grp:BROADCAST://` / `grp:BACKUP://` and any other
/// libsrt-based peer that speaks the bonding handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrtBondingMode {
    /// All member links active simultaneously. libsrt deduplicates on the
    /// receiver. Equivalent to SMPTE 2022-7 hitless redundancy at the SRT
    /// layer but negotiated in the handshake — a single bonded session from
    /// the peer's perspective.
    #[serde(rename = "broadcast")]
    Broadcast,
    /// Primary/backup: only the highest-priority healthy link carries data;
    /// others stand by for automatic failover.
    #[serde(rename = "backup")]
    Backup,
}

/// One endpoint (member link) in an SRT bonding group.
///
/// Each bonded member is a separate SRT path — typically pinned to a
/// different NIC / upstream network. The SRT config options on the parent
/// `SrtInputConfig` / `SrtOutputConfig` (latency, encryption, stream_id,
/// payload_size, etc.) apply to every member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtBondingEndpoint {
    /// Remote address (caller side) or local bind address (listener side).
    /// For callers this is the peer listener; for listeners this is the
    /// local bind address for this member. In caller mode every endpoint
    /// must supply a remote address.
    pub addr: String,
    /// Optional local bind address for caller members (e.g. to pin the
    /// member to a specific NIC / source IP). Ignored in listener mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Backup-mode member priority. Lower values are preferred. Ignored in
    /// broadcast mode. Default: 0 (all members equal — libsrt picks
    /// deterministically).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
}

/// Native libsrt SRT bonding (socket group) config.
///
/// Mutually exclusive with [`SrtRedundancyConfig`] — bonding replaces the
/// app-layer 2022-7 hitless merge with libsrt's native group handshake.
/// Only supported with the libsrt backend; rejected at config validation
/// time when the pure-Rust backend is active.
///
/// Broadcast mode is wire-compatible with SMPTE 2022-7-style redundancy at
/// the SRT level. Backup mode is primary/backup failover. At least 2 and
/// at most 8 endpoints are supported (libsrt's practical cap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtBondingConfig {
    /// Bonding mode: broadcast (all-active, dedup) or backup (primary/
    /// failover).
    pub mode: SrtBondingMode,
    /// Member endpoints (2..=8). In caller mode each entry is a remote
    /// listener peer; in listener mode each entry is a local bind address
    /// that accepts one member of the group handshake.
    pub endpoints: Vec<SrtBondingEndpoint>,
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

/// RIST Simple Profile (TR-06-1:2020) input. Binds a dual-port UDP channel
/// (RTP on even port P, RTCP on P+1) and decodes the reliable-RTP stream
/// delivered by a remote `ristsender` or equivalent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistInputConfig {
    /// Local bind address, e.g. "0.0.0.0:6000". The port **must be even** —
    /// RIST binds RTCP on port+1.
    pub bind_addr: String,
    /// Receiver jitter / retransmit buffer depth in milliseconds.
    /// Default 1000 ms, range 50–30000 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<u32>,
    /// Maximum NACK retransmission attempts per lost packet. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nack_retries: Option<u32>,
    /// CNAME emitted in RTCP SDES packets (optional, auto-generated if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// RTCP emission interval in milliseconds (TR-06-1 requires ≤ 100 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtcp_interval_ms: Option<u32>,
    /// Optional SMPTE 2022-7 redundancy (merge two RIST legs).
    /// The primary `bind_addr` is leg 1; this defines leg 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RistInputRedundancyConfig>,
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

/// RIST Simple Profile (TR-06-1:2020) output. Binds a local dual-port UDP
/// channel and transmits reliable RTP to the peer's even RTP port; the
/// peer's RTCP traffic is learned dynamically (no P+1 assumption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Remote RIST address, e.g. "203.0.113.10:6000". Port must be even.
    pub remote_addr: String,
    /// Local bind address for the sender's RTP socket. Optional — defaults
    /// to "0.0.0.0:0" (ephemeral even port). When set, the port must be even.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Sender retransmit buffer depth in milliseconds. Default 1000 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<u32>,
    /// Retransmit buffer capacity in packets (sender side). Default 2048.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_buffer_capacity: Option<usize>,
    /// CNAME emitted in RTCP SDES packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// RTCP emission interval in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtcp_interval_ms: Option<u32>,
    /// Optional SMPTE 2022-7 redundancy (duplicate output to a second leg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RistOutputRedundancyConfig>,
    /// If set, filter the (possibly MPTS) input stream down to this single
    /// program. Must be > 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional PID remapping. See [`RtpOutputConfig::pid_map`] for semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
    /// Optional output delay for stream synchronization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<OutputDelay>,
    /// Optional audio encode block (AAC-LC / HE-AAC / MP2 / AC-3). See
    /// [`RtpOutputConfig::audio_encode`] for semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage. Ignored when `audio_encode` is
    /// not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block (x264 / x265 / NVENC, feature-gated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

/// SMPTE 2022-7 redundancy config for a RIST input (leg 2).
/// The primary `bind_addr` in the parent RistInputConfig is leg 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistInputRedundancyConfig {
    /// Bind address for leg 2. Port must be even.
    pub bind_addr: String,
}

/// SMPTE 2022-7 redundancy config for a RIST output (leg 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistOutputRedundancyConfig {
    /// Remote address for leg 2. Port must be even.
    pub remote_addr: String,
    /// Local bind address for leg 2 (optional). When set, port must be even.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
}

// ── Bonded input / output ────────────────────────────────────────────────────
//
// Media-aware multi-path bonding (see bilbycast-bonding). Each `BondedInputConfig`
// / `BondedOutputConfig` maps 1:1 to a `BondSocket::{sender,receiver}` built from
// a list of paths (UDP / QUIC / RIST). Engine's `input_bonded.rs` and
// `output_bonded.rs` are the thin adapters that publish bonded payloads onto the
// flow's broadcast channel as `RtpPacket { is_raw_ts: true, .. }`, and subscribe
// to the channel to emit bond frames respectively.

/// Bonded input — reassembles a bond flow arriving on N paths and publishes the
/// delivered payloads into the flow's broadcast channel. Use this as the
/// `receiver` side of a `bilbycast-bonder` (or edge-to-edge bond) link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BondedInputConfig {
    /// Bond-layer flow identifier — must match the sender end.
    pub bond_flow_id: u32,
    /// Paths to bind on.
    pub paths: Vec<BondPathConfig>,
    /// Reassembly hold time in milliseconds. Default 500 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
    /// Base NACK delay in milliseconds. Default 30 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nack_delay_ms: Option<u32>,
    /// Max NACK retries per gap. Default 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nack_retries: Option<u32>,
    /// Keepalive interval in milliseconds. Default 200 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_ms: Option<u32>,
}

/// Bonded output — subscribes to the flow's broadcast channel, frames each
/// packet into the bond header, and transmits across N paths using the
/// configured scheduler. IDR-duplication and NAL-aware priority are enabled
/// by default via [`BondSchedulerKind::MediaAware`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BondedOutputConfig {
    /// Unique output ID within the flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Bond-layer flow identifier — must match the receiver end.
    pub bond_flow_id: u32,
    /// Paths to transmit across.
    pub paths: Vec<BondPathConfig>,
    /// Scheduling policy.
    #[serde(default)]
    pub scheduler: BondSchedulerKind,
    /// Sender retransmit buffer capacity in packets. Default 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_capacity: Option<usize>,
    /// Keepalive interval in milliseconds. Default 200 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_ms: Option<u32>,
    /// Optional MPTS `program_number` filter applied before bonding (mirrors
    /// other TS-native outputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional PID remapping applied after `program_number` filtering and
    /// before bonding. See [`RtpOutputConfig::pid_map`] for semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
}

/// A single bond leg definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BondPathConfig {
    /// Path identifier — must be unique within the paths array.
    pub id: u8,
    /// Operator-visible name (e.g. `"lte-0"`, `"starlink"`).
    pub name: String,
    /// Scheduler weight hint; higher = more traffic at steady state.
    #[serde(default = "default_bond_weight")]
    pub weight_hint: u32,
    /// Transport flavour.
    pub transport: BondPathTransportConfig,
}

fn default_bond_weight() -> u32 {
    1
}

/// Transport enum for a single bond leg.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BondPathTransportConfig {
    /// Raw UDP — bidirectional, simplest path.
    Udp {
        /// Local bind `ip:port`. Optional on the sender side
        /// (ephemeral), required on the receiver side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<String>,
        /// Remote peer `ip:port`. Required on the sender side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
        /// Optional NIC pin (e.g. `"eth0"`, `"wwan0"`). Forces
        /// egress onto a specific interface regardless of the
        /// routing table. Without this, multiple paths with the
        /// same destination will typically all use the default
        /// route. See `bilbycast-bonding/docs/nic-pinning.md`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<String>,
    },
    /// RIST Simple Profile leg. Unidirectional at the bond layer; set
    /// `role` to match the bonded-input-or-output side of this path.
    Rist {
        role: BondRistRole,
        /// Remote RIST receiver (sender role). Ignored in receiver role.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
        /// Local bind. Required on receiver role; optional on sender.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_bind: Option<String>,
        /// RIST buffer in milliseconds (default 1000 ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        buffer_ms: Option<u32>,
    },
    /// QUIC path (TLS 1.3 + DATAGRAM extension). Full-duplex.
    Quic {
        role: BondQuicRole,
        /// Client: remote `host:port`. Server: local bind.
        addr: String,
        /// Client: server name for SNI/ALPN. Ignored on server role.
        #[serde(default)]
        server_name: String,
        tls: BondQuicTls,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondRistRole {
    Sender,
    Receiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondQuicRole {
    Client,
    Server,
}

/// QUIC TLS material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BondQuicTls {
    /// Self-signed in-process (dev / loopback / trusted LAN).
    SelfSigned,
    /// PEM cert chain + private key, loaded from on-disk paths.
    Pem {
        cert_chain_path: String,
        private_key_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_trust_root_path: Option<String>,
    },
}

/// Scheduling policy for a bonded output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BondSchedulerKind {
    /// Equal-weight rotation — simplest policy, fine for links with
    /// nearly identical health.
    RoundRobin,
    /// RTT-weighted rotation — sends more traffic to lower-RTT paths
    /// and duplicates `Critical`-priority packets across the two
    /// lowest-RTT paths.
    WeightedRtt,
    /// RTT-weighted plus NAL-type detection: walks H.264 / HEVC NAL
    /// boundaries in the outbound TS stream, tags SPS / PPS / IDR
    /// (H.264 types 7, 8, 5; HEVC VPS/SPS/PPS/IDR_W_RADL/IDR_N_LP)
    /// as `Priority::Critical` so the scheduler duplicates them
    /// across the two best paths. Falls back to single-path for
    /// non-IDR frames. **Default for broadcast flows.**
    #[default]
    MediaAware,
}

/// RTMP/RTMPS output configuration for publishing to streaming platforms.
///
/// Demuxes H.264/HEVC and AAC from the MPEG-2 TS stream, muxes into FLV, and
/// publishes via the RTMP protocol. Supports RTMPS (RTMP over TLS) via
/// `rustls`. H.264 uses classic FLV `CodecID=7`; HEVC rides over
/// [Enhanced RTMP v2](https://veovera.org/docs/enhanced/enhanced-rtmp-v2)
/// with FourCC `hvc1` — receivers must understand E-RTMP for HEVC output.
///
/// # Limitations
/// - Output only (publish). RTMP input is not supported.
/// - HEVC passthrough and HEVC re-encode emit E-RTMP tags; legacy FLV
///   receivers expecting only classic AVC will not decode HEVC outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtmpOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage. Ignored when `audio_encode` is
    /// not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block (x264 / x265 / NVENC, feature-gated).
    /// H.264 targets are packed as classic FLV VideoData; HEVC targets
    /// emit Enhanced RTMP v2 tags (FourCC `hvc1`). When unset, video is
    /// passed through — only H.264 passthrough is supported today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
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
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Optional PID remapping applied to the TS bytes before segmentation.
    /// See [`RtpOutputConfig::pid_map`] for semantics. Applied only on the
    /// passthrough/codec-preserving path; the audio-reencode fast path
    /// still honours the map because PIDs are rewritten after re-encode.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<BTreeMap<u16, u16>>,
    /// Optional audio encode block. When set, this output stops being a
    /// pure TS-passthrough segmenter: it demuxes video + audio, re-encodes
    /// audio via the ffmpeg sidecar encoder, and re-muxes a fresh TS for
    /// the segment buffer. HLS-TS supports `aac_lc`, `he_aac_v1`,
    /// `he_aac_v2`, `mp2`, and `ac3`. Requires ffmpeg in PATH at runtime.
    /// The same-codec fast path (codec=aac_lc with no overrides on an
    /// AAC-LC source) skips the re-mux and falls back to passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** `audio_encode` re-encodes. Any unset
    /// field passes through that stage. Ignored when `audio_encode` is
    /// not set; setting `transcode` disables the same-codec passthrough
    /// fast-path because the PCM must be decoded to apply the shuffle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
}

fn default_segment_duration() -> f64 {
    2.0
}

fn default_max_segments() -> usize {
    5
}

/// CMAF / CMAF-LL output.
///
/// Publishes fragmented-MP4 (CMAF per ISO/IEC 23000-19) segments alongside
/// one or more manifests (HLS m3u8 and/or DASH .mpd) via HTTP push ingest.
/// Each artifact is uploaded to `{ingest_url}/{filename}` — for example
/// `https://ingest.example.com/live/init.mp4`, `.../seg-00042.m4s`,
/// `.../manifest.m3u8`, `.../manifest.mpd`.
///
/// When `low_latency = true`, each media segment is sent as a single HTTP
/// request with chunked transfer encoding, streaming CMAF chunks as they
/// are produced (`chunk_duration_ms` apart). Standard mode (`low_latency = false`)
/// buffers each segment to completion and uploads it with a single PUT.
///
/// Video is passed through unchanged by default (source must be H.264 or
/// HEVC with IDR at least every `segment_duration_secs`). Setting
/// `video_encode` forces a decode + re-encode with explicit GoP alignment —
/// useful when the source cadence does not match the CMAF segment boundary.
///
/// Audio may be passed through (AAC carried in the source MPEG-TS) or
/// re-encoded via the shared `audio_encode` + `transcode` stages. CMAF
/// audio targets: `aac_lc`, `he_aac_v1`, `he_aac_v2`.
///
/// # Encryption
///
/// When `encryption` is set, segments are encrypted with Common Encryption
/// (ISO/IEC 23001-7) using the operator-supplied `key_id` + `key`. The MVP
/// ships ClearKey — the edge inserts a ClearKey `pssh` so W3C EME ClearKey
/// clients can decrypt. Operators layering commercial DRM (Widevine /
/// PlayReady / FairPlay) may provide pre-built `pssh_boxes` from their DRM
/// vendor; the edge wraps them verbatim into the init segment's `moov`.
///
/// # Limitations
///
/// - Output only. Standard-mode segment-based transport adds 1-4 s latency;
///   LL-CMAF with 500 ms chunks targets <3 s glass-to-glass.
/// - Source must have an IDR at least every `segment_duration_secs` unless
///   `video_encode` is set.
/// - Phase 1 MVP: H.264 video + AAC audio; HEVC and encryption come in later
///   phases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmafOutputConfig {
    /// Unique output ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Base URL for segment / init / manifest uploads. Must begin with
    /// `http://` or `https://`. Individual artifacts are PUT to
    /// `{ingest_url}/{filename}` — keep a trailing path component (e.g.
    /// `/live`) if the ingest expects one.
    pub ingest_url: String,
    /// Enable Low-Latency CMAF (chunked-transfer streaming PUT per segment
    /// plus `#EXT-X-PART` / `availabilityTimeOffset` in manifests).
    /// Default: `false` (standard whole-segment CMAF).
    #[serde(default)]
    pub low_latency: bool,
    /// LL-CMAF chunk duration in milliseconds. Ignored when
    /// `low_latency = false`. Range: 100-2000. Default: 500.
    #[serde(default = "default_cmaf_chunk_duration_ms")]
    pub chunk_duration_ms: u32,
    /// Target segment (GoP) duration in seconds. Range: 1.0-10.0.
    /// Default: 2.0. Segments always cut on IDR — this is advisory.
    #[serde(default = "default_segment_duration")]
    pub segment_duration_secs: f64,
    /// Maximum number of segments in the rolling playlist. Range: 1-30.
    /// Default: 5.
    #[serde(default = "default_max_segments")]
    pub max_segments: usize,
    /// Manifests to publish. Non-empty subset of `["hls", "dash"]`.
    /// Default: both. **Phase 1 MVP ships only `"hls"` — validation
    /// rejects `"dash"` until Phase 2 lands.**
    #[serde(default = "default_cmaf_manifests")]
    pub manifests: Vec<String>,
    /// Optional ClearKey CENC encryption. **Phase 1 MVP validation
    /// rejects this field until Phase 5 lands.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<CencConfig>,
    /// Optional audio encode block. CMAF audio codecs: `aac_lc`,
    /// `he_aac_v1`, `he_aac_v2`. **Phase 1 MVP validation rejects this
    /// until Phase 3 lands — Phase 1 requires AAC passthrough.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional PCM transcode (channel shuffle / SRC / bit-depth), sits
    /// between the AAC decoder and `audio_encode`. Requires `audio_encode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block. When set, the output decodes the
    /// source H.264/HEVC and re-encodes with explicit GoP alignment to the
    /// CMAF segment duration. **Phase 1 MVP validation rejects this until
    /// Phase 3 lands.** CMAF video codec must be H.264 (Phase 1) or HEVC
    /// (Phase 2+); MPEG-DASH additionally supports hvc1/hev1 signalling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
    /// MPTS program filter. If the source is an MPTS, filter down to this
    /// single program before segmenting. Must be `> 0` (program 0 is the
    /// NIT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional Bearer token sent as `Authorization: Bearer {token}` on
    /// every upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

/// Common Encryption (ISO/IEC 23001-7) configuration for a CMAF output.
///
/// The MVP ships ClearKey — the operator supplies `key_id` + `key` and the
/// edge emits a ClearKey `pssh` so W3C EME ClearKey clients can decrypt.
/// Commercial DRM systems (Widevine, PlayReady, FairPlay) can be layered
/// by supplying pre-built `pssh_boxes` — the edge copies each entry
/// verbatim into the init segment's `moov`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CencConfig {
    /// Key ID, exactly 32 lowercase or uppercase hex characters (16 bytes).
    /// Embedded in the `tenc.default_KID` field and the ClearKey `pssh`.
    pub key_id: String,
    /// AES-128 content key, exactly 32 hex characters (16 bytes).
    /// **This is a secret. It is written to the node's config — clients
    /// receive the key only via the ClearKey license flow or operator
    /// DRM integration.**
    pub key: String,
    /// Encryption scheme: `"cenc"` (AES-128 CTR, the default) or `"cbcs"`
    /// (AES-128 CBC with 1:9 block pattern, required for FairPlay).
    pub scheme: String,
    /// Optional pre-built `pssh` box payloads (hex-encoded), one per
    /// additional DRM system. Each entry must be the full `pssh` box
    /// starting from its size field. The edge validates the fourcc is
    /// `pssh` and copies bytes verbatim into `moov`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pssh_boxes: Vec<String>,
}

fn default_cmaf_chunk_duration_ms() -> u32 {
    500
}

fn default_cmaf_manifests() -> Vec<String> {
    vec!["hls".to_string(), "dash".to_string()]
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
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Optional audio channel shuffle / sample-rate transcode applied to
    /// the decoded PCM **before** the Opus encoder. When
    /// `transcode.channels` is set, it overrides the Opus encoder's
    /// channel count (so you can force a stereo source down to mono).
    /// When unset, Opus follows the source channel count. Any other
    /// unset field passes through that stage. Ignored when
    /// `audio_encode` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video encode block. When set, the output decodes the
    /// source H.264/HEVC video, re-encodes via the selected backend
    /// (`x264` or `h264_nvenc` — browsers do not decode HEVC), and
    /// emits fresh RTP H.264 packets per RFC 6184 with SPS/PPS
    /// inline on every IDR. Enables HEVC-source → WebRTC viewers and
    /// bitrate/profile transcoding for H.264 sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,
}

/// Video encoder configuration block. Used by SRT, UDP, and RTP outputs
/// (as `video_encode`) to enable end-to-end video transcoding inside the
/// MPEG-TS stream: the output decodes the source H.264/HEVC elementary
/// stream, rescales (when `width` / `height` change), re-encodes via
/// `video-engine::VideoEncoder`, and re-muxes the resulting bitstream
/// back into the output TS with a fresh PMT (if the target codec
/// differs from the source).
///
/// The valid backend set depends on which Cargo features were enabled at
/// build time:
/// - `video-encoder-x264` → `x264` (GPL v2+).
/// - `video-encoder-x265` → `x265` (GPL v2+).
/// - `video-encoder-nvenc` → `h264_nvenc` / `hevc_nvenc`.
///
/// Validation in `config::validation` enforces field bounds at config
/// load time. Unavailable backends are surfaced as a runtime error when
/// the output starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoEncodeConfig {
    /// Encoder backend. One of: `x264`, `x265`, `h264_nvenc`, `hevc_nvenc`.
    pub codec: String,
    /// Output width in pixels. When unset, the encoder uses the source
    /// resolution. Range: 64–7680.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Output height in pixels. When unset, uses the source resolution.
    /// Range: 64–4320.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Output frame-rate numerator (e.g. 30000 for 29.97 fps). Defaults
    /// to the source frame rate detected from the input stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps_num: Option<u32>,
    /// Output frame-rate denominator (e.g. 1001 for 29.97 fps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps_den: Option<u32>,
    /// Target average bitrate in kbps. Range: 100–100_000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Keyframe interval (GOP size) in frames. Defaults to 2× the frame
    /// rate (two-second GOPs). Range: 1–600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gop_size: Option<u32>,
    /// Speed/quality preset. One of: `ultrafast`, `superfast`, `veryfast`,
    /// `faster`, `fast`, `medium`, `slow`, `slower`, `veryslow`. Default:
    /// `medium`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// H.264/HEVC profile target. One of: `baseline`, `main`, `high`,
    /// `high10`, `high422`, `high444`, `main10`. When unset, the encoder
    /// chooses. Higher profiles (`high10` / `high422` / `high444`,
    /// `main10`) must be combined with a matching `chroma` /
    /// `bit_depth`; validation enforces this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Chroma subsampling for the encoded bitstream. One of: `yuv420p`
    /// (default — preserves legacy behaviour), `yuv422p`, `yuv444p`.
    /// NVENC backends only support `yuv420p` / `yuv422p`; validation
    /// rejects `yuv444p` with NVENC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroma: Option<String>,
    /// Sample bit depth. One of: `8` (default), `10`. NVENC H.264 is
    /// 8-bit only; validation rejects `10` on `h264_nvenc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    /// Rate-control mode. One of: `vbr` (default — legacy behaviour),
    /// `cbr`, `crf`, `abr`. In `crf` mode `bitrate_kbps` is ignored and
    /// `crf` drives quantisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_control: Option<String>,
    /// Constant rate factor (quality target) for `rate_control == "crf"`.
    /// Range: 0..=51 (lower = better quality; broadcast typical 18..=28).
    /// For NVENC this is translated to `cq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crf: Option<u8>,
    /// Max bitrate cap in kbps for VBR (`rc_max_rate`) or CBR ceiling.
    /// Range: 100..=100_000. Ignored in `crf` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
    /// Consecutive B-frames. `0` (default) preserves legacy behaviour
    /// (no B-frames). Range: 0..=16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bframes: Option<u8>,
    /// Reference frames count. When unset, the encoder uses its default.
    /// Range: 1..=16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<u8>,
    /// Codec level — e.g. `"3.0"`, `"4.0"`, `"5.1"`. When unset, the
    /// encoder selects based on resolution / bitrate / framerate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Encoder `tune` hint — one of: `zerolatency` (default), `film`,
    /// `animation`, `grain`, `stillimage`, `fastdecode`, `psnr`, `ssim`.
    /// Empty string means "unset" (encoder chooses). Only x264 / x265
    /// accept all values; NVENC tolerates `zerolatency` and otherwise
    /// ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune: Option<String>,
    /// Colour primaries — one of: `bt709`, `bt2020`, `smpte170m`,
    /// `smpte240m`, `bt470m`, `bt470bg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    /// Transfer characteristics — one of: `bt709`, `smpte170m`,
    /// `smpte2084` (PQ), `arib-std-b67` (HLG), `bt2020-10`, `bt2020-12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    /// Matrix coefficients — one of: `bt709`, `bt2020nc`, `bt2020c`,
    /// `smpte170m`, `smpte240m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_matrix: Option<String>,
    /// Colour range — `tv` (limited, default) or `pc` (full).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
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
    /// If true, inject a continuous zero-filled (silent) PCM track into
    /// the encoder whenever the upstream source has no audio PID, or
    /// stops delivering audio mid-stream (watchdog, see
    /// `engine::audio_silence`). Default false preserves legacy behaviour
    /// (no audio → no encoder, no audio tags emitted).
    ///
    /// Silent fallback guarantees the container always carries a valid,
    /// continuous audio track. This is required by ingest services like
    /// Twitch / YouTube that gate their live-preview thumbnailer on the
    /// presence of audio, and by low-latency distribution formats
    /// (WebRTC / CMAF-LL) whose segmenters expect monotonic audio
    /// timestamps per segment.
    #[serde(default, skip_serializing_if = "is_false")]
    pub silent_fallback: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Optional planar PCM transcode applied between PCM depacketization and
    /// re-packetization. Unset fields pass through (so an empty block is a
    /// no-op). Broadcast channel shape stays PCM-RTP — outputs on the same
    /// flow keep receiving raw PCM. **Rejected on ST 2110-31 (AES3)** —
    /// validation refuses the block because AES3 sub-frames carry SMPTE 337M
    /// payload (possibly Dolby E) that a linear-PCM transcoder would corrupt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress audio re-encode. When set, the PCM input is
    /// depacketized, optionally transcoded, encoded into the configured codec
    /// (`aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`), and muxed into an
    /// audio-only MPEG-TS stream. **This changes the broadcast channel shape**
    /// from PCM-RTP to MPEG-TS: ST 2110-30/-31 and 302M-mode outputs can no
    /// longer attach to the same flow. Validation enforces the compatibility
    /// rules. **Rejected on ST 2110-31 (AES3)** — see `transcode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
}

/// SMPTE ST 2110-30 / -31 audio output configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110AudioOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Audio track selector for de-embedding from MPEG-TS inputs.
    ///
    /// When the upstream input carries MPEG-TS (SRT, RTP, UDP, RTMP, RTSP),
    /// the TS may contain multiple audio elementary streams (e.g., different
    /// languages). This field selects which audio track to extract:
    ///
    /// - `None` (default): use the first audio track found in the PMT.
    /// - `Some(0)`: first audio track, `Some(1)`: second audio track, etc.
    ///
    /// Ignored when the upstream input is a native audio essence (ST 2110-30,
    /// ST 2110-31, rtp_audio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_track_index: Option<u8>,
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
    /// Optional planar PCM transcode. See [`St2110AudioInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress audio re-encode. See [`St2110AudioInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
}

/// Generic RFC 3551 PCM-over-RTP audio output. See [`RtpAudioInputConfig`]
/// for the rationale. Supports the same `transcode` block as ST 2110-30
/// outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtpAudioOutputConfig {
    pub id: String,
    pub name: String,
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    /// Audio track selector for de-embedding from MPEG-TS inputs.
    /// See [`St2110AudioOutputConfig::audio_track_index`] for details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_track_index: Option<u8>,
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
    /// Whether this output is currently active. Passive outputs are kept in
    /// config but not spawned by the engine. Defaults to `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and the
    /// Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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

// ── ST 2110-20 / -23 video configuration ────────────────────────────────

/// RFC 4175 pixel format for ST 2110-20 / -23 on the wire.
///
/// Phase 2 supports 4:2:2 YCbCr at 8-bit or 10-bit. Other formats are
/// rejected by validation and will be added in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum St2110VideoPixelFormat {
    #[serde(rename = "yuv422_8bit")]
    Yuv422_8bit,
    #[serde(rename = "yuv422_10bit")]
    Yuv422_10bit,
}

impl St2110VideoPixelFormat {

    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yuv422_8bit => "yuv422_8bit",
            Self::Yuv422_10bit => "yuv422_10bit",
        }
    }
    pub fn depth(self) -> u8 {
        match self {
            Self::Yuv422_8bit => 8,
            Self::Yuv422_10bit => 10,
        }
    }
}

/// Partition mode for ST 2110-23 multi-stream video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum St2110_23PartitionModeConfig {
    #[serde(rename = "two_sample_interleave")]
    TwoSampleInterleave,
    #[serde(rename = "sample_row")]
    SampleRow,
}

/// ST 2110-20 (uncompressed video) input configuration. Inputs MUST include
/// a `video_encode` block — the encoded MPEG-TS is what enters the flow's
/// broadcast channel. An encoder backend feature (`video-encoder-x264`,
/// `-x265`, or `-nvenc`) must be compiled in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110VideoInputConfig {
    pub bind_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    pub width: u32,
    pub height: u32,
    /// Frame-rate numerator (e.g. 30000 for 29.97 fps, 60 for 60 fps).
    pub frame_rate_num: u32,
    /// Frame-rate denominator (e.g. 1001 for 29.97 fps, 1 for 60 fps).
    pub frame_rate_den: u32,
    /// Wire pgroup format. Only `yuv422_8bit` and `yuv422_10bit` are accepted
    /// by validation in Phase 2.
    pub pixel_format: St2110VideoPixelFormat,
    /// Dynamic RTP payload type (96..=127). Default: 96.
    #[serde(default = "default_st2110_video_pt")]
    pub payload_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_mbps: Option<f64>,
    /// Mandatory encoder block. The ingress pipeline depacketizes RFC 4175,
    /// decodes pixel data, then feeds the raw YUV into this encoder. Output
    /// H.264/HEVC bitstream is muxed into MPEG-TS and published on the
    /// broadcast channel.
    pub video_encode: VideoEncodeConfig,
}

/// ST 2110-20 (uncompressed video) output configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110VideoOutputConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub dest_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub pixel_format: St2110VideoPixelFormat,
    #[serde(default = "default_st2110_video_pt")]
    pub payload_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<u32>,
    /// RTP payload budget per datagram (bytes of payload excluding the RTP
    /// header and RFC 4175 triplet headers). Typical 1428 for 1500-byte MTU.
    #[serde(default = "default_st2110_video_payload_budget")]
    pub payload_budget: usize,
}

/// Per-sub-stream bind (ST 2110-23 input). Each sub-stream is a valid -20
/// receiver for one partition of the full video essence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110_23SubStreamBind {
    pub bind_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    #[serde(default = "default_st2110_video_pt")]
    pub payload_type: u8,
}

/// Per-sub-stream destination (ST 2110-23 output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110_23SubStreamDest {
    pub dest_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<RedBlueBindConfig>,
    #[serde(default = "default_st2110_video_pt")]
    pub payload_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<u32>,
}

/// ST 2110-23 input: multiple -20 sub-streams reassembled into one essence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110_23InputConfig {
    pub sub_streams: Vec<St2110_23SubStreamBind>,
    pub partition_mode: St2110_23PartitionModeConfig,
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub pixel_format: St2110VideoPixelFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    pub video_encode: VideoEncodeConfig,
}

/// ST 2110-23 output: one essence split into N sub-stream -20 senders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110_23OutputConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub sub_streams: Vec<St2110_23SubStreamDest>,
    pub partition_mode: St2110_23PartitionModeConfig,
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub pixel_format: St2110VideoPixelFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    #[serde(default = "default_dscp")]
    pub dscp: u8,
    #[serde(default = "default_st2110_video_payload_budget")]
    pub payload_budget: usize,
}

fn default_st2110_video_pt() -> u8 {
    96
}

fn default_st2110_video_payload_budget() -> usize {
    1428
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

    /// The flow-level `assembly` block must round-trip through JSON
    /// without dropping any slot kind — including nested `hitless` pairs
    /// around an explicit-PID primary and essence-kind backup. Exercises
    /// the internally-tagged `SlotSource` enum which is sensitive to
    /// `serde_json`'s `Content` intermediate.
    #[test]
    fn flow_assembly_roundtrips_full_shape() {
        let json = r#"{
            "id": "f",
            "name": "F",
            "input_ids": ["in-a", "in-b"],
            "output_ids": ["out-1"],
            "assembly": {
                "kind": "mpts",
                "pcr_source": { "input_id": "in-a", "pid": 256 },
                "programs": [
                    {
                        "program_number": 1,
                        "service_name": "Channel 1",
                        "pmt_pid": 4096,
                        "streams": [
                            {
                                "source": { "type": "pid", "input_id": "in-a", "source_pid": 256 },
                                "out_pid": 257,
                                "stream_type": 27,
                                "label": "Main video"
                            },
                            {
                                "source": {
                                    "type": "hitless",
                                    "primary": { "type": "pid", "input_id": "in-a", "source_pid": 258 },
                                    "backup":  { "type": "essence", "input_id": "in-b", "kind": "audio" }
                                },
                                "out_pid": 259,
                                "stream_type": 15
                            }
                        ]
                    }
                ]
            }
        }"#;
        let parsed: FlowConfig = serde_json::from_str(json).expect("parse");
        let a = parsed.assembly.as_ref().expect("assembly present");
        assert!(matches!(a.kind, AssemblyKind::Mpts));
        assert_eq!(a.programs.len(), 1);
        assert_eq!(a.programs[0].streams.len(), 2);
        assert!(matches!(&a.programs[0].streams[1].source, SlotSource::Hitless { .. }));
        let reser = serde_json::to_string(&parsed).unwrap();
        let reparsed: FlowConfig = serde_json::from_str(&reser).unwrap();
        assert_eq!(parsed.assembly, reparsed.assembly);
    }

    /// Absent `assembly` must not appear in the JSON and must still
    /// parse back as `None` — keeps legacy flows byte-clean on the wire.
    #[test]
    fn flow_assembly_skipped_when_unset() {
        let json = r#"{"id":"f","name":"F","input_ids":["a"],"output_ids":["o"]}"#;
        let parsed: FlowConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.assembly.is_none());
        let reser = serde_json::to_string(&parsed).unwrap();
        assert!(!reser.contains("assembly"), "assembly leaked into JSON: {reser}");
    }

    /// pid_map must round-trip through the JSON wire format on every
    /// TS-native output. Serde's default path for `BTreeMap<u16, u16>`
    /// breaks inside `#[serde(tag = "type")]` enums (`serde_json`'s
    /// `Content` intermediate drops the key coercion), so each field
    /// uses the shared `pid_map_serde` helper. This test pins that.
    #[test]
    fn pid_map_roundtrips_on_every_ts_native_output() {
        let cases = [
            r#"{"type":"udp","id":"u","name":"u","dest_addr":"239.0.0.1:5004","pid_map":{"256":768,"257":769}}"#,
            r#"{"type":"rtp","id":"r","name":"r","dest_addr":"239.0.0.1:5004","pid_map":{"256":768}}"#,
            r#"{"type":"srt","id":"s","name":"s","mode":"caller","remote_addr":"1.2.3.4:6000","pid_map":{"256":768}}"#,
            r#"{"type":"rist","id":"ri","name":"ri","remote_addr":"1.2.3.4:6000","pid_map":{"256":768}}"#,
            r#"{"type":"hls","id":"h","name":"h","ingest_url":"https://x/y","pid_map":{"256":768}}"#,
        ];
        for json in cases {
            let parsed: OutputConfig = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("parse failed for {json}: {e}"));
            let reser = serde_json::to_string(&parsed).unwrap();
            assert!(reser.contains("\"pid_map\""), "pid_map stripped on reserialise: {reser}");
            let reparsed: OutputConfig = serde_json::from_str(&reser).unwrap();
            let map: BTreeMap<u16, u16> = match reparsed {
                OutputConfig::Udp(u) => u.pid_map.unwrap(),
                OutputConfig::Rtp(r) => r.pid_map.unwrap(),
                OutputConfig::Srt(s) => s.pid_map.unwrap(),
                OutputConfig::Rist(r) => r.pid_map.unwrap(),
                OutputConfig::Hls(h) => h.pid_map.unwrap(),
                _ => panic!("unexpected variant for {json}"),
            };
            assert_eq!(map.get(&256), Some(&768));
        }
    }

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
            version: 2,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            resource_limits: None,
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "rtp-in-1".to_string(),
                name: "RTP Input".to_string(),
                config: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
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
            }],
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                active: true,
                group: None,
                id: "rtp-out-1".to_string(),
                name: "Output 1".to_string(),
                dest_addr: "127.0.0.1:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: default_dscp(),
                redundancy: None,
                program_number: None,
                pid_map: None,
                delay: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            })],
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
                input_ids: vec!["rtp-in-1".to_string()],
                output_ids: vec!["rtp-out-1".to_string()],
                assembly: None,
            }],
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.flows.len(), 1);
        assert_eq!(parsed.flows[0].id, "test-flow");
        assert_eq!(parsed.flows[0].input_ids, vec!["rtp-in-1".to_string()]);
        assert_eq!(parsed.flows[0].output_ids, vec!["rtp-out-1"]);
        assert_eq!(parsed.inputs.len(), 1);
        assert_eq!(parsed.outputs.len(), 1);
    }

    #[test]
    fn test_resolve_flow() {
        let config = AppConfig {
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "in-1".to_string(),
                name: "Input 1".to_string(),
                config: InputConfig::Udp(UdpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    audio_encode: None,
                    transcode: None,
                    video_encode: None,
                }),
            }],
            outputs: vec![OutputConfig::Rtp(RtpOutputConfig {
                active: true,
                group: None,
                id: "out-1".to_string(),
                name: "Output 1".to_string(),
                dest_addr: "127.0.0.1:5004".to_string(),
                bind_addr: None,
                interface_addr: None,
                fec_encode: None,
                dscp: default_dscp(),
                redundancy: None,
                program_number: None,
                pid_map: None,
                delay: None,
                audio_encode: None,
                transcode: None,
                video_encode: None,
            })],
            flows: vec![FlowConfig {
                id: "flow-1".to_string(),
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
            }],
            ..Default::default()
        };

        let resolved = config.resolve_flow(&config.flows[0]).unwrap();
        assert_eq!(resolved.inputs.len(), 1);
        assert!(resolved.active_input().is_some());
        assert_eq!(resolved.outputs.len(), 1);
        assert_eq!(resolved.outputs[0].id(), "out-1");
    }

    #[test]
    fn test_resolve_flow_missing_input() {
        let config = AppConfig {
            flows: vec![FlowConfig {
                id: "flow-1".to_string(),
                name: "Flow 1".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["nonexistent".to_string()],
                output_ids: vec![],
                assembly: None,
            }],
            ..Default::default()
        };

        assert!(config.resolve_flow(&config.flows[0]).is_err());
    }

    #[test]
    fn test_srt_config_with_redundancy() {
        let json = r#"{
            "version": 2,
            "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
            "inputs": [{
                "id": "srt-in",
                "name": "SRT Redundant",
                "type": "srt",
                "mode": "listener",
                "local_addr": "0.0.0.0:9000",
                "latency_ms": 500,
                "redundancy": {
                    "mode": "listener",
                    "local_addr": "0.0.0.0:9001",
                    "latency_ms": 500
                }
            }],
            "outputs": [{
                "type": "rtp",
                "id": "out-1",
                "name": "Output",
                "dest_addr": "192.168.1.50:5004"
            }],
            "flows": [{
                "id": "srt-flow",
                "name": "SRT Flow",
                "enabled": true,
                "input_ids": ["srt-in"],
                "output_ids": ["out-1"]
            }]
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.inputs.len(), 1);
        if let InputConfig::Srt(srt) = &config.inputs[0].config {
            assert!(srt.redundancy.is_some());
        } else {
            panic!("Expected SRT input");
        }
        assert_eq!(config.flows[0].input_ids, vec!["srt-in".to_string()]);
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.version, 2);
        assert_eq!(config.server.listen_port, 8080);
        assert!(config.flows.is_empty());
        assert!(config.inputs.is_empty());
        assert!(config.outputs.is_empty());
    }

    #[test]
    fn test_flow_using_input_output() {
        let config = AppConfig {
            flows: vec![FlowConfig {
                id: "f1".to_string(),
                name: "F1".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["in-1".to_string()],
                output_ids: vec!["out-1".to_string(), "out-2".to_string()],
                assembly: None,
            }],
            ..Default::default()
        };

        assert!(config.flow_using_input("in-1").is_some());
        assert!(config.flow_using_input("in-2").is_none());
        assert!(config.flow_using_output("out-1").is_some());
        assert!(config.flow_using_output("out-2").is_some());
        assert!(config.flow_using_output("out-3").is_none());
    }
}
