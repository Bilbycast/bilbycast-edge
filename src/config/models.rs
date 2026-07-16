// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Host-level physical-uplink capacity declaration for the shared-leg
/// capacity broker (`engine::bond_leg_broker`). Names a NIC and its real
/// usable uplink bandwidth so the broker can divide it fairly across the
/// bonded flows that share it. For cellular / Starlink links whose capacity
/// varies, this is the safe upper bound the broker's adaptive estimate never
/// probes past (see `../bilbycast-bonding/docs/shared-leg-capacity-broker.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BondUplinkConfig {
    /// NIC name the bonded legs pin to (e.g. `"eno4"`, `"wwp151s0u1i4"`,
    /// `"wlo5"`). Matched against each bonded leg's `interface`.
    pub interface: String,
    /// Physical uplink capacity, bits/sec — the amount divided across all
    /// bonded flows sharing this NIC.
    pub capacity_bps: u64,
    /// Optional per-flow minimum-viable rate on this leg, bits/sec. When the
    /// sum of viable minimums across the flows on the leg exceeds
    /// `capacity_bps`, the broker flags admission pressure. Default 200 kbps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_viable_bps: Option<u64>,
    /// Optional per-uplink override of the broker's activity-grace threshold,
    /// bits/sec — a flow delivering at least this on the leg keeps its
    /// min-viable reservation; below it (once the grace window lapses) it
    /// releases its share so a co-flow that actually wants the leg can use it.
    /// Unset → broker default (50 kbps). Raise it when sporadic keyframe-dup
    /// traffic on a leg a flow doesn't really use holds an unused reservation
    /// and under-utilises the leg for the flow that does want it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand_active_bps: Option<u64>,
}

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
    /// One-shot bearer token gating the `/setup` wizard against non-loopback
    /// callers. Auto-generated on first boot when `setup_enabled` is true and
    /// no token exists in `secrets.json`; cleared on first successful manager
    /// registration. Persisted in `secrets.json` (encrypted), never in
    /// `config.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_token: Option<String>,
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
    /// OPTIONAL host-level policy caps for the shared-leg capacity broker — one
    /// hard ceiling per physical NIC that the broker's auto-discovered capacity
    /// estimate must never probe past. Only needed for a metered / rate-limited
    /// link whose ceiling the broker can't infer from packet loss; on a normal
    /// link leave this empty and the broker self-discovers the capacity. Listing
    /// an uplink is NOT what enables the broker — it is on by default (see
    /// `shared_leg_broker`). See `engine::bond_leg_broker` /
    /// `../bilbycast-bonding/docs/shared-leg-capacity-broker.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bond_uplinks: Vec<BondUplinkConfig>,
    /// Explicit on/off for the shared-leg broker. `None` → ON (the default): the
    /// broker runs, auto-discovers each shared NIC's capacity, and reserves it
    /// across co-located bonded flows by each flow's priority — `bond_uplinks`
    /// emptiness has no bearing on whether it is enabled. `Some(false)` disables
    /// it (revert to uncoordinated per-bond contention). A lone flow on a leg is
    /// never throttled regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_leg_broker: Option<bool>,
    /// Optional structured-JSON log shipper for SIEM / NMS / observability
    /// pickup (Splunk, Skyline DataMiner, generic JSON-line ingesters). When
    /// configured, every operational event the edge emits is also written as
    /// a single JSON line to the chosen sink (file / syslog / stdout) in
    /// addition to the existing Prometheus metrics and manager WS push.
    /// See [`LoggingConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingConfig>,
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
    /// Optional AMWA IS-04 registration-client configuration. When `enabled`,
    /// the edge POSTs its node + device + sources + flows + senders + receivers
    /// to an external NMOS registry (e.g. Celebrum, Riedel MediorNet Control,
    /// Lawo VSM, EVS Cerebrum) and heartbeats the node so the registry's query
    /// API surfaces the edge to registry-driven NMOS controllers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nmos_registration: Option<NmosRegistrationConfig>,
    /// Remote-upgrade configuration. When `enabled`, the edge accepts
    /// `upgrade_binary` commands from the manager: fetch a Sigstore-signed
    /// manifest from the hardcoded `github.com/Bilbycast/*` release URL,
    /// verify the keyless signature against the compiled-in identity
    /// allowlist, download + extract the tarball, atomically swap the
    /// `current` symlink, and exit so systemd respawns into the new
    /// binary. Off by default — operators opt in per node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrades: Option<UpgradeConfig>,
    /// Optional cellular-uplink telemetry sources (RutOS routers). Modems
    /// managed by ModemManager are auto-detected and need **no** entry here.
    /// Read-only; off the data path. See `docs/cellular.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cellular_uplinks: Vec<CellularUplinkConfig>,
    /// Optional Starlink dish telemetry sources. Each entry names the kernel
    /// netdev to annotate and the dish gRPC address (default
    /// `192.168.100.1:9200`). Read-only; off the data path. No credential is
    /// needed — the dish gRPC is unauthenticated on the LAN. See
    /// `docs/starlink.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub starlink_uplinks: Vec<StarlinkUplinkConfig>,
}

/// One opt-in cellular-uplink telemetry source. Only RutOS routers need an
/// entry (`kind = "rutos"`); USB/PCIe modems are auto-detected via ModemManager.
/// The radio state read from this source is attached, read-only, to the kernel
/// `interface` it annotates (`HealthPayload.network_interfaces[].cellular`).
///
/// The `password` is an **infrastructure secret**: it lives only in
/// `secrets.json` (keyed by `interface`), is stripped from `GetConfig`, and is
/// re-merged on `UpdateConfig` — the manager never round-trips it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellularUplinkConfig {
    /// Kernel netdev this annotates, e.g. `"eno4"`.
    pub interface: String,
    /// Source kind. Only `"rutos"` is read by this source today; other values
    /// are ignored here (modems auto-detect).
    #[serde(default = "default_cellular_kind")]
    pub kind: String,
    /// `"http"` | `"https"` (default `"https"`).
    #[serde(default = "default_cellular_scheme")]
    pub scheme: String,
    /// Router host or IP (no scheme), e.g. `"192.168.1.1"`.
    pub address: String,
    /// `"ubus"` (default, broad compatibility) | `"rest"` (RutOS 7.x).
    #[serde(default = "default_cellular_api")]
    pub api: String,
    /// Read-only RutOS username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Read-only RutOS password. **Secret** — stored in `secrets.json`,
    /// stripped from `GetConfig`. Never set in `config.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Verify the router's TLS chain. Defaults `false` because RutOS ships a
    /// self-signed cert; prefer a `cert_fingerprint` pin over CA validation.
    #[serde(default)]
    pub verify_tls: bool,
    /// Optional SHA-256 cert fingerprint pin (hex, `:`-separators allowed).
    /// Stronger than CA validation for a self-signed router.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,
}

fn default_cellular_kind() -> String {
    "rutos".to_string()
}
fn default_cellular_scheme() -> String {
    "https".to_string()
}
fn default_cellular_api() -> String {
    "ubus".to_string()
}

/// One opt-in Starlink dish telemetry source. The dish link state read from
/// this source is attached, read-only, to the kernel `interface` it annotates
/// (`HealthPayload.network_interfaces[].starlink`).
///
/// Unlike [`CellularUplinkConfig`] there is **no credential / secret** — the
/// dish gRPC is unauthenticated on the LAN, so this whole struct lives in
/// `config.json` with nothing split into `secrets.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StarlinkUplinkConfig {
    /// Kernel netdev this annotates, e.g. `"wlo5"`.
    pub interface: String,
    /// Dish gRPC endpoint `host[:port]` (no scheme / path). Defaults to the
    /// well-known dish management address `192.168.100.1:9200`; the port
    /// defaults to 9200 when omitted.
    #[serde(default = "default_starlink_address")]
    pub address: String,
    /// Optional source IP to bind this dish's poll to. **Only needed for more
    /// than one dish on the same host:** every Starlink dish hard-codes the
    /// identical management address (`192.168.100.1:9200`), so two dishes on
    /// different interfaces collide on the host route. Set each leg's source IP
    /// here (the edge binds the poll to it via `local_address`) and add a
    /// per-leg policy route on the host (`ip rule from <source_address> table N`
    /// + the dish route in table N) so each poll egresses the right interface.
    /// Unset (single dish) → no bind; the main-table route is used (unchanged).
    /// See `docs/starlink.md` ("Multiple dishes on one host").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_address: Option<String>,
    /// Optional gateway (the Starlink Wi-Fi router) for the dish's management
    /// subnet, e.g. `"192.168.4.1"`. When set, the edge programs + re-asserts
    /// the route to the dish subnet (`<dish>/24 via <gateway> dev <interface>`,
    /// MAIN table) itself — so the operator never re-adds it by hand after the
    /// Wi-Fi link cycles. When unset, the edge derives `.1` of the interface's
    /// IPv4 subnet (the usual Starlink router address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

/// The well-known Starlink dish gRPC management endpoint. SpaceX fixes this to
/// the same value on every terminal, so it's a sensible default — but it is only
/// a default: each uplink's `address` is fully operator-overridable (e.g. a
/// re-IP'd dish, a different management address, or a NATed reach). The single
/// source of truth referenced by the config default, the gRPC probe fallback,
/// and the manager `test_starlink_uplink` handler.
pub const DEFAULT_DISH_ADDR: &str = "192.168.100.1:9200";

fn default_starlink_address() -> String {
    DEFAULT_DISH_ADDR.to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 2,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            setup_token: None,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            resource_limits: None,
            logging: None,
            flows: Vec::new(),
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
            nmos_registration: None,
            upgrades: None,
            cellular_uplinks: Vec::new(),
            starlink_uplinks: Vec::new(),
            bond_uplinks: Vec::new(),
            shared_leg_broker: None,
        }
    }
}

/// Remote-upgrade configuration. See `bilbycast-edge/docs/upgrade.md` for the
/// operator guide and the trust model.
///
/// Trust roots (Fulcio CA root, Rekor public key, identity allowlist) are
/// **compiled into the binary** — operators never configure them. The fields
/// here only gate operator-controlled scheduling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradeConfig {
    /// Master switch. Default `false` — operators opt in per node. Even when
    /// the manager schedules an upgrade, an edge with `enabled: false` rejects
    /// it with `error_code: "upgrade_disabled"` (no download attempted).
    #[serde(default)]
    pub enabled: bool,
    /// Channels this node is allowed to install. Default `["stable"]`.
    /// `"nightly"` and `"beta"` available for testbed nodes; commands carrying
    /// any other channel are rejected with `upgrade_channel_not_allowed`.
    #[serde(default = "default_allowed_channels")]
    pub allowed_channels: Vec<String>,
    /// Optional minimum version floor. The edge rejects any upgrade whose
    /// manifest declares a lower semver. Stored as a string so the wire shape
    /// is forgiving — parsed lazily at validate-time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    /// How many minor versions back from the currently installed version the
    /// edge is willing to roll *forward* to. Default `1` — anything older
    /// than `current_minor - 1` is rejected with `upgrade_version_too_old`,
    /// even if the manager pushes a manifest newer than `min_version`.
    #[serde(default = "default_rollback_grace")]
    pub rollback_grace: u32,
    /// On-disk install root. Default `/opt/bilbycast/edge`. The edge writes
    /// `versions/<v>/`, `current` symlink, `previous` symlink, and
    /// `state.json` under this directory.
    #[serde(default = "default_install_root")]
    pub install_root: std::path::PathBuf,
    /// Window after a respawn during which the new binary must produce
    /// healthy manager beats; if it fails to authenticate within this window,
    /// the boot watchdog rolls the symlink back to `previous` and emits
    /// `upgrade_rolled_back`. Default 120 s.
    #[serde(default = "default_boot_health_window_secs")]
    pub boot_health_window_secs: u32,
    /// Maximum boot attempts on the new binary before automatic rollback.
    /// `state.json.boot_attempts` increments on every respawn into a
    /// `pending_health` state; on the (max+1)th boot the watchdog reverts.
    /// Default `3`.
    #[serde(default = "default_max_boot_attempts")]
    pub max_boot_attempts: u32,
    /// When true, the manager can stage an upgrade (download + verify +
    /// extract under `versions/<new>/`) but the symlink swap waits for a
    /// local `kill -SIGUSR1 <pid>` so a human operator approves the actual
    /// switchover. Default `false`.
    #[serde(default)]
    pub manual_only: bool,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_channels: default_allowed_channels(),
            min_version: None,
            rollback_grace: default_rollback_grace(),
            install_root: default_install_root(),
            boot_health_window_secs: default_boot_health_window_secs(),
            max_boot_attempts: default_max_boot_attempts(),
            manual_only: false,
        }
    }
}

fn default_allowed_channels() -> Vec<String> {
    vec!["stable".to_string()]
}
fn default_rollback_grace() -> u32 {
    1
}
fn default_install_root() -> std::path::PathBuf {
    std::path::PathBuf::from("/opt/bilbycast/edge")
}
fn default_boot_health_window_secs() -> u32 {
    120
}
fn default_max_boot_attempts() -> u32 {
    3
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
    /// API listen address, e.g. "0.0.0.0". Legacy single-address field.
    /// When [`listen_addrs`] is set, this is ignored on bind. Kept for
    /// backward compatibility with pre-dual-stack configs.
    pub listen_addr: String,
    /// API listen port, default 8080
    pub listen_port: u16,
    /// Dual-stack listener addresses. When set, the API server binds one
    /// listener per entry (e.g. `["0.0.0.0", "[::]"]` for dual-stack).
    /// IPv6 entries get `IPV6_V6ONLY=1` so they coexist with v4 listeners
    /// on the same port without overlap. Unset = fall back to
    /// `[listen_addr]` (legacy behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addrs: Option<Vec<String>>,
    /// Optional TLS configuration for HTTPS on the API server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    /// Optional OAuth 2.0 authentication configuration.
    /// When absent or `enabled: false`, all endpoints are unauthenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::api::auth::AuthConfig>,
}

impl ServerConfig {
    /// Resolve the effective list of bind addresses. Falls back to
    /// `[listen_addr]` when [`listen_addrs`] is unset or empty.
    pub fn effective_listen_addrs(&self) -> Vec<String> {
        match &self.listen_addrs {
            Some(addrs) if !addrs.is_empty() => addrs.clone(),
            _ => vec![self.listen_addr.clone()],
        }
    }
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
            // Loopback-only by default (defense-in-depth). The local HTTP
            // API + setup wizard ship with auth disabled, and the edge's
            // control plane is the OUTBOUND manager WebSocket — it needs no
            // inbound listener to be managed. Operators who want LAN/remote
            // access override with `--bind-addrs 0.0.0.0,[::]` (or set
            // `server.listen_addrs` in config.json) and should also enable
            // `server.auth`. Only affects fresh configs — existing
            // config.json files keep their explicit address (loopback IPv4 +
            // IPv6 here so the dual-stack listener path stays exercised).
            listen_addr: "127.0.0.1".to_string(),
            listen_addrs: Some(vec!["127.0.0.1".to_string(), "[::1]".to_string()]),
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
    /// Dashboard listen address, e.g. "0.0.0.0". Legacy single-address field.
    /// Ignored when [`listen_addrs`] is set.
    pub listen_addr: String,
    /// Dual-stack listener addresses for the dashboard. Same semantics as
    /// [`ServerConfig::listen_addrs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addrs: Option<Vec<String>>,
    /// Dashboard listen port, e.g. 9090
    pub listen_port: u16,
}

impl MonitorConfig {
    /// Resolve the effective list of bind addresses. Falls back to
    /// `[listen_addr]` when [`listen_addrs`] is unset or empty.
    pub fn effective_listen_addrs(&self) -> Vec<String> {
        match &self.listen_addrs {
            Some(addrs) if !addrs.is_empty() => addrs.clone(),
            _ => vec![self.listen_addr.clone()],
        }
    }
}

/// Structured-JSON log shipper configuration.
///
/// When `json_target` is set, every operational `Event` the edge emits (the
/// same stream that flows over the manager WS `event` channel) is also
/// written as a single-line JSON record to the configured sink. This lets
/// Splunk / Skyline DataMiner / Loki / generic syslog stacks pick up the
/// edge's events without polling the manager.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    /// JSON event sink. `None` disables the shipper (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_target: Option<JsonLogTarget>,
}

/// JSON log sink — exactly one of `file`, `syslog`, or `stdout`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JsonLogTarget {
    /// Write JSON lines to stdout. Useful in container environments where
    /// the runtime captures stdout and forwards it to the log aggregator.
    Stdout {
        #[serde(default)]
        format: LogFormat,
    },
    /// Append JSON lines to a file with simple size-based rotation.
    /// Filename rotation: `<path>.1` is the most recent backup, `<path>.2`
    /// is older, etc. The oldest backup beyond `max_backups` is dropped.
    File {
        /// Absolute path to the active log file. Parent directory must exist.
        path: String,
        #[serde(default)]
        format: LogFormat,
        /// Rotate when the active file exceeds this size in megabytes.
        /// Default: 64. Range: 1..=4096.
        #[serde(default = "default_max_size_mb")]
        max_size_mb: u32,
        /// Maximum number of rotated backup files to retain. Default: 5.
        /// Range: 0..=100. Set to 0 to truncate on rotate (no backups).
        #[serde(default = "default_max_backups")]
        max_backups: u32,
    },
    /// UDP syslog (RFC 5424) target. The shipper is fire-and-forget — a
    /// black-holed endpoint never blocks the edge.
    Syslog {
        /// Syslog destination, e.g. `127.0.0.1:514` or `siem.internal:6514`.
        addr: String,
        #[serde(default)]
        format: LogFormat,
    },
}

/// JSON envelope shape. Different SIEMs prefer slightly different field
/// names; the shipper renders the same event with the chosen layout.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// Generic single-line JSON envelope (default).
    #[default]
    Raw,
    /// Splunk HEC-friendly: wraps the envelope inside a top-level
    /// `{"event": ...}` object so a Splunk HTTP Event Collector forwarder
    /// can ingest the same line.
    Splunk,
    /// Skyline DataMiner-friendly field renames (`error_code` →
    /// `parameter_id`).
    Dataminer,
}

fn default_max_size_mb() -> u32 {
    64
}

fn default_max_backups() -> u32 {
    5
}

/// AMWA IS-04 registration-client configuration.
///
/// When `enabled`, a background task POSTs this node's IS-04 resource set
/// (node + device + sources + flows + senders + receivers) to the configured
/// registry's `/x-nmos/registration/<api_version>/resource` endpoint, and
/// heartbeats the node every `heartbeat_interval_secs` against the registry's
/// `/health/nodes/{id}` endpoint. On shutdown the node resource is DELETEd
/// best-effort so the registry stops advertising it.
///
/// Resource UUIDs are deterministic UUID v5 values derived from `node_id` so
/// they are stable across restarts — re-POSTing the same resource set updates
/// the registry record in place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NmosRegistrationConfig {
    /// Whether registration is active. When false the task is not spawned and
    /// the rest of this struct is ignored.
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the NMOS registry, e.g. `https://registry.example.com:8235`
    /// or `http://localhost:8235`. Path components are appended internally —
    /// do not include `/x-nmos/...` in this URL.
    pub registry_url: String,
    /// IS-04 registration API version. Currently the only supported value is
    /// `v1.3`; older registries that only speak v1.2/v1.1 are out of scope
    /// for this release.
    #[serde(default = "default_nmos_api_version")]
    pub api_version: String,
    /// How often to POST a `health/nodes/{id}` heartbeat to the registry.
    /// AMWA recommends 5 s; the registry treats nodes as expired after roughly
    /// 12 s of missed heartbeats.
    #[serde(default = "default_nmos_heartbeat_secs")]
    pub heartbeat_interval_secs: u32,
    /// HTTP request timeout for registration / heartbeat / delete requests.
    #[serde(default = "default_nmos_timeout_secs")]
    pub request_timeout_secs: u32,
    /// Optional Bearer token to include on every registry request. Mirrors the
    /// node-secret pattern: persisted in `secrets.json` (envelope-encrypted),
    /// stripped before sending the config to the manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

fn default_nmos_api_version() -> String {
    "v1.3".to_string()
}

fn default_nmos_heartbeat_secs() -> u32 {
    5
}

fn default_nmos_timeout_secs() -> u32 {
    10
}

impl Default for NmosRegistrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            registry_url: String::new(),
            api_version: default_nmos_api_version(),
            heartbeat_interval_secs: default_nmos_heartbeat_secs(),
            request_timeout_secs: default_nmos_timeout_secs(),
            bearer_token: None,
        }
    }
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
    /// How often (in seconds) to capture a fresh thumbnail. `None` keeps the
    /// 5 s default. Lower = more frequent previews at proportionally higher
    /// decode CPU cost. Validated to `1..=60`; the manager UI offers a
    /// curated set (1 / 2 / 5 / 10 / 30 s). The freeze and no-signal
    /// detection windows scale with this value so the alarms stay correct
    /// at any cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_interval_secs: Option<u32>,
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
    /// Optional in-depth content analysis tiers. Three independent toggles:
    /// Lite (compressed-domain, GOP / signalling / captions / SCTE-35 / MDI),
    /// Audio Full (R128 / true-peak / silence), and Video Full (blockiness /
    /// blur / letterbox / colour-bar / freeze). When the field is absent the
    /// effective tier is `Lite=on, AudioFull=off, VideoFull=off` — see
    /// [`ContentAnalysisConfig::default`]. Present on every flow type; tiers
    /// gracefully no-op on inputs that can't supply the relevant essence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_analysis: Option<ContentAnalysisConfig>,
    /// Optional per-flow continuous recording to disk. When set, a sibling
    /// subscriber drains the flow's broadcast channel and writes rolling
    /// MPEG-TS segments to the local replay store (see [`RecordingConfig`]).
    /// Composes with passthrough + assembled flows — the bytes recorded are
    /// always the bytes the outputs see. Drop-on-lag, never blocks the data
    /// path. Requires the `replay` Cargo feature; on binaries without it,
    /// configs carrying this field still load but the runtime emits a
    /// Critical event and refuses to bring up the recorder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingConfig>,
    /// Optional per-flow master-clock configuration. Drives PCR
    /// generation, output emission timing, and lipsync trim. When unset,
    /// the runtime auto-selects based on the active input (see
    /// `engine::master_clock::select_master_kind_for_input`). Operators
    /// override here when they want the same flow on every plant edge to
    /// share a PTP grandmaster regardless of input type, or when they
    /// know the source-PCR PLL is the right call even if the auto-policy
    /// disagrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_clock: Option<MasterClockConfig>,
    /// Optional per-flow bandwidth profile. Sizes the flow's broadcast
    /// channels (fan-out + per-input pre-broadcast + transcode chain
    /// hand-off) to give enough headroom for the active essence's
    /// bitrate class. `None` (default) → auto-derive from the flow's
    /// inputs (uncompressed ST 2110 / MXL video → `Uncompressed`,
    /// everything else → `Standard`). Operators override here for the
    /// rare case of >500 Mbps compressed video where Standard's
    /// jitter headroom is tight. See
    /// `engine::bandwidth_profile::resolve_for_flow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_profile: Option<BandwidthProfile>,
}

/// Channel-capacity tier for a flow's broadcast-class channels.
///
/// Each broadcast slot holds one [`crate::engine::packet::RtpPacket`]
/// (1316 bytes typical for 7×188 TS bundled into RTP). The tier sets
/// the slot count, which translates to a wallclock jitter budget at
/// the flow's bitrate:
///
/// | Tier            | Slots   | At 50 Mbps | At 500 Mbps | At 3 Gbps | At 12 Gbps |
/// |-----------------|--------:|-----------:|------------:|----------:|-----------:|
/// | `Standard`      | 16 384  | 3.4 s      | 344 ms      | 56 ms     | 14 ms      |
/// | `HighBitrate`   | 32 768  | 6.9 s      | 690 ms      | 115 ms    | 28 ms      |
/// | `Uncompressed`  | 65 536  | 13.8 s     | 1.38 s      | 230 ms    | 57 ms      |
///
/// Memory cost per broadcast channel: ~21 MB / 43 MB / 86 MB. Per
/// flow the cost multiplies by the number of broadcast-class channels
/// (the flow's main fan-out + one per-input pre-broadcast + the
/// fixer command channel — typically 3 to 10 channels depending on
/// the number of inputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandwidthProfile {
    /// 16 384 slots. The default for TS-carrying flows up to ~500 Mbps
    /// compressed video. Generous jitter budget on every typical
    /// contribution / distribution flow.
    Standard,
    /// 32 768 slots. For compressed video flows in the 500 Mbps –
    /// 3 Gbps range — UHD HEVC contribution, JPEG XS (ST 2110-22 when
    /// it lands), or a hitless dual-leg at the top of the compressed
    /// envelope.
    HighBitrate,
    /// 65 536 slots. For uncompressed essence — ST 2110-20 / -23 video,
    /// MXL video. Auto-selected when the flow's inputs include any
    /// uncompressed-essence type.
    Uncompressed,
}

impl BandwidthProfile {
    /// Broadcast channel capacity (slots) for this profile.
    pub const fn broadcast_capacity(self) -> usize {
        match self {
            Self::Standard => 16_384,
            Self::HighBitrate => 32_768,
            Self::Uncompressed => 65_536,
        }
    }
}

impl Default for BandwidthProfile {
    fn default() -> Self {
        Self::Standard
    }
}

/// Per-flow master-clock override. Mirrors the
/// `engine::master_clock::MasterClockKind` enum on the wire so the same
/// label appears in both config and telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasterClockConfig {
    /// Which master-clock backend to use for this flow.
    pub kind: MasterClockKindConfig,
    /// Lipsync trim in 90 kHz ticks. Bounded ±18 000 (±200 ms) at
    /// runtime; values outside that range are clamped, not rejected.
    /// Default 0.
    #[serde(default)]
    pub lipsync_offset_90k: i64,
    /// PLL-lock timeout in seconds. When the `kind` is
    /// `source_pcr_pll` (or `auto` selects it for the active input)
    /// and the PLL hasn't locked within this many seconds, the master
    /// clock falls back to a free-running wallclock master and emits
    /// a Warning event with the lock failure reason. `None` (or
    /// unset) → default 30 s. Set to `0` to disable the fallback
    /// entirely (PLL strict-mode — flow stays unlocked indefinitely
    /// on unlockable sources). Range when set: 5..=300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pll_lock_timeout_s: Option<u32>,
    /// p99 residual-jitter threshold (µs) at which the PLL transitions
    /// to `locked = true`. `None` → broadcast-tier default of 100 µs,
    /// calibrated for hardware-paced contribution sources (Appear X,
    /// Cobalt, Cisco encoders). Customers with prosumer encoders or
    /// internet contribution paths over high-jitter networks routinely
    /// see source PCR-vs-wallclock jitter in the 500 µs – 5 ms range
    /// even on healthy streams, which never crosses the broadcast
    /// threshold. Raising this to 500–2000 lets the PLL lock on those
    /// sources at the cost of a less tight clock recovery. Range when
    /// set: 50..=5000. The matching unlock threshold scales as 5× the
    /// lock value, preserving the threshold-level hysteresis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pll_lock_jitter_us: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterClockKindConfig {
    SourcePcrPll,
    Ptp,
    AudioMaster,
    /// "This flow doesn't need a recovered source clock." Output PCR
    /// comes from source bytes (passthrough) or source PTS
    /// (transcoded) so the master clock is informational only. Backing
    /// runtime is `wallclock` but no PLL spawns, no fallback alarm.
    /// Default for most contribution-to-distribution flows when the
    /// operator hasn't pinned `source_pcr_pll` explicitly.
    Passthrough,
    /// Recover rate from the SRT/RIST sender's per-packet timestamp
    /// instead of MPEG-TS PCR sampled from the bytes. Useful for
    /// internet-contribution paths where SRT's latency buffer makes
    /// PCR-from-bytes look jittery to the PLL but the underlying
    /// libsrt `srctime` reflects the sender's clock cleanly. See
    /// [`engine::master_clock::MasterClockKind::SenderTimestamp`] —
    /// framework only today; the srctime extraction is wired through
    /// in a follow-up. Selecting this kind on a non-SRT / non-RIST
    /// input is rejected by config validation.
    SenderTimestamp,
    Wallclock,
    /// Contribution-mode PLL: same runtime backend as `source_pcr_pll`
    /// (PI-controller PLL recovering source's 27 MHz from incoming PCR
    /// samples) but flags the operator's *intent* — "this is a clean
    /// contribution feed and I want the output PCR to track the source
    /// clock". The auto-policy maps SRT/RTP/UDP/RIST/RTMP/RTSP to
    /// `wallclock` by default (always-locked, monotonic, no
    /// discontinuity sensitivity) because most contribution sources
    /// have transient PCR discontinuities that prevent the PLL from
    /// locking. Operators who run on PTP-disciplined or clean-PCR
    /// sources opt into this mode to get cross-edge clock coherence.
    /// Telemetry preserves the `Contribution` label on
    /// `MasterClockTelemetry.configured_kind` so the manager UI can
    /// render "PLL (contribution)" distinct from a legacy
    /// `source_pcr_pll` explicit pin.
    Contribution,
    /// Per-input auto policy. **ST 2110 / MXL** inputs are PTP-domain
    /// essence, so they resolve straight to **PTP** (no PLL-first).
    /// **Contribution + assembly** inputs (SRT/RIST/RTP/UDP/RTMP/RTSP/…)
    /// get the cascade: **source PCR PLL → PTP → Wallclock**. The cascade
    /// first tries to lock the selected input's PCR via the source-PCR PLL
    /// (best — output tracks the source clock with zero source-relative
    /// drift). If the PLL can't lock within the grace window, it falls to
    /// the node's PTP clock when a PTP role is configured (`ptp.conf` !=
    /// off) AND PTP is healthy (slave `Locked`, or this node is the
    /// grandmaster) — a clean, cross-edge-coherent reference (genlock-free
    /// 2022-7). Otherwise it falls to wallclock, the always-available floor.
    ///
    /// The PTP-vs-wallclock choice is latched when the PLL gives up, and
    /// only ever demotes one-way (PTP → wallclock) if PTP later loses
    /// lock — so the output never rides an unlocked `CLOCK_REALTIME` and
    /// never oscillates epochs. Telemetry reports `configured_kind =
    /// "auto"` with `kind` = the active rung, so the manager UI renders
    /// "Auto → Source PCR PLL" / "Auto → PTP" / "Auto → Wallclock".
    Auto,
}

/// Per-flow continuous-recording attributes. When attached to a
/// [`FlowConfig`], a dedicated subscriber on the flow's broadcast channel
/// writes rolling 188-byte-aligned MPEG-TS segments under
/// `<replay_root>/<storage_id or flow_id>/` and maintains a side-car
/// timecode → byte-offset index for fast scrub. Retention enforces both
/// `retention_seconds` and `max_bytes`, evicting oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Whether the recorder is currently armed. Default true (start with
    /// the flow). Setting false leaves the configuration in place but
    /// stops the writer task — useful for gating recording on a routine.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Storage subdirectory under the replay root. Defaults to flow_id.
    /// Subject to the same character set as media-library filenames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_id: Option<String>,
    /// Rolling segment duration in seconds. Default 10. Range [2, 60].
    #[serde(default = "default_recording_segment_seconds")]
    pub segment_seconds: u32,
    /// Maximum recording age in seconds. Default 86_400 (24h). 0 = unlimited.
    #[serde(default = "default_recording_retention_seconds")]
    pub retention_seconds: u64,
    /// Maximum total bytes per recording. Default 50 GiB. 0 = unlimited
    /// (still subject to free disk space; writer emits Critical
    /// `replay_disk_full` and stops on actual exhaustion).
    #[serde(default = "default_recording_max_bytes")]
    pub max_bytes: u64,
    /// Pre-buffer (ring buffer) duration in seconds. When set, the
    /// writer auto-arms in `PreBuffer` mode at flow start and keeps
    /// the last N seconds of TS on disk even before the operator hits
    /// Start. On `start_recording` the existing pre-buffer becomes the
    /// head of the recording session (retention bumps to
    /// `retention_seconds`); on `stop_recording` the writer drops back
    /// to `PreBuffer` and segments older than N s prune naturally on
    /// the next roll. Range [1, 300] s. `None` = pre-buffer disabled
    /// (Phase 1 behaviour: writer only runs while explicitly armed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_buffer_seconds: Option<u32>,
    /// Filmstrip thumbnail cadence in seconds. When set, a sibling
    /// subscriber generates a small JPEG (160×90) every N seconds into
    /// `<recording_dir>/thumbs/<pts_90khz>.jpg`, used by the manager
    /// `/replay` page to render a visual scrubber strip behind the
    /// timeline canvas. Range [1, 30] s. `None` = filmstrip disabled
    /// (a recording-only flow keeps its existing CPU + disk shape).
    /// Independent from the live `FlowConfig.thumbnail` boolean — the
    /// live thumbnail goes to the manager via WS, the filmstrip goes to
    /// disk for later scrub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filmstrip_seconds: Option<u32>,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            storage_id: None,
            segment_seconds: default_recording_segment_seconds(),
            retention_seconds: default_recording_retention_seconds(),
            max_bytes: default_recording_max_bytes(),
            pre_buffer_seconds: None,
            filmstrip_seconds: None,
        }
    }
}

fn default_recording_segment_seconds() -> u32 {
    10
}
fn default_recording_retention_seconds() -> u64 {
    86_400
}
fn default_recording_max_bytes() -> u64 {
    50 * 1024 * 1024 * 1024
}

impl FlowConfig {
    /// Resolved content-analysis configuration with defaults filled in.
    ///
    /// Calling code never has to special-case `content_analysis = None` —
    /// this returns the same defaulted struct every time so `if cfg.lite { … }`
    /// works on legacy flows out of the box.
    pub fn effective_content_analysis(&self) -> ContentAnalysisConfig {
        self.content_analysis.clone().unwrap_or_default()
    }
}

/// Per-flow content-analysis tier selection.
///
/// Each tier toggles one independent analyser task that subscribes to the
/// flow's broadcast channel. Tiers are ordered by increasing CPU cost:
///
/// - **Lite** (`<1%` per core, no decode): TR-101290 extension —
///   GOP structure, HDR / aspect-ratio / colour-space / range / AFD,
///   SMPTE timecode, SEI caption presence, SCTE-35 cue presence,
///   Media Delivery Index (NDF / MLR per RFC 4445). Applies to TS-carrying
///   inputs; gracefully no-ops on PCM / ANC inputs.
///
/// - **Audio Full** (`~5–10%` per core): EBU R128 momentary / short-term /
///   integrated loudness, true peak (dBTP), silence / hard-mute / clipping
///   per audio PID. Decodes audio inline (AAC via fdk-aac-rs, MP2 / AC-3 /
///   E-AC-3 via ffmpeg-video-rs / libavcodec, raw PCM passthrough for
///   ST 2110-30 / -31 / `rtp_audio`).
///
/// - **Video Full** (`~10–25%` per core): blockiness, blur, letterbox /
///   pillarbox, colour-bar / test-pattern, slate, frozen-frame via YUV-SAD
///   (replaces the JPEG-hash heuristic in `engine::thumbnail`). Reuses the
///   existing FFmpeg decode pipeline; runs at `video_full_hz` (default 1 Hz).
///
/// All tiers are non-blocking — each runs in its own task, drops on
/// `broadcast::Lagged`, and never feeds back into the data path. Toggling
/// a tier off cancels its task within one packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentAnalysisConfig {
    /// Lite (compressed-domain) tier. **Default on** — cheap and broadly
    /// useful for remote-site triage.
    #[serde(default = "default_true")]
    pub lite: bool,
    /// Audio Full tier (R128 / true-peak / silence / mute / clipping).
    /// Default off; opt-in.
    #[serde(default)]
    pub audio_full: bool,
    /// Video Full tier (blockiness / blur / letterbox / colour-bar / slate /
    /// freeze). Default off; opt-in.
    #[serde(default)]
    pub video_full: bool,
    /// Override the Video Full sample rate in Hz. `None` = 1.0 Hz default.
    /// Bounded to `(0.0, 30.0]` by validation; values above 5 Hz cost CPU
    /// proportional to decode cadence and are mostly useful for benchmarks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_full_hz: Option<f32>,
}

impl Default for ContentAnalysisConfig {
    fn default() -> Self {
        Self {
            lite: true,
            audio_full: false,
            video_full: false,
            video_full_hz: None,
        }
    }
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
    Pid { input_id: String, source_pid: u16 },
    /// Broad essence selector — the assembler picks the first stream of
    /// the given kind from the named input's PMT. Useful when the input
    /// is single-program and the operator just wants "its video".
    Essence { input_id: String, kind: EssenceKind },
    /// Hitless SMPTE 2022-7-style pair. Both legs carry the same ES;
    /// the merger downstream dedupes by RTP sequence. Either nested
    /// source may itself be another `Pid` or `Essence` — nested
    /// `Hitless` is rejected at validation time.
    Hitless {
        primary: Box<SlotSource>,
        backup: Box<SlotSource>,
        /// Failover algorithm. Defaults to `PrimaryPreference` so
        /// existing configs keep their stall-timer-based failover. Set
        /// to `SeqAware` for true SMPTE 2022-7 hitless: the merger
        /// dedups + gap-fills against the upstream RTP / SRT sequence
        /// number on each leg, switching legs sub-frame instead of
        /// after a 200 ms stall.
        #[serde(default, skip_serializing_if = "HitlessMode::is_default")]
        mode: HitlessMode,
        /// Override for the stall timer (`PrimaryPreference` mode
        /// only — `SeqAware` ignores this). Range: 20..=5000 ms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stall_ms: Option<u64>,
        /// Reorder window for the seq-aware merger (`SeqAware` mode
        /// only). Multiple of 64 in 64..=4096; default 1024 (≈ 0.4 s
        /// at 2400 packets/s).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reorder_window: Option<u16>,
        /// Path-differential / skew-accommodation buffer in milliseconds
        /// (`SeqAware` mode only). When set, the merger holds every
        /// packet for this many ms after first arrival before releasing
        /// it in seq order — gives the slower leg the full window to
        /// deliver and lets a single-leg loss within the window be
        /// hitlessly filled. Range: 5..=2000 ms. Default: 50 ms.
        /// `None` falls back to a dedup-only path (lower latency, no
        /// asymmetric-path protection).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_differential_ms: Option<u32>,
    },
    /// Operator-driven N-input switch. The slot subscribes to every
    /// leg concurrently (warm) but only forwards bytes from the leg
    /// whose `input_id` equals the flow's current `active_input_id`.
    /// On `ActivateInput`, the assembler flips the active leg, bumps
    /// the owning PMT version, and arms DI=1 on the next PCR for the
    /// slot's `out_pid` so receivers stay locked without re-tuning.
    Switch {
        /// 1..=64 legs. Each leg names an input + a source descriptor.
        /// Nested `Switch` / `Hitless` is rejected at the type level
        /// because legs are a flat enum.
        legs: Vec<SwitchLeg>,
        /// Input ID of the leg that is active at flow bring-up. Must
        /// equal one of `legs[i].input_id`. At runtime, supersedes
        /// per `flow.active_input_id`.
        initial_input_id: String,
        /// PES Switch Phase 4. Controls what happens to outbound bytes
        /// during a `SwitchActiveInput`:
        /// - [`SpliceMode::PmtBump`] (default): receiver-friendly but
        ///   not access-unit-aligned. Today's behaviour — PMT version
        ///   bumps mod 32 and DI=1 is armed on the next PCR.
        /// - [`SpliceMode::PesAligned`]: hold the outbound stream at
        ///   the from-leg's last fully-emitted PES boundary, wait up
        ///   to [`splice_budget_ms`] for the to-leg to produce a PES
        ///   whose PTS is monotonically past the last emitted PTS,
        ///   then concatenate. On budget exhaustion the path falls
        ///   back to `PmtBump` and emits `pes_splice_timeout`.
        /// `PesAligned` is honoured for both audio (AAC / MP2 / AC-3 /
        /// E-AC-3, aligned on the next PES boundary, with an optional
        /// AAC-ADTS parameter check) and video (H.264 `0x1B` / HEVC
        /// `0x24`, IDR-aligned with an SPS codec-param check). Slots
        /// whose `stream_type` is neither a supported audio nor video
        /// codec (e.g. MPEG-2 video `0x02`) fall through to `PmtBump`
        /// and the assembler emits `pes_splice_degraded`.
        #[serde(default, skip_serializing_if = "SpliceMode::is_default")]
        splice_mode: SpliceMode,
        /// Splice budget in milliseconds (PES-aligned mode only).
        /// Default 200 ms for audio (enough for ≥8 audio frames at
        /// every common AAC / MP2 / AC-3 framerate). Range 20..=5000.
        /// Ignored when `splice_mode = PmtBump`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        splice_budget_ms: Option<u32>,
    },
}

/// PES Switch Phase 4. Splice strategy for `SlotSource::Switch` slots
/// when the operator drives `ActivateInput`. See [`SlotSource::Switch`]
/// for runtime semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpliceMode {
    /// PMT version bump + DI=1 on next PCR. Receiver-friendly but
    /// not access-unit-aligned — decoders re-acquire if the two
    /// sources are independent encoders of the same content.
    /// Backwards-compatible default.
    #[default]
    PmtBump,
    /// Hold the outbound stream at the from-leg's last fully-emitted
    /// PES boundary, then concatenate the to-leg's first PES whose
    /// PTS is monotonically past the last emitted PTS. Glitchless on
    /// real receivers (gate 7) when both sources are coherent content.
    PesAligned,
}

impl SpliceMode {
    pub fn is_default(&self) -> bool {
        matches!(self, SpliceMode::PmtBump)
    }
}

/// One leg of a [`SlotSource::Switch`]. Flat (not `Box<SlotSource>`)
/// so the type system rejects Switch-in-Switch and Switch-in-Hitless
/// without runtime checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwitchLeg {
    /// Explicit PID on a named input.
    Pid { input_id: String, source_pid: u16 },
    /// Auto-pick the first stream of a given kind from the input's PMT.
    Essence { input_id: String, kind: EssenceKind },
}

impl SwitchLeg {
    pub fn input_id(&self) -> &str {
        match self {
            SwitchLeg::Pid { input_id, .. } => input_id,
            SwitchLeg::Essence { input_id, .. } => input_id,
        }
    }
}

/// Hitless failover algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HitlessMode {
    /// Primary-preference with a stall timer. Forwards primary verbatim
    /// and flips to backup if no primary packet arrives within
    /// `stall_ms`. Backwards-compatible default.
    #[default]
    PrimaryPreference,
    /// True SMPTE 2022-7 seq-aware hitless. Both legs are observed
    /// continuously; the merger dedups + gap-fills against the
    /// upstream wire-level sequence number. Requires both legs'
    /// inputs to carry an `upstream_seq` (RTP / SRT — not raw TS).
    SeqAware,
}

impl HitlessMode {
    pub fn is_default(&self) -> bool {
        matches!(self, HitlessMode::PrimaryPreference)
    }
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

fn default_true_opt() -> Option<bool> {
    Some(true)
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

/// Per-socket physical interface binding.
///
/// Pins a UDP-socket-owning input or output to a specific NIC by name.
/// Two release tiers, picked by `strict`:
///
/// * **Loose** (`strict: false`): the edge resolves `name` to the
///   interface's first non-link-local IPv4 address and uses it as the
///   socket's source address (same effect as setting `interface_addr`
///   today). The kernel still consults the routing table — if the table
///   says the destination should leave a different NIC, it does. This
///   is advisory binding, no extra permissions required.
/// * **Strict** (`strict: true`): the edge additionally calls
///   `setsockopt(SO_BINDTODEVICE, name)` on the socket fd. The kernel
///   then refuses to send the packet on any other interface, regardless
///   of routing-table preference. Requires `CAP_NET_RAW` on the edge
///   process — operators opt in via the systemd drop-in
///   `packaging/strict-binding.conf`. Edges advertise the
///   `"interface-binding-strict"` capability only when a startup probe
///   confirms `setsockopt(SO_BINDTODEVICE)` succeeds.
///
/// `name` must match an interface enumerated on the host
/// (`HealthPayload.network_interfaces`). The same struct rides on every
/// UDP-based input + output config plus the redundancy / bonding
/// sub-blocks so 2022-7 dual-leg can pin Red and Blue to different NICs
/// and SRT bonding can pin each member to its own physical link.
///
/// Coexists with the legacy `interface_addr` / `bind_addr` /
/// `local_addr` fields — `interface_binding` wins when set; the legacy
/// fields stay for backward compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceBinding {
    /// NIC name as enumerated by the kernel, e.g. `"eno4"`. Must satisfy
    /// the Linux `IFNAMSIZ` constraint (≤ 15 bytes, alphanumeric + `._-`).
    pub name: String,
    /// When true, also call `setsockopt(SO_BINDTODEVICE, name)` so the
    /// kernel cannot route the socket to any other NIC. Requires
    /// `CAP_NET_RAW`. Default false (loose source-address binding only).
    #[serde(default)]
    pub strict: bool,
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
    /// File-based media player: replays a local `.ts` / `.mp4` / `.mov` /
    /// `.mkv` / image asset (single file or sequential playlist) as a
    /// fresh MPEG-TS feed onto the flow broadcast channel. Used as a
    /// fallback source under PID-bus Hitless when the live primary stalls,
    /// or as a manual idle fill. Files live under the edge's media
    /// library directory and are uploaded from the manager — see
    /// [`MediaPlayerInputConfig`] for source kinds.
    #[serde(rename = "media_player")]
    MediaPlayer(MediaPlayerInputConfig),
    /// Replay input: pump a previously-recorded flow's TS segments back
    /// onto the broadcast channel, PTS-paced. Designed for tier-2 sports
    /// instant-replay workflows — switch a flow's active input to this
    /// variant via the existing gap-free `activate_input` and the egress
    /// outputs (UDP/RTP/SRT/RIST/HLS/CMAF/WebRTC/RTMP) consume the
    /// playback unchanged. Mark / cue / play / scrub commands route to
    /// this input via the per-flow replay command channel — see the
    /// `replay` Cargo feature and `engine::input_replay` for runtime
    /// behaviour. Field schema in [`ReplayInputConfig`].
    #[serde(rename = "replay")]
    Replay(ReplayInputConfig),
    /// Consume a MXL (Media eXchange Layer) **video** flow — V210 4:2:2
    /// 10-bit grains from the shared-memory bus, encoded to H.264/HEVC
    /// and published as MPEG-TS onto the flow's broadcast channel.
    /// PTP-required. Gated by the `mxl` Cargo feature. See
    /// `docs/mxl-integration-plan.md`.
    #[serde(rename = "mxl_video")]
    MxlVideo(MxlVideoInputConfig),
    /// Consume a MXL **audio** flow — Float32 PCM @ 48 kHz samples from
    /// the shared-memory bus. Optional `audio_encode` synthesises an
    /// audio-only MPEG-TS for TS-bearing outputs (same shape as ST
    /// 2110-30). PTP-required. Gated by the `mxl` Cargo feature.
    #[serde(rename = "mxl_audio")]
    MxlAudio(MxlAudioInputConfig),
    /// Consume a MXL **ancillary** flow — RFC 8331 ANC grains from the
    /// shared-memory bus. SCTE-104, SMPTE 12M timecode, CEA-608/708
    /// captions. PTP-required. Gated by the `mxl` Cargo feature.
    #[serde(rename = "mxl_anc")]
    MxlAnc(MxlAncInputConfig),
    /// Capture SDI directly off a Blackmagic DeckLink card — packed 4:2:2
    /// video + embedded PCM audio from one device handle, encoded to
    /// H.264/HEVC and muxed into a single A+V MPEG-TS on the flow's
    /// broadcast channel. Self-clocked (no PTP requirement, unlike MXL).
    /// Gated by the `sdi-decklink` Cargo feature; requires a
    /// `video-encoder-*` backend. See `bilbycast-decklink-rs`.
    #[serde(rename = "sdi")]
    Sdi(SdiInputConfig),
}

/// File-backed replay input. Reads MPEG-TS segments from a local
/// recording (built by a flow with [`RecordingConfig`] attached) and
/// publishes paced packets onto the flow's broadcast channel. Mark /
/// cue / play / stop / scrub state lives in the input task — the WS
/// dispatcher routes commands via the per-flow replay command channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayInputConfig {
    /// ID of the recording on this edge's replay store. Required.
    /// Resolves against `<replay_root>/<recording_id>/`.
    pub recording_id: String,
    /// Optional clip ID. When set, only the clip's `[in_pts, out_pts]`
    /// range plays. Unset = whole recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_id: Option<String>,
    /// When true, restart at the beginning on EOF. Default false (idle on
    /// last frame, NULL-PID padding to keep downstream sockets alive).
    #[serde(default)]
    pub loop_playback: bool,
    /// Initial state on flow start. When true (default), the input
    /// publishes NULL-PID padding until a `play_clip` / `cue_clip`
    /// command activates playback. When false, playback begins immediately
    /// at the configured `clip_id` start (or the beginning of the
    /// recording).
    #[serde(default = "default_true")]
    pub start_paused: bool,
    /// Optional audio re-encode applied at ingress, before the bytes
    /// reach the flow broadcast channel. Same semantics as the live TS
    /// inputs (RTP/SRT/RTMP/...). When set, the recorded ES is decoded,
    /// optionally reshaped via `transcode`, re-encoded to the configured
    /// codec, and re-muxed back into the published TS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode (channel shuffle / sample-rate /
    /// bit-depth) applied between decode and `audio_encode`. Ignored
    /// when `audio_encode` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional video re-encode applied at ingress. Decodes the recorded
    /// video ES, re-encodes via the configured backend, and re-muxes the
    /// output TS. Feature-gated like every other ingress `video_encode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
}

/// What each synthetic audio channel carries in a test-pattern input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestPatternAudioContent {
    /// Classic line-up: the same sine tone on every channel.
    #[default]
    Tone,
    /// Each channel announces its own 1-based number so an operator can
    /// tell channels apart by ear — a looped spoken digit when a voice
    /// clip is available in the testgen-voice directory, otherwise N
    /// counted beeps per cycle as a built-in fallback.
    ChannelIdent,
}

/// Visual style of the A/V-sync marker when `av_sync_marker` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestPatternAvSyncStyle {
    /// Corner luma patch that flashes on the beep (EBU R 49 / 2-pop).
    #[default]
    Flash,
    /// A dot orbits a ring once per second; the beep fires as it crosses
    /// 12 o'clock. Read A/V skew from the dot's position when you hear the
    /// pip — more intuitive than a flash for eyeballing offset.
    Sweep,
}

/// How the per-channel number idents are arranged in time when
/// `audio_content = channel_ident`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestPatternChannelIdentLayout {
    /// Round-robin: channel N announces in its own one-second slot, so the
    /// numbers are heard one at a time even on a summed / downmixed monitor
    /// (the BLITS / GLITS approach to surround channel identification). N
    /// channels take N seconds per loop. Default — matches the common
    /// "let me hear them all" workflow.
    #[default]
    Sequential,
    /// Every channel announces its number at the same instant. Best when
    /// soloing / routing one channel at a time (each self-identifies
    /// whenever you listen to it alone); a cacophony on a downmix. This was
    /// the original behaviour before the sequential layout was added.
    Simultaneous,
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
    /// A/V sync test mode (a.k.a. "beep and flash"). When true, the tone is
    /// gated into a short burst on the first ~80 ms of every wallclock
    /// second (silence in between) and the video draws a luma flash patch
    /// next to the timecode on the same frames. A reviewer or scope sees
    /// the offset between the audible pip and the visible flash directly —
    /// the standard EBU R 49 / SMPTE 2-pop style sync test. Pairs with
    /// gate 3 (A/V sync drift, EBU R37 ±40 ms) in the broadcast-quality
    /// gates. Requires `audio_enabled = true`.
    #[serde(default)]
    pub av_sync_marker: bool,
    /// Optional ingress audio re-encode applied to the synthesised tone
    /// before the bytes reach the flow broadcast channel. Useful when a
    /// downstream consumer needs a different codec / sample-rate than
    /// the test-pattern's built-in AAC-LC stereo at 48 kHz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode applied between decode and
    /// `audio_encode`. Ignored when `audio_encode` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode applied to the synthesised
    /// colour-bars before fan-out. Useful for testing alternative
    /// encoder backends or codecs against a known-good signal — the
    /// built-in encoder picked by `select_video_backend()` still produces
    /// the bars, the replacer then transcodes to the requested target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Optional burned-in identifier drawn large near the top of the frame
    /// so multiple generators are distinguishable on a multiviewer.
    /// Rendered uppercase; characters outside A–Z / 0–9 / space / `-` /
    /// `.` / `:` are dropped. Capped at 32 chars. Empty / unset → no label
    /// (the timecode + bouncing box still prove liveness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    /// Number of audio channels to synthesise. AAC-native configs only:
    /// 1 (mono), 2 (stereo), 6 (5.1), 8 (7.1). Default 2. Ignored when
    /// `audio_enabled = false`.
    #[serde(default = "default_tp_audio_channels")]
    pub audio_channels: u8,
    /// What each audio channel carries — a shared tone (default) or a
    /// per-channel number announcement. See [`TestPatternAudioContent`].
    /// Overridden by the A/V-sync beep when `av_sync_marker = true`.
    #[serde(default)]
    pub audio_content: TestPatternAudioContent,
    /// A/V-sync marker style when `av_sync_marker = true`. See
    /// [`TestPatternAvSyncStyle`].
    #[serde(default)]
    pub av_sync_style: TestPatternAvSyncStyle,
    /// When `audio_content = channel_ident`, how the per-channel
    /// announcements are laid out in time. See
    /// [`TestPatternChannelIdentLayout`]. `sequential` (default) plays one
    /// channel per second so an operator can hear them one at a time even
    /// on a downmix; `simultaneous` fires every channel at once (better for
    /// solo monitoring). Ignored for `tone` content and when
    /// `av_sync_marker = true`.
    #[serde(default)]
    pub channel_ident_layout: TestPatternChannelIdentLayout,
    /// Maximum number of 188-byte MPEG-TS packets bundled into each output
    /// datagram (`RtpPacket`) this generator emits. Default 7 → 7 × 188 =
    /// 1316 B, the SRT payload size and the safe maximum for the public
    /// internet (it fits inside a 1500-byte MTU with IP/UDP headers, so
    /// routers don't fragment it). Lower it (e.g. 3–6) to test a
    /// constrained / low-MTU path; raise it (8+) to test jumbo datagrams on
    /// a LAN. Bounded 1..=348 (348 × 188 = 65 424 B, the largest that fits
    /// one UDP datagram). Independent of any downstream UDP/RTP/SRT output,
    /// which re-chunk to their own fixed 1316 B wire size — this knob
    /// governs the raw generator feed and the QUIC/UDP tunnel path, which
    /// forward each datagram unchanged. Before this field, the generator
    /// bundled a whole frame per datagram (~2 KB+), which fragmented or
    /// dropped over the internet.
    #[serde(default = "default_tp_ts_packets_per_datagram")]
    pub ts_packets_per_datagram: u16,
}

fn default_tp_width() -> u16 {
    1280
}
fn default_tp_audio_channels() -> u8 {
    2
}
fn default_tp_height() -> u16 {
    720
}
fn default_tp_fps() -> u16 {
    25
}
fn default_tp_video_bitrate() -> u32 {
    2000
}
fn default_tp_tone_hz() -> f32 {
    1000.0
}
fn default_tp_tone_dbfs() -> f32 {
    -20.0
}
fn default_tp_ts_packets_per_datagram() -> u16 {
    7
}

/// One asset inside a [`MediaPlayerInputConfig`]. The edge's media library
/// stores files by sanitised `name` under the configured media directory
/// (default `~/.bilbycast/media/`). The `kind` selects how the file is
/// turned into MPEG-TS at runtime:
///
/// * `ts`  — pre-encoded MPEG-TS file. Read raw, paced from PCR (or the
///   optional `paced_bitrate_bps` override), no decoder. Lowest CPU.
/// * `mp4` — MP4 / MOV / MKV container. Demuxed with the `mp4` crate,
///   H.264 NAL units converted from AVCC to Annex-B, AAC samples wrapped
///   in ADTS, all re-muxed via the shared [`crate::engine::rtmp::ts_mux::TsMuxer`].
/// * `image` — single still image (JPEG / PNG). Decoded once, fed to the
///   in-process H.264 encoder at low frame-rate, optionally paired with
///   silence — the standard "slate" pattern. Requires the same
///   `media-codecs` + `fdk-aac` features that [`TestPatternInputConfig`]
///   needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaPlayerSource {
    /// Pre-encoded MPEG-TS file in the edge's media library.
    Ts {
        /// Filename inside the media library directory (no path components).
        name: String,
        /// Down-select an MPTS file to a single program before publishing
        /// onto the flow. Unset (the default) → pass the file through
        /// unchanged: the whole MPTS reaches the flow, or the only program
        /// in an SPTS reaches the flow. Set to a program number → run each
        /// packet through `TsProgramFilter` so only the target program's
        /// PAT (rewritten single-program), PMT, PCR, and ES PIDs reach the
        /// flow. Must be `> 0` if set (program 0 is reserved for the NIT
        /// in the MPEG-TS spec).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program_number: Option<u16>,
    },
    /// MP4 / MOV / MKV container in the edge's media library.
    Mp4 {
        /// Filename inside the media library directory (no path components).
        name: String,
    },
    /// Still image (JPEG / PNG) in the edge's media library.
    Image {
        /// Filename inside the media library directory (no path components).
        name: String,
        /// Encoder frame rate for the still — keeps decoder TS state warm
        /// without burning CPU. Default 5.
        #[serde(default = "default_image_fps")]
        fps: u8,
        /// Encoder target bitrate in kbps. A still image compresses to
        /// nothing; default 250.
        #[serde(default = "default_image_bitrate_kbps")]
        bitrate_kbps: u32,
        /// When true, also encode silent AAC stereo (matches every other
        /// MediaPlayer source which carries audio). Default true.
        #[serde(default = "default_true")]
        audio_silence: bool,
    },
}

/// File-backed media player input. Plays one or more local assets as a
/// looping fresh MPEG-TS feed onto the flow's broadcast channel.
///
/// Combine with PID-bus Hitless (`FlowConfig.assembly.kind = spts | mpts`,
/// slot `source = hitless { primary, backup }`) for automatic failover when
/// the live primary stalls — the assembler's 200 ms stall threshold cuts
/// over to the media player without operator intervention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPlayerInputConfig {
    /// Ordered list of assets. A single entry is a single-file player;
    /// multiple entries form a sequential playlist. Empty = invalid.
    pub sources: Vec<MediaPlayerSource>,
    /// When true, restart at the head of `sources` after the last asset
    /// finishes. Default true (the common "fill while live is missing"
    /// shape).
    #[serde(default = "default_true")]
    pub loop_playback: bool,
    /// When true and `sources.len() > 1`, randomise the playback order
    /// each time the playlist starts. Default false.
    #[serde(default)]
    pub shuffle: bool,
    /// Override the natural pacing for `ts`-kind sources. When unset, the
    /// reader paces from embedded PCRs (the common case). When set, packets
    /// are paced at this fixed rate — useful for files that lack PCRs.
    /// Range 100 kbps … 200 Mbps; ignored for `mp4` and `image` kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paced_bitrate_bps: Option<u64>,
    /// Optional audio re-encode applied at ingress, before the playlist
    /// bytes reach the flow broadcast channel. Same plumbing as the live
    /// TS inputs — decoded ES, optional `transcode` reshape, re-encode
    /// to the configured codec, re-mux into the published TS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode applied between decode and
    /// `audio_encode`. Ignored when `audio_encode` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. Useful for normalising a
    /// playlist of mixed codecs / resolutions to a single output codec
    /// before fan-out — feature-gated like every other ingress
    /// `video_encode` block (`video-encoder-x264` / `-x265` / `-nvenc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
    /// Maximum number of 188-byte MPEG-TS packets bundled into each output
    /// datagram (`RtpPacket`) published onto the flow. Default 7 → 7 × 188 =
    /// 1316 B, the SRT payload size and the internet-safe MTU (it fits inside
    /// a 1500-byte MTU with IP/UDP headers, so routers don't fragment it).
    /// Lower it (3–6) for a constrained / low-MTU path; raise it (8+) only
    /// for a LAN / jumbo-frame link. Bounded 1..=348 (348 × 188 = 65 424 B,
    /// the largest that fits one UDP datagram). Applies to every source kind
    /// (ts / mp4 / image). Independent of downstream UDP/RTP/SRT outputs,
    /// which re-chunk to their own fixed 1316 B wire size — this knob governs
    /// the raw published feed and the QUIC/UDP tunnel path, which forward
    /// each datagram unchanged.
    #[serde(default = "default_mp_ts_packets_per_datagram")]
    pub ts_packets_per_datagram: u16,
}

fn default_image_fps() -> u8 {
    5
}
fn default_image_bitrate_kbps() -> u32 {
    250
}
fn default_mp_ts_packets_per_datagram() -> u16 {
    7
}

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
            InputConfig::MediaPlayer(_) => "media_player",
            InputConfig::Replay(_) => "replay",
            InputConfig::MxlVideo(_) => "mxl_video",
            InputConfig::MxlAudio(_) => "mxl_audio",
            InputConfig::MxlAnc(_) => "mxl_anc",
            InputConfig::Sdi(_) => "sdi",
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
            | InputConfig::TestPattern(_)
            // Media player synthesises fresh MPEG-TS from local files
            // (raw TS pass-through, MP4 demux + remux, or image + slate
            // encode).
            | InputConfig::MediaPlayer(_)
            // Replay reads MPEG-TS segments off the replay store and
            // pumps them onto the broadcast channel verbatim.
            | InputConfig::Replay(_) => true,
            // PCM-only inputs become TS carriers when `audio_encode` is set —
            // the input task muxes the encoded audio into an audio-only TS.
            // ST 2110-31 (AES3 transparent) becomes a TS carrier only via
            // `audio_encode.codec = "s302m"` — validation already rejects
            // any other codec, so checking `is_some()` is sufficient.
            InputConfig::St2110_30(c) => c.audio_encode.is_some(),
            InputConfig::RtpAudio(c) => c.audio_encode.is_some(),
            InputConfig::St2110_31(c) => c.audio_encode.is_some(),
            InputConfig::St2110_40(_) => false,
            // MXL video always encodes to TS at v1.0 (video_encode is required).
            InputConfig::MxlVideo(_) => true,
            // MXL audio becomes a TS carrier only when audio_encode is set
            // — mirrors the ST 2110-30 pattern.
            InputConfig::MxlAudio(c) => c.audio_encode.is_some(),
            // MXL ANC is RFC 8331 data, never TS.
            InputConfig::MxlAnc(_) => false,
            // SDI always encodes captured video (+ embedded audio) into a
            // single A+V MPEG-TS — video_encode is required.
            InputConfig::Sdi(_) => true,
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
    /// Optional public `host:port` reachable from outside this node's network
    /// (e.g. a port-forward on the firewall in front of this edge). Hint to
    /// the manager UI only — the edge itself binds `bind_addr` and ignores
    /// this field semantically. The manager's topology link matcher treats
    /// this as an alternate match candidate so cross-NAT lines draw correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
    /// Network interface IP for multicast join (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Optional source address for source-specific multicast (SSM, RFC 3678).
    /// When set on a multicast `bind_addr`, the kernel filters to only this
    /// source — skips PIM-RP, gives per-source filtering. Required by many
    /// ST 2110 / ST 2059 broadcast plants. Must be unicast and the same
    /// address family as `bind_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this leg to a physical NIC (loose source-IP bind by default,
    /// strict `SO_BINDTODEVICE` when `strict: true`). See
    /// [`InterfaceBinding`] for semantics. Wins over `interface_addr`
    /// when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Fixed ingress **delay** (0..=1000 ms). When set, packets are held in
    /// a per-input FIFO and released to the broadcast channel at exactly
    /// `recv_time_us + ingress_delay_ms`, so every downstream consumer
    /// (content analysis, thumbnail, PID bus, replay, outputs) sees the same
    /// constant-shifted timeline. This is a **pure delay line** — it shifts
    /// the timeline by a fixed amount but reproduces the inter-arrival
    /// spacing exactly, so it does **NOT** remove network jitter. Its purpose
    /// is deterministic alignment / cross-device sync (the input-side
    /// counterpart to the per-output `OutputDelay::Fixed`), applied before
    /// fan-out so all consumers share it. **To absorb network packet-delay
    /// variation, use [`Self::ingress_dejitter_ms`] instead** (a rate-paced
    /// servo that actually de-jitters; de-jitter supersedes this delay if
    /// both are set). `None` or `0` disables. Renamed from the former
    /// `ingress_smoothing_ms` (accepted as a serde alias for
    /// back-compat). See [`docs/configuration-guide.md`](configuration-guide.md).
    #[serde(
        default,
        alias = "ingress_smoothing_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingress_delay_ms: Option<u16>,
    /// Ingress **de-jitter** buffer setpoint, in ms of content. The
    /// ingress counterpart to the per-output `egress_buffer_ms` servo:
    /// when set, packets are buffered and released to the broadcast
    /// channel paced at the **recovered source rate** (a leaky bucket
    /// trimmed ±5 % by the buffer-fill error, with a hard residence-cap
    /// shed), so every downstream consumer sees a smooth cadence
    /// regardless of network packet-delay-variation. Unlike
    /// `ingress_delay_ms` (a pure delay line that *preserves* jitter)
    /// this actually removes it — it's the real broadcast-grade input
    /// de-jitter. `None` → env (`BILBYCAST_INGRESS_BUFFER_MS`) / 60 ms
    /// default; bounded 20–2000 ms. De-jitter supersedes smoothing if both
    /// are set. On a SMPTE 2022-7 dual-leg RTP input it runs *after* the
    /// hitless merger (re-pacing the merger's bursty seq-ordered drain).
    /// See [`crate::engine::ingress_dejitter`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_dejitter_ms: Option<u32>,
    /// Opt OUT of muxer-mode PCR + PES PTS regeneration. Default
    /// `false` (muxer mode ON) — bilbycast-edge regenerates PCR and
    /// PES PTS/DTS values from the per-flow master clock per the
    /// industry-standard remux model (Sencore RMX / Cobalt 9970-MX /
    /// Cisco D9036 mux mode). Set `true` to emit source PCR/PTS
    /// bytes unchanged (relay / transparent-forwarder mode — inherits
    /// source clock jitter and discontinuities at the receiver). See
    /// [`crate::engine::ts_pts_rewriter`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
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
    /// Optional public `host:port` reachable from outside this node's network.
    /// See [`RtpInputConfig::external_address`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
    /// Network interface IP for multicast join (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Source-specific multicast (SSM) source — see `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this input to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Fixed ingress delay (0..=1000 ms) — a pure delay line, NOT a jitter
    /// buffer (use `ingress_dejitter_ms` to de-jitter). See [`RtpInputConfig::ingress_delay_ms`].
    #[serde(
        default,
        alias = "ingress_smoothing_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingress_delay_ms: Option<u16>,
    /// Ingress de-jitter buffer setpoint (20–2000 ms of content). See
    /// [`RtpInputConfig::ingress_dejitter_ms`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_dejitter_ms: Option<u32>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrtInputConfig {
    /// SRT connection mode
    pub mode: SrtMode,
    /// Local bind address, e.g. "0.0.0.0:9000".
    /// Required for listener/rendezvous (the listen address). For **caller** this
    /// is the **source** socket (bind-then-connect), not the destination — optional,
    /// defaults to "0.0.0.0:0" (ephemeral). Never set a caller's `local_addr` equal
    /// to `remote_addr` (validation rejects the self-connect) or to a co-located
    /// egress tunnel's port (caught by the port-conflict preflight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Optional public `host:port` reachable from outside this node's network
    /// (e.g. a port-forward on the firewall in front of this edge). Only
    /// meaningful when `mode = listener`. Hint to the manager UI only — the
    /// edge itself binds `local_addr` and ignores this field semantically.
    /// The manager's topology link matcher treats this as an alternate match
    /// candidate so cross-NAT lines draw correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this SRT input to a physical NIC. Phase 1: loose only — strict
    /// mode is rejected (libsrt SRTO_BINDTODEVICE plumbing deferred). When
    /// set, the resolved interface IP is used as `local_addr` if
    /// `local_addr` is unset; otherwise the operator's `local_addr` wins.
    /// See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Fixed ingress delay (0..=1000 ms) — a pure delay line for alignment,
    /// NOT a jitter buffer. Layered on top of SRT's own `latency` jitter
    /// buffer it is usually pointless (SRT already re-times); logs a warning
    /// at startup when both are set. See
    /// [`RtpInputConfig::ingress_delay_ms`].
    #[serde(
        default,
        alias = "ingress_smoothing_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingress_delay_ms: Option<u16>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
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
    /// Optional public `host:port` reachable from outside this node's network.
    /// See [`RtpInputConfig::external_address`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Fixed ingress delay (0..=1000 ms) — a pure delay line, NOT a jitter
    /// buffer (use `ingress_dejitter_ms` to de-jitter). See
    /// [`RtpInputConfig::ingress_delay_ms`].
    #[serde(
        default,
        alias = "ingress_smoothing_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingress_delay_ms: Option<u16>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Fixed ingress delay (0..=1000 ms) — a pure delay line, NOT a jitter
    /// buffer (use `ingress_dejitter_ms` to de-jitter). See
    /// [`RtpInputConfig::ingress_delay_ms`].
    #[serde(
        default,
        alias = "ingress_smoothing_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingress_delay_ms: Option<u16>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
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
    /// Optional bind address for the WHIP UDP listener (e.g.
    /// `0.0.0.0:8000`). When unset the edge auto-binds to a random
    /// port (`0.0.0.0:0`, or `<public_ip>:0` when `public_ip` is set).
    /// Pin this when the publisher must reach a fixed port through a
    /// firewall / NAT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
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
    /// Accept self-signed TLS certificates on the WHEP endpoint
    /// (useful for testing against self-signed origin servers).
    /// Defaults to `true` to preserve historical behaviour. Set to
    /// `false` for production deployments that must validate the
    /// CA chain. When `cert_fingerprint` is set, that takes
    /// precedence and the CA chain IS validated regardless of this
    /// flag — only the leaf-cert fingerprint check overlays it.
    #[serde(default = "default_true_opt", skip_serializing_if = "Option::is_none")]
    pub accept_self_signed_cert: Option<bool>,
    /// SHA-256 fingerprint of the expected WHEP server certificate
    /// (colon-separated hex, e.g. `"ab:cd:..."`). When set, full
    /// CA-chain validation runs **and** the leaf-cert fingerprint
    /// must match. Defends against compromised CAs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,
    /// Optional ingress audio re-encode. See [`RtpInputConfig::audio_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// Optional planar PCM transcode. See [`RtpInputConfig::transcode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress video re-encode. See [`RtpInputConfig::video_encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
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
    /// packetizes onto the wire. Requires the `media-codecs` feature
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
    /// Decode the flow's video + audio and render to a local Linux display
    /// connector (HDMI / DisplayPort) plus an ALSA audio device.
    /// Linux-only at runtime (gated by Cargo feature `display`); the schema
    /// is always present so configs round-trip cleanly across platforms.
    #[serde(rename = "display")]
    Display(DisplayOutputConfig),
    /// Native SDI playout via Blackmagic DeckLink: decode the flow's video
    /// and schedule it against the card's clock. Video-only today; audio
    /// joins once the video path has soaked. Gated by the `sdi-decklink`
    /// Cargo feature; the schema is always present so configs round-trip
    /// cleanly across builds. See `docs/sdi.md`.
    #[serde(rename = "sdi")]
    Sdi(SdiOutputConfig),
    /// Produce a MXL **video** flow — decode the flow's H.264/HEVC TS,
    /// scale + convert to V210 4:2:2 10-bit planar, publish as grains
    /// onto the shared-memory bus. PTP-required. Gated by the `mxl`
    /// Cargo feature.
    #[serde(rename = "mxl_video")]
    MxlVideo(MxlVideoOutputConfig),
    /// Produce a MXL **audio** flow — Float32 PCM @ 48 kHz samples onto
    /// the shared-memory bus. PTP-required. Gated by the `mxl` Cargo
    /// feature.
    #[serde(rename = "mxl_audio")]
    MxlAudio(MxlAudioOutputConfig),
    /// Produce a MXL **ancillary** flow — RFC 8331 ANC grains onto the
    /// shared-memory bus. PTP-required. Gated by the `mxl` Cargo feature.
    #[serde(rename = "mxl_anc")]
    MxlAnc(MxlAncOutputConfig),
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
            OutputConfig::Display(c) => &c.id,
            OutputConfig::Sdi(c) => &c.id,
            OutputConfig::MxlVideo(c) => &c.id,
            OutputConfig::MxlAudio(c) => &c.id,
            OutputConfig::MxlAnc(c) => &c.id,
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
            OutputConfig::Display(c) => &c.name,
            OutputConfig::Sdi(c) => &c.name,
            OutputConfig::MxlVideo(c) => &c.name,
            OutputConfig::MxlAudio(c) => &c.name,
            OutputConfig::MxlAnc(c) => &c.name,
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
            OutputConfig::Display(_) => "display",
            OutputConfig::Sdi(_) => "sdi",
            OutputConfig::MxlVideo(_) => "mxl_video",
            OutputConfig::MxlAudio(_) => "mxl_audio",
            OutputConfig::MxlAnc(_) => "mxl_anc",
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
            OutputConfig::Display(c) => c.active,
            OutputConfig::Sdi(c) => c.active,
            OutputConfig::MxlVideo(c) => c.active,
            OutputConfig::MxlAudio(c) => c.active,
            OutputConfig::MxlAnc(c) => c.active,
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
            OutputConfig::Display(c) => c.active = active,
            OutputConfig::Sdi(c) => c.active = active,
            OutputConfig::MxlVideo(c) => c.active = active,
            OutputConfig::MxlAudio(c) => c.active = active,
            OutputConfig::MxlAnc(c) => c.active = active,
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
            OutputConfig::Display(c) => c.group.as_deref(),
            OutputConfig::Sdi(c) => c.group.as_deref(),
            OutputConfig::MxlVideo(c) => c.group.as_deref(),
            OutputConfig::MxlAudio(c) => c.group.as_deref(),
            OutputConfig::MxlAnc(c) => c.group.as_deref(),
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
    /// Enable per-output A/V alignment buffer. Buffers video and audio
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
    /// (`video-encoder-x264`, `video-encoder-x265`, `video-encoder-nvenc`,
    /// `video-encoder-qsv`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_encode: Option<VideoEncodeConfig>,

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Optional CBR padding to a target wire bitrate (kbps). See
    /// [`UdpOutputConfig::cbr_pad_to_kbps`] for semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cbr_pad_to_kbps: Option<u32>,
    /// Pin this output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Egress pacing model for this compressed RTP output. See
    /// [`EgressPacingMode`]. `None` → auto: `pcr` when the flow has a
    /// bonded input (re-smooths the reassembly burst cadence), else
    /// `forward` (emit at input cadence — the default every other
    /// output should keep unless it has a genuinely bursty unpaced
    /// ingress or a strict-T-STD receiver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_pacing: Option<EgressPacingMode>,
    /// Egress de-jitter buffer setpoint (ms of content). See
    /// [`UdpOutputConfig::egress_buffer_ms`] for full semantics. Only valid
    /// with `egress_pacing: "servo"` (validation rejects it otherwise);
    /// `None` → no cushion. Bounded 20–2000 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_buffer_ms: Option<u32>,
}

/// Egress pacing model for compressed (MPEG-TS) UDP/RTP outputs — how
/// `engine::wire_emit` times each datagram's release onto the wire.
///
/// An **unset** field (`None` on the config struct) means **auto**: the
/// engine resolves it per flow via [`EgressPacingMode::resolve_auto`] —
/// `pcr` when the flow has a bonded input (the bond reassembly buffer
/// delivers in hold-time bursts that nothing upstream has smoothed),
/// otherwise `forward` (the historical default, unchanged).
///
/// - **`forward`**: emit at the input's own cadence — no edge
///   re-pacing. The upstream (SRT/RIST TSBPD, `media_player` pacer, a clean
///   encoder) has already paced the stream and the receiver re-clocks from
///   PCR, so re-metering only adds latency and holds I-frame bursts
///   (measured: receiver-buffer underrun → periodic freezing on a clean
///   feed). Lowest latency; output PCR accuracy ≈ input PDV.
/// - **`pcr`**: open-loop PCR-delta re-pacing — emit each packet at its
///   PCR-implied wall instant. For 2022-7 dual-leg coherence, a
///   strict-T-STD receiver that can't ride input burst cadence, or a
///   bond-reassembled cadence.
/// - **`servo`**: closed-loop release-rate servo (leaky bucket at the
///   recovered source rate, ±5 % authority). For a genuinely bursty raw-UDP
///   ingress that nothing upstream has smoothed. Pair with
///   `egress_buffer_ms` to seed a real de-jitter cushion (adds that much
///   latency); without it the servo only rate-trims.
///
/// Protocol-paced outputs (SRT / RIST / RTMP / HLS / WebRTC) and ST 2110
/// raster pacing are unaffected — this knob exists only where the wire
/// emitter owns release timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressPacingMode {
    /// Emit at input cadence (no re-pacing). The auto-resolution default
    /// for non-bonded flows.
    #[default]
    Forward,
    /// Open-loop PCR-delta re-pacing.
    Pcr,
    /// Closed-loop release-rate servo (optionally cushioned via
    /// `egress_buffer_ms`).
    Servo,
}

impl EgressPacingMode {
    /// Resolve an unset (`auto`) per-output `egress_pacing` for a
    /// wire-emit (UDP/RTP-family) TS output. An explicit operator value
    /// always wins; absent resolves to `pcr` when the output's flow has
    /// at least one bonded input — the bond reassembly buffer releases
    /// recovered packets in hold-time bursts, so forwarding at input
    /// cadence would put that burst structure straight onto the wire —
    /// and to `forward` (the historical default) otherwise.
    pub fn resolve_auto(explicit: Option<Self>, flow_has_bonded_input: bool) -> Self {
        match explicit {
            Some(mode) => mode,
            None if flow_has_bonded_input => Self::Pcr,
            None => Self::Forward,
        }
    }

    /// Stable lowercase label, matching the serde wire form. Used by
    /// telemetry (`OutputStats.egress_pacing_effective`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Pcr => "pcr",
            Self::Servo => "servo",
        }
    }
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

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Optional CBR padding to a target wire bitrate (kbps). When set,
    /// `engine::ts_null_padder` injects PID 0x1FFF NULL packets between
    /// the transcoder pipeline and `wire_emit` so the wire rate matches
    /// the target regardless of natural encoder rate. Useful for
    /// downstream multiplexers / legacy receivers that expect a stable
    /// CBR feed. Bound by `MIN_TARGET_KBPS = 1000` and
    /// `MAX_TARGET_KBPS = 1_000_000`. Validation also enforces this is
    /// at least 5 % above the sum of declared `audio_encode.bitrate_kbps`
    /// + `video_encode.bitrate_kbps` so there's headroom to pad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cbr_pad_to_kbps: Option<u32>,
    /// Pin this UDP output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Egress pacing model for this compressed UDP output. See
    /// [`EgressPacingMode`]. `None` → auto: `pcr` when the flow has a
    /// bonded input (re-smooths the reassembly burst cadence), else
    /// `forward` (emit at input cadence — the default every other
    /// output should keep unless it has a genuinely bursty unpaced
    /// ingress or a strict-T-STD receiver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_pacing: Option<EgressPacingMode>,
    /// Egress de-jitter buffer setpoint, in milliseconds of content. Seeds
    /// the wire-emit release-rate servo's cushion for this compressed output:
    /// the servo holds the internal queue centred on this fill, absorbing a
    /// source-rate-vs-wallclock offset (or burst) as a ±5 % rate trim instead
    /// of letting it integrate into latency, and a hard residence cap (≈4×
    /// this, min 1000 ms) sheds the backlog if a burst exceeds the servo's
    /// authority. Only valid with `egress_pacing: "servo"` (validation
    /// rejects it otherwise); `None` → no cushion. Bounded 20–2000 ms. Has
    /// no effect on protocol-paced outputs (the wire emitter only runs on
    /// UDP/RTP). See `docs/egress-dejitter-design.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_buffer_ms: Option<u32>,
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
    /// Required for listener/rendezvous (the listen address). For **caller** this
    /// is the **source** socket (bind-then-connect), not the destination — optional,
    /// defaults to "0.0.0.0:0" (ephemeral). Never set a caller's `local_addr` equal
    /// to `remote_addr` (validation rejects the self-connect) or to a co-located
    /// egress tunnel's port (caught by the port-conflict preflight).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Optional public `host:port` reachable from outside this node's network.
    /// Only meaningful when `mode = listener`. See [`SrtInputConfig::external_address`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
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

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Optional CBR padding to a target wire bitrate (kbps). See
    /// [`UdpOutputConfig::cbr_pad_to_kbps`] for semantics. SRT carries
    /// the padded TS opaquely — receivers measuring CBR rate see the
    /// inflated stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cbr_pad_to_kbps: Option<u32>,
    /// Force a browser-safe ("WebRTC-compatible") H.264 egress. The report
    /// scenario is "route this SRT feed to a WebRTC audience" — an external
    /// WebRTC gateway (mediamtx / Janus) ingesting the SRT feed rejects
    /// H.264 with B-frames. When true, the edge **always** re-encodes video
    /// to browser-safe H.264 (zero B-frames, 8-bit 4:2:0, browser-decodable
    /// profile, inline SPS/PPS) via [`webrtc_safe_video_encode`], regardless
    /// of the source's GOP structure. Requires an H.264 encoder backend
    /// compiled in (validation checks). See also
    /// [`WebrtcOutputConfig::webrtc_compatible`] for the native WHIP/WHEP path.
    #[serde(default)]
    pub webrtc_compatible: bool,
    /// Pin this SRT output to a physical NIC. Phase 1: loose only —
    /// strict mode is rejected (libsrt SRTO_BINDTODEVICE plumbing
    /// deferred). When set, the resolved interface IP is used as
    /// `local_addr` if `local_addr` is unset. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Pin SRT leg 2 to a physical NIC (Phase 1: loose only). Lets
    /// 2022-7-style SRT dual-leg pin Red and Blue to different NICs.
    /// See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Pin this bonding endpoint to a physical NIC (Phase 1: loose only).
    /// When set, the resolved interface IP is used as `local_addr` if
    /// `local_addr` is unset. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Source-specific multicast (SSM) source for leg 2. Real Red/Blue plants
    /// often have different source IPs per network — see
    /// `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
    /// Path-differential / skew-accommodation buffer in milliseconds.
    /// `None` keeps the legacy stateless dedup-only path (lower
    /// latency but no protection against asymmetric path delay). When
    /// set, the merger holds every packet for this many ms after first
    /// arrival before releasing it in seq order — the slower leg has
    /// the full window to deliver, and a single-leg loss within the
    /// window is filled hitlessly. Range: 5..=2000 ms. Industry
    /// typical: 30–80 ms terrestrial WAN, 200–500 ms satellite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_differential_ms: Option<u32>,
    /// Pin RTP leg 2 to a physical NIC. Lets 2022-7 hitless plants pin
    /// Red and Blue to different NICs. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Pin RTP output leg 2 to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
}

/// RIST Simple Profile (TR-06-1:2020) input. Binds a dual-port UDP channel
/// (RTP on even port P, RTCP on P+1) and decodes the reliable-RTP stream
/// delivered by a remote `ristsender` or equivalent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistInputConfig {
    /// Local bind address, e.g. "0.0.0.0:6000". The port **must be even** —
    /// RIST binds RTCP on port+1.
    pub bind_addr: String,
    /// Optional public `host:port` reachable from outside this node's network.
    /// See [`RtpInputConfig::external_address`]. Port must be even (RIST binds
    /// RTCP on port+1) but this is the operator's responsibility — the edge
    /// does not consume the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
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

    /// Optional ingress program filter for MPTS sources. When set, the
    /// input drops every TS packet that doesn't belong to this program
    /// before publishing onto the broadcast channel. Other programs in
    /// the source MPTS are discarded at ingress, freeing broadcast-channel
    /// bandwidth but preventing other outputs on the same flow from
    /// accessing the dropped programs. Unset = full MPTS passes through.
    /// Must be > 0; `program_number` 0 is reserved for the NIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Optional mechanical PID remap table applied at ingress (after the
    /// program filter, before any role-keyed override or transcode).
    /// Keys + values must sit in `0x0010..=0x1FFE`; source PIDs not
    /// present pass through. Symmetric with the output-side `pid_map`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_map_serde"
    )]
    pub pid_map: Option<std::collections::BTreeMap<u16, u16>>,
    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this RIST input to a physical NIC (Phase 1: loose only —
    /// strict deferred pending librist plumbing). See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// Muxer-mode PCR + PES PTS regeneration opt-out. See
    /// [`RtpInputConfig::passthrough_clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_clock: Option<bool>,
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

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this RIST output to a physical NIC (Phase 1: loose only —
    /// strict deferred pending librist plumbing). See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
}

/// SMPTE 2022-7 redundancy config for a RIST input (leg 2).
/// The primary `bind_addr` in the parent RistInputConfig is leg 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistInputRedundancyConfig {
    /// Bind address for leg 2. Port must be even.
    pub bind_addr: String,
    /// Optional public `host:port` reachable from outside this node's network
    /// for leg 2. See [`RistInputConfig::external_address`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_address: Option<String>,
    /// Pin RIST leg 2 to a physical NIC (Phase 1: loose only).
    /// See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
}

/// SMPTE 2022-7 redundancy config for a RIST output (leg 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RistOutputRedundancyConfig {
    /// Remote address for leg 2. Port must be even.
    pub remote_addr: String,
    /// Local bind address for leg 2 (optional). When set, port must be even.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_addr: Option<String>,
    /// Pin RIST output leg 2 to a physical NIC (Phase 1: loose only).
    /// See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Reassembly hold time in milliseconds. Default 500 ms. When
    /// `hold_max_ms` is set, this is the floor the adaptive servo grows
    /// up from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
    /// Optional adaptive hold-time **ceiling** in milliseconds. When set
    /// above `hold_ms`, the receiver grows the reorder/recovery budget
    /// toward the realized recovery latency (×1.5) within
    /// `[hold_ms, hold_max_ms]` and decays back as the links calm —
    /// latency tracks the links instead of a fixed guess. Unset = fixed
    /// `hold_ms` (default behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_max_ms: Option<u32>,
    /// Base NACK delay in milliseconds. Default 30 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nack_delay_ms: Option<u32>,
    /// Max NACK retries per gap. Default 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nack_retries: Option<u32>,
    /// Keepalive interval in milliseconds. Default 200 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_ms: Option<u32>,
    /// Optional 64-hex-char (32-byte) AEAD key — must match the bonded
    /// output's key for the encrypted legs to open. See
    /// [`BondedOutputConfig::encryption_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key: Option<String>,
    /// Optional proactive FEC — must match the bonded output's geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec: Option<BondFecConfig>,
    /// Optional ingress de-jitter depth in milliseconds applied to the
    /// reassembled bond stream before it reaches the flow's outputs
    /// (HDMI display, UDP/RTP egress). The bond reassembler releases
    /// in-order but in HOL-gated bursts (a straggler on a slow leg stalls
    /// the head, then a clump flushes); without smoothing that burstiness
    /// is forwarded verbatim and shows up as display jerk / egress PCR
    /// jitter. Mirrors the UDP/RIST inputs' `ingress_dejitter_ms`. Unset /
    /// 0 = pass through unchanged (default). Typical 150–250 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_dejitter_ms: Option<u32>,
    /// Per-leg latency/jitter **equalization** mode on this bonded input (see
    /// `bilbycast-bonding/docs/per-leg-equalization.md`). The receiver measures
    /// each leg's relative one-way delay (from the sender's v2 header
    /// timestamps) and, in `auto`/`on`, time-aligns the legs so heterogeneous
    /// high-latency/jitter legs (5G + Starlink + ISP) AGGREGATE their bandwidth
    /// in-order instead of head-of-line-blocking. See [`BondEqualizationMode`].
    /// Defaults to `auto` (self-configuring). The latency budget is
    /// [`Self::max_bonding_latency_ms`].
    #[serde(
        default,
        deserialize_with = "de_bond_equalization_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub equalization: Option<BondEqualizationMode>,
    /// Equalization latency budget in milliseconds — the ceiling for how far a
    /// fast leg is held to align a slow one, and the loss-recovery deadline
    /// while aligned. The single bonding-latency knob; should match the bonded
    /// output's value. When unset, derived from `hold_max_ms`, else defaults to
    /// 1000. Kept distinct from `hold_ms` (the jitter floor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bonding_latency_ms: Option<u32>,
}

/// Per-leg equalization mode for a bonded input/output. Self-configuring by
/// default. Deserializes from `"auto"` / `"off"` / `"on"`, and — for
/// backward-compatibility with configs predating the tri-state — from the
/// legacy boolean (`true` → `auto`, `false` → `off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BondEqualizationMode {
    /// Stamp + measure always; engage alignment only when the measured
    /// inter-leg skew is worth the latency, within budget, and the sender isn't
    /// ride-fastest. The self-configuring aggregation default.
    #[default]
    Auto,
    /// Never stamp/measure/align — legacy aggregate-but-never-align (the
    /// latency-critical escape hatch: a slow leg's reorder is ARQ/FEC-recovered
    /// rather than absorbed by alignment latency).
    Off,
    /// Force-engage alignment whenever a leg is warm, overriding the
    /// ride-fastest (duplicate-all) suppression.
    On,
}

/// Deserialize [`BondEqualizationMode`] accepting the tri-state string or the
/// legacy boolean (`true` → `auto`, `false` → `off`). Absent → `None` (the
/// mapping then applies the `auto` default).
fn de_bond_equalization_mode<'de, D>(d: D) -> Result<Option<BondEqualizationMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bool(bool),
        Mode(BondEqualizationMode),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None => None,
        Some(Raw::Bool(true)) => Some(BondEqualizationMode::Auto),
        Some(Raw::Bool(false)) => Some(BondEqualizationMode::Off),
        Some(Raw::Mode(m)) => Some(m),
    })
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
    /// Optional congestion-control tuning for the `adaptive` scheduler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub congestion: Option<BondCongestionConfig>,
    /// Optional proactive FEC (off by default). Recovers sparse loss
    /// without a NACK round-trip; the bonded input must use the same
    /// geometry. Costs `1/rows` bandwidth overhead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec: Option<BondFecConfig>,
    /// Optional packet **redundancy** (off by default). Replicates packets
    /// across the N best legs so a copy survives as long as any one leg
    /// delivers it — the strongest loss-resilience, at N× the bandwidth for
    /// the replicated traffic. Sender-side only (the bonded input dedups
    /// duplicates). See [`BondRedundancyConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<BondRedundancyConfig>,
    /// Optional 64-hex-char (32-byte) AEAD key. When set, every UDP/RIST
    /// leg of this bond is ChaCha20-Poly1305 encrypted; the matching
    /// bonded input must carry the same key. QUIC legs are already TLS.
    /// Sensitive but operator-managed (mirrors SRT `passphrase`), so it
    /// lives in `config.json` and is visible in the manager UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key: Option<String>,
    /// Sender retransmit buffer capacity in packets. Default 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit_capacity: Option<usize>,
    /// Per-leg latency/jitter **equalization** mode (see
    /// `bilbycast-bonding/docs/per-leg-equalization.md`). In `auto`/`on` the
    /// sender stamps each data packet with a v2 (16-byte) header timestamp so
    /// the receiver can measure each leg's relative one-way delay and
    /// time-align the legs into one aggregated in-order stream. See
    /// [`BondEqualizationMode`]. Defaults to `auto` (self-configuring). The
    /// matching bonded input should use the same mode + budget.
    #[serde(
        default,
        deserialize_with = "de_bond_equalization_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub equalization: Option<BondEqualizationMode>,
    /// Equalization latency budget in milliseconds — the single bonding-latency
    /// knob (default 1000). A leg whose relative one-way delay would exceed this
    /// is benched from carrying unique media (still used for redundancy/FEC)
    /// rather than aligning the whole flow to it; it is also the receiver's
    /// alignment ceiling + loss-recovery deadline. Should match the bonded
    /// input's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bonding_latency_ms: Option<u32>,
    /// Keepalive interval in milliseconds. Default 200 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_ms: Option<u32>,
    /// Smallest IP-layer path MTU (bytes) across this bond's legs. The
    /// output re-chunks outbound TS payloads at 188-byte packet boundaries
    /// into datagrams that fit this MTU after every per-datagram overhead
    /// (IP/UDP, bond header, AEAD envelope, relay tunnel framing), so no
    /// leg emits an IP-fragmented datagram — cellular CGNAT paths routinely
    /// drop fragments and black-hole PMTU discovery, so a fragmented
    /// datagram is lost wholesale. Measure the constrained leg with a DF
    /// ping sweep (`ping -M do -s <n>`). Default 1500 (standard ethernet),
    /// which derives the classic 1316-byte (7 × 188) TS datagram; lower it
    /// to the measured value for cellular / satellite legs (e.g. ~1000 on
    /// some carrier-NAT bearers → 4 × 188 = 752 B datagrams). Valid
    /// [576, 9000]. Sender-side only — the bonded input needs no change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_mtu: Option<u32>,
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
    /// QoS priority tier for the shared-leg capacity broker (default
    /// `best_effort`). When several bonded flows share one physical uplink,
    /// the guaranteed tiers (`critical`, then `normal`) reserve their live
    /// (VBR-tracking) demand ahead of best-effort flows. No bandwidth number
    /// required — the broker measures demand. See [`BondPriority`] /
    /// `engine::bond_leg_broker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<BondPriority>,
}

/// A single bond leg definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BondPathConfig {
    /// Path identifier — must be unique within the paths array.
    pub id: u8,
    /// Operator-visible name (e.g. `"lte-0"`, `"starlink"`).
    pub name: String,
    /// Scheduler weight hint; higher = more traffic at steady state.
    /// With the `adaptive` scheduler this seeds the path's initial
    /// capacity estimate (a relative prior) so a known-fat link starts
    /// carrying its share sooner.
    #[serde(default = "default_bond_weight")]
    pub weight_hint: u32,
    /// Optional **hard upper bound** on this leg's send rate, bits/sec.
    /// The adaptive scheduler never drives the link above this even if
    /// it could carry more — use it to cap a metered cellular modem for
    /// cost control. Unset = auto-discover with no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_bps: Option<u64>,
    /// Optional **per-leg** FEC for this path (interleaved XOR). When set,
    /// this leg runs its own FEC over only the packets it carries, so a leg
    /// burst (e.g. a Starlink satellite handoff) is recovered locally
    /// instead of clustering in the combined stream and overrunning a shared
    /// column. Mutually exclusive with the bond-level `fec` (combined): a
    /// bond uses one model or the other. The matching end (input ↔ output)
    /// must list the same geometry for this leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec: Option<BondFecConfig>,
    /// Transport flavour.
    pub transport: BondPathTransportConfig,
}

fn default_bond_weight() -> u32 {
    1
}

/// Congestion-control tuning for the `adaptive` bonded-output scheduler.
/// Every field is optional; unset falls back to the broadcast-tuned
/// library default. Exposed so an operator can adapt the controller to
/// an unusual link mix without an edge release.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct BondCongestionConfig {
    /// Floor each link's capacity estimate never drops below, kbps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rate_kbps: Option<u32>,
    /// Starting capacity estimate (before measurement) for a unit-weight
    /// link, kbps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_rate_kbps: Option<u32>,
    /// Loss percent below which a link is "clean" and probes up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_low_pct: Option<f32>,
    /// Loss percent at/above which a link backs off hard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_high_pct: Option<f32>,
    /// RTT inflation over a link's own minimum treated as queue-building
    /// congestion (delay-based signal), milliseconds. When
    /// `delay_inflation_auto` is set this is the floor of the
    /// auto-derived per-link threshold rather than a fixed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_inflation_ms: Option<u32>,
    /// Auto-derive the queue-building delay threshold per link from its
    /// own measured baseline RTT instead of using a fixed
    /// `delay_inflation_ms`. A bufferbloated cellular link (high
    /// baseline) gets a proportionally looser threshold automatically —
    /// removing the per-link hand-tuning such links otherwise need —
    /// while a terrestrial link keeps the tight `delay_inflation_ms`
    /// floor. Opt-in; default off (fixed threshold).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_inflation_auto: Option<bool>,
    /// Token-bucket burst depth, milliseconds of capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_ms: Option<u32>,
    /// Evidence bound on the probed capacity: once a delivered rate has
    /// been measured, a link's estimate never exceeds
    /// `delivered × probe_cap_mult` — bounds undersubscribed-bond
    /// inflation while still letting the estimate grow after a failover.
    /// Default 2.0. Valid [1.1, 10.0].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_cap_mult: Option<f64>,
    /// Window over which each link's minimum RTT baseline is tracked
    /// (BBR-style), milliseconds — a route change that shifts the RTT
    /// floor ages out of the window instead of reading as permanent
    /// congestion. Default 10000. Valid [1000, 120000].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_min_window_ms: Option<u64>,
    /// Smoothed interarrival jitter (milliseconds) above which a link is
    /// demoted from carrying UNIQUE media — it keeps carrying redundancy /
    /// FEC copies (late copies are harmless insurance) but can no longer
    /// head-of-line-block the in-order reassembly of the whole flow. A
    /// catastrophically jittery link (a bufferbloated retail modem at
    /// 300 ms+) otherwise stalls delivery for every leg. Re-admits when its
    /// jitter recovers. `0` disables demotion (jitter still soft-deweights
    /// the split). Default 150 ms. Valid [0, 5000].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_demote_ms: Option<u32>,
}

/// FEC algorithm for a bonded link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BondFecAlgorithm {
    /// Interleaved XOR (SMPTE 2022-1 column model) — recovers one loss per
    /// column. The only algorithm for the bond-wide (combined) FEC.
    #[default]
    Xor,
    /// Reed-Solomon — recovers up to `parity` losses per `data+parity`
    /// block. **Per-leg only** (not valid for the combined `fec`).
    ReedSolomon,
}

/// Proactive FEC geometry for a bonded link. Off by default; both ends must
/// use the same geometry. For **XOR** (default), `columns` is the interleave
/// depth (burst tolerance) and `rows` is packets per column (overhead
/// `1/rows`). For **Reed-Solomon** (per-leg only), `columns` = data shards
/// (k) and `rows` = parity shards (m): recovers up to `m` losses per `k+m`
/// block at `m/k` overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondFecConfig {
    pub columns: u16,
    pub rows: u16,
    /// FEC algorithm. Absent = `xor` (back-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<BondFecAlgorithm>,
    /// Reed-Solomon **adaptive** parity ceiling. When set above `rows`
    /// (= the parity floor), the leg's RS parity scales between
    /// `[rows, parity_max]` with its measured loss. Ignored for XOR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity_max: Option<u16>,
}

/// Packet-redundancy mode for a bonded output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BondRedundancyMode {
    /// No extra replication (only Critical/IDR keyframes still duplicate).
    #[default]
    Off,
    /// Replicate every packet across the N best legs (N× bandwidth).
    All,
    /// Replicate only packets at/above `min_priority` across the N best legs.
    Threshold,
}

/// Priority threshold for `threshold`-mode redundancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondRedundancyPriority {
    Normal,
    High,
    Critical,
}

/// Optional packet redundancy on a bonded output — replicate packets across
/// several legs for max loss-resilience over flaky links. Off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondRedundancyConfig {
    #[serde(default)]
    pub mode: BondRedundancyMode,
    /// For `threshold` mode: replicate packets at/above this priority.
    /// Defaults to `high` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_priority: Option<BondRedundancyPriority>,
    /// Number of legs to replicate across (2–8). Default 2. Clamped at
    /// runtime to the live leg count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u8>,
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
        ///
        /// This is **interface-mode** path selection. For the
        /// dumb-switch / single-NIC topology use `gateway` instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<String>,
        /// **Gateway-mode** path selection (sender side only). The
        /// router / next-hop this leg egresses through. The edge
        /// programs a dedicated policy route (`ip rule from <source>
        /// → table → default via <gateway>`) via netlink, so several
        /// legs on one NIC each go out their own router. Requires
        /// `source`. Mutually exclusive with `interface`.
        /// See `docs/bonding-gateway-routing.md`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway: Option<String>,
        /// Source address (`ip/prefix`, e.g. `192.168.10.2/24`) this
        /// leg binds to, inside the `gateway`'s subnet. The edge
        /// ensures the address is present on the NIC and keys the
        /// policy rule on it. Required when `gateway` is set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// **Relayed** leg — this bond leg rides a native plain-UDP relay tunnel
    /// **in-process** (no `127.0.0.1` loopback hop). The edge bridge owns the
    /// relay socket: the same Register/keepalive rendezvous + failover the
    /// native SRT/RIST relay tunnel uses, plus the 16-byte `tunnel_id` prefix —
    /// so `bilbycast-relay` needs **zero** changes (it still demuxes by prefix
    /// and forwards verbatim). The leg's direction is auto-derived from the
    /// side: a bonded **output** leg registers `egress`, a bonded **input** leg
    /// `ingress` (like the QUIC leg's client/server).
    ///
    /// **Fail-closed encryption** (enforced by validation): a relay leg must
    /// carry **exactly one** encryption layer — the bond's own
    /// [`BondedOutputConfig::encryption_key`] / [`BondedInputConfig::encryption_key`]
    /// (`0xBD` AEAD, sealed inside the bond) **or** this leg's
    /// `tunnel_encryption_key` (the tunnel AEAD the bridge applies under the
    /// prefix). Neither = silent blackhole; both = wasteful double-encrypt.
    Relay {
        /// Relay tunnel id (UUID) — stamped by the manager FK; both ends share
        /// it so the relay pairs the ingress + egress halves.
        tunnel_id: String,
        /// Relay address(es) `host:port`. ≥1 required; extra entries are a
        /// failover list (the bridge rotates on a dead relay).
        relay_addrs: Vec<String>,
        /// Optional 64-hex (32-byte) relay bind secret → HMAC-SHA256 bind token
        /// in the `Register` (same auth the native SRT/RIST relay tunnel uses).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tunnel_bind_secret: Option<String>,
        /// Optional 64-hex (32-byte) per-leg tunnel AEAD key. Present **iff**
        /// the bond is unkeyed (fail-closed — see the variant docs). When set,
        /// the bridge ChaCha20-Poly1305-seals each datagram under the prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tunnel_encryption_key: Option<String>,
        /// Optional NIC pin (`"wwan0"`, `"eno4"`) for the bridge's outbound
        /// relay socket — same `SO_BINDTODEVICE` → `IP_UNICAST_IF` mechanism as
        /// the UDP leg.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<String>,
        /// Optional source address (`ip` or `ip/prefix`) the bridge socket
        /// binds to. Pins the egress source IP; in gateway mode it also keys
        /// the policy rule.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// Optional gateway-mode next-hop. Requires `source` + `interface`; the
        /// bridge programs a `from <source>` policy route via the gateway so
        /// this leg egresses a specific uplink on a shared NIC.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway: Option<String>,
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
        /// QUIC role. **Auto-derived from the side** — a bonded *output*
        /// (sender) leg is always the `client`, a bonded *input* (receiver)
        /// leg is always the `server`. Optional in config (defaults to
        /// `client`) and any explicit value is overridden by the side at
        /// build time, so operators never need to set it. Kept as a field
        /// only for back-compat with configs that still carry it.
        #[serde(default)]
        role: BondQuicRole,
        /// Client: remote `host:port`. Server: local bind.
        addr: String,
        /// Client: server name for SNI/ALPN. Ignored on server role.
        #[serde(default)]
        server_name: String,
        tls: BondQuicTls,
        /// Client-only local source bind `ip:port` (port usually 0) to
        /// pin egress on a multi-homed sender. Without it the QUIC
        /// client binds `0.0.0.0:0` and every leg collapses onto the
        /// kernel default route (cosmetic bond). Set to a source IP
        /// that policy-routes out the intended uplink. Ignored on the
        /// server role (it binds `addr`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<String>,
        /// NIC pin (e.g. `"eno4"`, `"wwan0"`) — same `SO_BINDTODEVICE`
        /// → `IP_UNICAST_IF` mechanism as the UDP leg. Applies to both
        /// roles. `None` leaves egress to the routing table / `bind`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondRistRole {
    Sender,
    Receiver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BondQuicRole {
    /// Default — overridden by the leg's side at build time (output → client,
    /// input → server). The value is never authoritative.
    #[default]
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

/// QoS priority tier for a bonded flow, used by the shared-leg capacity broker
/// when several bonded flows share one physical uplink. Guaranteed tiers
/// (`Critical`, then `Normal`) reserve their live, VBR-tracking demand first —
/// in that order — and `BestEffort` flows share whatever is left. Strict
/// priority *between* tiers; weighted-fair *within* a tier. No bandwidth number
/// required: the broker measures each flow's demand. See
/// `engine::bond_leg_broker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BondPriority {
    /// Highest — reserved before everything else (the main programme feed).
    Critical,
    /// Guaranteed, but yields to `Critical` under contention.
    Normal,
    /// Spare-only — takes whatever capacity the guaranteed flows leave.
    #[default]
    BestEffort,
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
    /// non-IDR frames. Legacy — superseded by `adaptive`.
    MediaAware,
    /// **Capacity-aware congestion-controlled** scheduler plus the same
    /// NAL-type IDR detection as `media_aware`. Each leg discovers its
    /// usable bandwidth from delivered-rate / loss / RTT-inflation
    /// feedback and is filled to (but not past) that capacity; the split
    /// is therefore proportional to *measured* capacity with smooth
    /// quality deweighting, and a saturated link spills to one with
    /// headroom. This is the right policy for a heterogeneous cellular +
    /// satellite contribution bond. **Default.**
    #[default]
    Adaptive,
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

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
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

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
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
/// RFC 6184 RTP. Audio is delivered to browser viewers as Opus — the codec
/// WebRTC requires.
///
/// # Audio
/// - WebRTC always emits **Opus**. Source audio is decoded and re-encoded to
///   Opus in-process on the default build (`webrtc` + `media-codecs` /
///   `fdk-aac`): fdk-aac decodes AAC-LC / HE-AAC, and libavcodec/libopus
///   decodes MP2 / AC-3 / E-AC-3 and performs the Opus encode. A ffmpeg
///   subprocess is used only as a fallback when `media-codecs` is disabled.
///   So an AAC-audio source is carried, not omitted — it is transcoded to
///   Opus (see `engine::output_webrtc`).
/// - Raw **Opus-in-TS passthrough** (a source already carrying Opus, i.e.
///   stream_type 0x06 + `Opus` registration descriptor) is not yet wired
///   through the str0m audio path: those frames are currently dropped with a
///   one-shot warning rather than passed through.
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
    /// Accept self-signed TLS certificates on the WHIP endpoint
    /// (`whip_client` mode only — `whep_server` is the receiving
    /// side and does not dial TLS). Defaults to `true` to preserve
    /// historical behaviour for customer testing setups. Set to
    /// `false` for production deployments that must validate the
    /// CA chain. When `cert_fingerprint` is set, that takes
    /// precedence and the CA chain IS validated regardless.
    #[serde(default = "default_true_opt", skip_serializing_if = "Option::is_none")]
    pub accept_self_signed_cert: Option<bool>,
    /// SHA-256 fingerprint of the expected WHIP server certificate
    /// (colon-separated hex). When set, full CA-chain validation
    /// runs **and** the leaf-cert fingerprint must match. Defends
    /// against compromised CAs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,
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
    /// Force a browser-safe ("WebRTC-compatible") H.264 egress. When true,
    /// the edge **always** re-encodes video to a stream a WebRTC audience
    /// (browser / WHEP SFU / mediamtx / Janus) can decode: H.264, **zero
    /// B-frames** (the crux — B-frames force non-monotonic RTP timestamps
    /// that browser jitter buffers freeze on), 8-bit 4:2:0, a
    /// browser-decodable profile, and inline SPS/PPS on every IDR. Convenience
    /// wrapper: it synthesises a browser-safe `video_encode` when none is set,
    /// or pins an operator-supplied one to safe settings (see
    /// [`webrtc_safe_video_encode`]). An operator wanting to reach a WebRTC
    /// audience sets this instead of hand-tuning NAL / profile internals.
    /// Requires an H.264 encoder backend compiled in (validation checks).
    #[serde(default)]
    pub webrtc_compatible: bool,

    /// Optional MPEG-TS PID overrides for the transcoded output. When set,
    /// the encoded ES is emitted on the override PID and the rewritten PMT
    /// entry advertises the override; pinning every transcoded source to
    /// the same wire layout lets a downstream decoder stay locked across
    /// switcher hops between transcoded inputs/outputs. See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for the per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
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
/// - `video-encoder-nvenc` → `h264_nvenc` / `hevc_nvenc` (NVIDIA iGPU/dGPU).
/// - `video-encoder-qsv` → `h264_qsv` / `hevc_qsv` (Intel iGPU/Arc dGPU,
///   x86_64 only, via Intel oneVPL).
/// - `video-encoder-vaapi` → `h264_vaapi` / `hevc_vaapi` (AMD Mesa
///   radeonsi or Intel iHD via libva, Linux only).
/// - `video-encoder-rkmpp` → `h264_rkmpp` / `hevc_rkmpp` (ARM Rockchip
///   RK3568 / RK3588 VPU; 8-bit 4:2:0 only).
///
/// Plus the per-host auto strings `h264_auto` / `hevc_auto`, which resolve
/// to the cheapest backend the host can actually run at flow start.
///
/// Validation in `config::validation` enforces field bounds at config
/// load time. Unavailable backends are surfaced as a runtime error when
/// the output starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoEncodeConfig {
    /// Encoder backend. One of: `x264`, `x265`, `h264_nvenc`, `hevc_nvenc`,
    /// `h264_qsv`, `hevc_qsv`, `h264_vaapi`, `hevc_vaapi`, `h264_rkmpp`,
    /// `hevc_rkmpp`, `h264_auto`, `hevc_auto`.
    pub codec: String,
    /// Optional source video PID to transcode. When unset (default), the
    /// replacer locks onto the **first** video stream in the active
    /// program's PMT whose stream_type is one of `0x01` (MPEG-1),
    /// `0x02` (MPEG-2), `0x1B` (H.264), `0x24` (H.265). When set, the
    /// replacer locks onto the named PID specifically — useful for MPTS
    /// programs with multiple video streams or when the operator wants
    /// to pin to a specific PID across input swaps. PID range
    /// `0x0010..=0x1FFE`. If the named PID is absent from the live PMT
    /// the replacer falls back to the first-match behaviour and emits a
    /// `tracing::warn!` with `error_code = video_source_pid_not_found`.
    ///
    /// **Multi-program scope**: the in-place transcoder is single-
    /// program — only one video PID is transcoded per output. To
    /// transcode multiple videos from an MPTS, create one output per
    /// program (each with its own `program_number` filter +
    /// `source_video_pid` pin). The validator rejects multi-program
    /// `pid_overrides` + `audio_encode`/`video_encode` on the same
    /// output with a clear error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_video_pid: Option<u16>,
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
    /// Hardware-decoder preference for the input decode side of the
    /// transcode pipeline. `None` (or `Auto`) lets the edge pick the
    /// best HW backend the host has compiled in and probed. `Cpu`
    /// forces software libavcodec decode. `Nvdec` / `Qsv` / `Vaapi`
    /// force a specific HW backend; the edge falls back to CPU and
    /// emits `video_decoder_unavailable` when the host can't satisfy
    /// the choice. The HW decoders are gated on the
    /// `video-decoder-nvdec` / `video-decoder-qsv` /
    /// `video-decoder-vaapi` Cargo features and shared between
    /// transcode and the local-display output, so an edge built for
    /// HW display playout also gets HW transcode decode "for free".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_decode: Option<HwDecodePreference>,
}

/// Build a browser-safe ("WebRTC-compatible") [`VideoEncodeConfig`] from an
/// optional operator-supplied one.
///
/// The `webrtc_compatible` output flag ([`WebrtcOutputConfig::webrtc_compatible`]
/// / [`SrtOutputConfig::webrtc_compatible`]) routes video through the normal
/// re-encode path but pins the encoder to settings a WebRTC audience
/// (browser / WHEP SFU / mediamtx / Janus) can actually decode:
///
/// * **H.264** — browsers do not decode HEVC. An HEVC codec (or an empty
///   codec) is replaced with `h264_auto`; the per-host resolver then picks a
///   compiled-in H.264 backend, with software libx264 as the fallback tail.
/// * **Zero B-frames** — the crux of the fix. B-frames are transmitted in
///   decode order, giving non-monotonic RTP timestamps that browser jitter
///   buffers and WHEP SFUs freeze on (RFC 7742 mandates Constrained Baseline,
///   which by definition forbids B-frames). `bframes` is hard-pinned to 0.
/// * **8-bit 4:2:0** — browsers do not decode 10-bit / 4:2:2 / 4:4:4.
/// * **Browser-decodable profile** — `high10` / `high422` / `high444` /
///   `main10` / `main422-10` profiles are downgraded to `main`. `baseline` /
///   `main` / `high` (all 8-bit) are left as the operator set them; Main or
///   High **with B-frames disabled** is WebRTC-valid, so baseline is not
///   forced (which would needlessly cost quality).
/// * **`zerolatency` tune** — low latency, and independently disables
///   B-frames in x264.
///
/// SPS/PPS travel in-band on every IDR because the re-encode call sites
/// (`output_webrtc` / `ts_video_replace`) already open the encoder with
/// `global_header = false`.
pub fn webrtc_safe_video_encode(existing: Option<&VideoEncodeConfig>) -> VideoEncodeConfig {
    fn is_hevc(codec: &str) -> bool {
        matches!(
            codec,
            "x265" | "hevc_nvenc" | "hevc_qsv" | "hevc_vaapi" | "hevc_rkmpp" | "hevc_auto"
        )
    }
    let mut enc = match existing {
        Some(v) => v.clone(),
        None => VideoEncodeConfig {
            // `h264_auto` resolves to the cheapest compiled-in H.264 backend
            // on this host (HW where present, else libx264).
            codec: "h264_auto".to_string(),
            source_video_pid: None,
            width: None,
            height: None,
            fps_num: None,
            fps_den: None,
            bitrate_kbps: None,
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
            hw_decode: None,
        },
    };
    // Force H.264 — browsers decode H.264 only.
    if enc.codec.trim().is_empty() || is_hevc(&enc.codec) {
        enc.codec = "h264_auto".to_string();
    }
    // Zero B-frames — the load-bearing constraint (monotonic RTP timestamps).
    enc.bframes = Some(0);
    // 8-bit 4:2:0 — the only chroma/depth browsers reliably decode.
    enc.chroma = Some("yuv420p".to_string());
    enc.bit_depth = Some(8);
    // Downgrade profiles that imply >8-bit or >4:2:0 to Main.
    if let Some(p) = enc.profile.as_deref() {
        if matches!(
            p,
            "high10" | "high422" | "high444" | "main10" | "main422-10" | "main422-10-intra"
        ) {
            enc.profile = Some("main".to_string());
        }
    }
    // Low-latency by default (also independently disables B-frames in x264).
    if enc.tune.is_none() {
        enc.tune = Some("zerolatency".to_string());
    }
    enc
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
    /// Optional source audio PID to transcode. When unset (default), the
    /// replacer locks onto the **first** audio stream in the active
    /// program's PMT whose stream_type is one of `0x0F` (AAC),
    /// `0x03`/`0x04` (MPEG-1/2), `0x81` (AC-3), `0x06` (private). When
    /// set, the replacer locks onto the named PID specifically — useful
    /// for MPTS programs with multiple audio tracks (e.g. EN/FR/5.1)
    /// where the operator wants to pin a specific track. PID range
    /// `0x0010..=0x1FFE`. If the named PID is absent from the live PMT
    /// the replacer falls back to first-match behaviour and emits a
    /// `tracing::warn!` with `error_code = audio_source_pid_not_found`.
    ///
    /// **Multi-program scope**: the in-place transcoder is single-
    /// program — only one audio PID is transcoded per output. To
    /// transcode multiple audio tracks from an MPTS (e.g. produce both
    /// an English-AAC and a French-AAC output), create one output per
    /// track with its own `program_number` filter + `source_audio_pid`
    /// pin + `audio_encode`. The validator rejects multi-program
    /// `pid_overrides` + `audio_encode`/`video_encode` on the same
    /// output with a clear error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_audio_pid: Option<u16>,
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
    /// **Opus-specific.** Rate-control mode. `None` (default) uses
    /// libopus's default (VBR). `"vbr"` is constrained variable bitrate;
    /// `"cbr"` is constant bitrate. Maps to ffmpeg's `-vbr {on,constrained,off}`
    /// where `cbr` → `off`, `vbr` → `constrained`, no value → `on`. Ignored
    /// for non-Opus codecs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus_vbr_mode: Option<String>,
    /// **Opus-specific.** Forward Error Correction (in-band redundancy
    /// for the previous frame). Defaults to off. Ignored for non-Opus.
    #[serde(default, skip_serializing_if = "is_false")]
    pub opus_fec: bool,
    /// **Opus-specific.** Discontinuous Transmission — encoder skips
    /// frames during silence to save bandwidth. Off by default (broadcast
    /// pipelines typically need a continuous stream so receivers don't
    /// lose A/V sync). Ignored for non-Opus.
    #[serde(default, skip_serializing_if = "is_false")]
    pub opus_dtx: bool,
    /// **Opus-specific.** Frame duration in milliseconds. One of: 5, 10,
    /// 20 (default), 40, 60. Shorter frames lower latency at the cost of
    /// efficiency; longer frames are higher quality at low bitrates.
    /// Ignored for non-Opus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus_frame_duration_ms: Option<u8>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Per-program MPEG-TS PID override block.
///
/// One entry's worth of "set the PMT/video/audio/PCR PID for *one* program"
/// configuration. Used inside [`TsPidOverridesMap`] keyed by `program_number`.
///
/// Two engine paths consume this:
///
/// 1. **Synthetic-TS inputs** (RTMP / RTSP / WebRTC / ST 2110-20 / test
///    pattern / media-player / replay / PCM-encode) build a fresh MPEG-TS
///    via the shared `TsMuxer`. They always emit a single program — the
///    map MUST contain exactly one entry, keyed `1`. Defaults: PMT 0x1000,
///    video 0x0100, audio 0x0101, PCR auto-rides the video PID (or audio
///    if no video). Every field below overrides the corresponding default.
///
/// 2. **TS-source transcoders + MPTS-passthrough rewriters** (`audio_encode` /
///    `video_encode` on RTP / UDP / SRT / RIST inputs and outputs, plus
///    standalone `pid_overrides` rewriting on passthrough TS): walk the
///    incoming PAT and PMTs, apply the entry whose key matches each program's
///    `program_number`. Programs not in the map pass through unchanged.
///
/// Valid PID range for each field: `0x0010..=0x1FFE`. PIDs `0x0000`
/// (PAT), `0x0001..=0x000F` (system reserved), and `0x1FFF` (NULL
/// padding) are rejected by validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsPidOverridesEntry {
    /// Override the PMT PID for this program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmt_pid: Option<u16>,
    /// Override the video PES PID for this program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_pid: Option<u16>,
    /// Override the audio PES PID for this program.
    ///
    /// Singular form — used by the synthetic-TS path (test pattern,
    /// media player, replay, image slate, PCM encode), which always
    /// emits exactly one audio elementary stream. On the TS-passthrough
    /// path with a multi-audio program (e.g. EN / FR / ES tracks), this
    /// field still remaps the *first* audio PID for back-compat; use
    /// [`Self::audio_pids`] to remap additional tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_pid: Option<u16>,
    /// Per-source-PID audio remap for multi-language / multi-track programs.
    ///
    /// Keyed by **source audio PID** → **target audio PID**. Honoured by
    /// the passthrough rewriter ([`crate::engine::ts_pid_overrides_rewriter`])
    /// and ignored by the synthetic-TS path (where there's never more than
    /// one audio ES). When both [`Self::audio_pid`] and an entry in this
    /// map would target the same source PID, the explicit entry in this
    /// map wins.
    ///
    /// Source PIDs are matched against the live PMT; entries for source
    /// PIDs that don't exist in the current PMT are inert (the manager UI
    /// surfaces this against the PSI catalogue).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::audio_pids_serde"
    )]
    pub audio_pids: Option<std::collections::BTreeMap<u16, u16>>,
    /// Override the PCR PID for this program. When unset, PCR rides the
    /// video PID (or audio if no video). The override must point at an ES
    /// PID that actually exists in the PMT — the muxer will not synthesise
    /// PCR on a standalone PID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_pid: Option<u16>,
}

/// Map from `program_number` → per-program PID overrides.
///
/// One entry per program the operator wants to re-PID. Programs not in
/// the map pass through unchanged. For SPTS / synthetic-TS configs, the
/// map MUST contain exactly one entry, keyed `1`. For MPTS configs, list
/// any subset of programs.
///
/// Why a map and not a single entry: an operator may want different PIDs
/// per program of an MPTS (e.g. unify program 1's audio PID across N
/// inputs while leaving program 2 alone). Modelling this as a map keeps
/// the schema unambiguous about which program a PID applies to and lets
/// validation catch synthetic-TS configs that try to override a non-existent
/// program 2.
pub type TsPidOverridesMap = std::collections::BTreeMap<u16, TsPidOverridesEntry>;

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
    /// Source-specific multicast (SSM) source for this leg. Real Red/Blue
    /// plants often have different source IPs per network — see
    /// `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
    /// Pin this leg to a physical NIC. Independent of the parent's
    /// binding so 2022-7 plants can put Red and Blue on different
    /// physical links. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Source-specific multicast (SSM) source for the primary leg — see
    /// `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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
    /// Optional MPEG-TS PID overrides for the synthesised TS that the
    /// PCM-encode stage emits (only consulted when `audio_encode` is
    /// set; ignored when the PCM input feeds the broadcast channel as
    /// raw PCM-RTP). See [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`] for per-field semantics.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this ST 2110 audio input to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Pin this ST 2110 audio output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Source-specific multicast (SSM) source — see `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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
    /// Optional MPEG-TS PID overrides for the synthesised TS that the
    /// PCM-encode stage emits (only consulted when `audio_encode` is
    /// set). See [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this RTP audio input to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Optional MPEG-TS PID overrides for the synthesised TS that the
    /// `audio_302m` transport mode emits (ignored in plain RTP mode). See
    /// [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this RTP audio output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Source-specific multicast (SSM) source for the primary leg — see
    /// `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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
    /// Pin this ST 2110-40 ancillary input to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Pin this ST 2110-40 ancillary output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Source-specific multicast (SSM) source for the primary leg — see
    /// `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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
    /// Optional MPEG-TS PID overrides for the synthesised TS that the
    /// 2110-20 encode stage emits. See [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin this ST 2110-20 video input to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Deprecated: pacing is now automatic on every ST 2110-20 output
    /// via `engine::wire_emit`. Field is retained for backward
    /// compatibility with stored configs and ignored at runtime
    /// (the edge logs a warning when the field is set). New configs
    /// should omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Pacing is now automatic — see docs/wire-pacing.md")]
    pub wire_pacing: Option<WirePacingConfig>,
    /// Pin this ST 2110-20 video output to a physical NIC. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// HW backend for the egress video decode (the output decodes the
    /// flow's H.264/HEVC and packetizes raw RFC 4175). Same enum and
    /// semantics as the transcode/display `hw_decode`: unset = `auto`
    /// (best available probed backend, VAAPI ≻ NVDEC ≻ QSV ≻ CPU);
    /// `cpu` forces the auto-threaded software decoder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_decode: Option<HwDecodePreference>,
}

/// Per-sub-stream bind (ST 2110-23 input). Each sub-stream is a valid -20
/// receiver for one partition of the full video essence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct St2110_23SubStreamBind {
    pub bind_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_addr: Option<String>,
    /// Source-specific multicast (SSM) source for this sub-stream's primary
    /// leg — see `RtpInputConfig::source_addr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_addr: Option<String>,
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
    /// Optional MPEG-TS PID overrides for the synthesised TS that the
    /// 2110-23 encode stage emits. See [`TsPidOverridesEntry`] (per program) and [`TsPidOverridesMap`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
    /// Pin every ST 2110-23 sub-stream input to a physical NIC. Each
    /// sub-stream's `St2110_23SubStreamBind` already has its own
    /// `interface_addr`; this is the parent fallback when the per-bind
    /// override isn't set. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
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
    /// Deprecated: pacing is now automatic on every ST 2110-23
    /// sub-stream output via `engine::wire_emit`. Field is retained
    /// for backward compatibility and ignored at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[deprecated(note = "Pacing is now automatic — see docs/wire-pacing.md")]
    pub wire_pacing: Option<WirePacingConfig>,
    /// Pin every ST 2110-23 sub-stream output to a physical NIC. Each
    /// sub-stream's `St2110_23SubStreamDest` already has its own
    /// `interface_addr`; this is the parent fallback. See [`InterfaceBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_binding: Option<InterfaceBinding>,
    /// HW backend for the egress video decode — same semantics as
    /// [`St2110VideoOutputConfig::hw_decode`]. Unset = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_decode: Option<HwDecodePreference>,
}

fn default_st2110_video_pt() -> u8 {
    96
}

fn default_st2110_video_payload_budget() -> usize {
    1428
}

/// ST 2110-21 sender profile per ST 2110-21 §6.3. Selects the pacer
/// math; v1 of the pacer treats all variants as `narrow_linear` (even
/// pacing across frame period). Reserved for the gapped-narrow follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum St2110_21ProfileConfig {
    Narrow,
    NarrowLinear,
    Wide,
}

impl Default for St2110_21ProfileConfig {
    fn default() -> Self {
        St2110_21ProfileConfig::Narrow
    }
}

/// Wire-pacing mode for an output. Today only ST 2110-20 / -23 outputs
/// honour this; compressed-TS outputs (UDP, RTP, SRT) are paced by
/// `engine::wire_emit` unconditionally and ignore this field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WirePacingConfig {
    /// Kernel-paced via `SO_TXTIME` + ETF qdisc. Operator must install
    /// the ETF qdisc on the egress NIC separately
    /// (`packaging/setup-etf-qdisc.sh`); the edge process does not
    /// install qdiscs (deliberately operator-side, needs
    /// `CAP_NET_ADMIN`). Capability `wire_pacing_txtime` advertised on
    /// `HealthPayload.capabilities` only when the host's kernel
    /// accepts `SO_TXTIME`. Without ETF qdisc the kernel still accepts
    /// the `SCM_TXTIME` CMSG but emits immediately — same observable
    /// behavior as today's unpaced ST 2110, no regression.
    TxTime {
        #[serde(default)]
        profile: St2110_21ProfileConfig,
    },
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

/// Local-display output: decode the flow's video + audio and render to a
/// physical Linux HDMI/DisplayPort connector + an ALSA audio device.
///
/// `device` is a KMS connector name (e.g. `"HDMI-A-1"`, `"DP-2"`) drawn from
/// `HealthPayload.display_devices` enumerated at edge startup. `audio_device`
/// is an ALSA device id (`"hw:0,3"`, `"plughw:0,3"`, `"default"`); `None`
/// disables audio playback. The remaining fields select which TS program,
/// audio elementary stream, and stereo channel pair drive the output.
///
/// At runtime this output is only spawnable on Linux builds compiled with
/// the `display` Cargo feature. Configs with `display` outputs round-trip
/// cleanly on every platform — non-Linux/feature-off builds reject the
/// output at `start_output()` with `display_device_invalid`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active. Passive outputs stay in
    /// config but the engine does not spawn a task for them.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars) for UI grouping and
    /// the Phase 2 switchboard feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// KMS connector name from the edge's display enumeration, e.g.
    /// `"HDMI-A-1"`, `"DP-2"`, `"DVI-D-1"`. Validated against the
    /// canonical KMS naming pattern `^[A-Z][A-Z0-9-]{0,63}$`.
    pub device: String,

    /// ALSA device id (`"hw:0,3"`, `"plughw:0,3"`, `"default"`,
    /// `"sysdefault"`, `"pulse"`). `None` or empty mutes audio playback —
    /// the display output is video-only in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,

    /// MPEG-TS program filter (1-based; `program_number = 0` is reserved
    /// for the NIT and rejected). `None` selects the lowest program in
    /// the active input's PAT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,

    /// Audio elementary-stream index within the chosen program. `None`
    /// selects the first audio track. Must be < 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_track_index: Option<u8>,

    /// Stereo channel pair to render from the decoded multichannel audio.
    /// Both indices must be < 8 and not equal. Defaults to `[0, 1]` (L/R).
    #[serde(default = "default_audio_channel_pair")]
    pub audio_channel_pair: [u8; 2],

    /// How the display task chooses the connector's KMS mode.
    /// Default `MatchSource` re-modesets to the smallest mode that
    /// covers the source's `(width, height)` on every source-shape
    /// change — best A/V sync, no scaling. `MonitorNative` picks the
    /// connector's preferred (panel-native) mode once at task startup
    /// and holds it; libswscale upscales the source to fill it. Pick
    /// `MonitorNative` for fixed-mode panels (HDCP-locked, signage)
    /// and most desktop monitors that handle their native mode best.
    #[serde(default)]
    pub scaling_mode: DisplayScalingMode,

    /// **Deprecated, ignored at runtime.** Replaced by `scaling_mode`.
    /// Accepted by the deserializer for backward-compat round-trip on
    /// existing configs; a load-time `tracing::warn` fires when set.
    /// Will be removed at the next major bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,

    /// **Deprecated, ignored at runtime.** Same rationale as
    /// `resolution`. Replaced by `scaling_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_hz: Option<u32>,

    /// A/V sync mode.
    /// - `"vsync_to_display"` (default): audio-master. Video is paced to the
    ///   measured ALSA audio-playout clock (`snd_pcm_delay()`); an adaptive
    ///   resampler holds the DAC buffer so the soundcard-vs-source rate gap
    ///   never accumulates.
    /// - `"genlock"`: the audio resampler instead locks the measured playout
    ///   to the flow master clock, so the panel stays rate-coherent with the
    ///   flow's wire outputs and with other edges on the same grandmaster.
    ///   Video still follows the audio playout, so on-screen lip-sync is
    ///   identical to audio-master; genlock only adds cross-output coherence.
    #[serde(default = "default_sync_mode")]
    pub sync_mode: String,

    /// Render a per-PID, per-channel audio level meter strip across the
    /// bottom of the picture. Independent of `audio_track_index` — every
    /// audio PID in the active program is decoded and metered, even ones
    /// not routed to ALSA. Defaults `false`. See
    /// `bilbycast-edge/docs/configuration-guide.md` ("Audio bars overlay")
    /// for the layout and meter-style reference.
    #[serde(default)]
    pub show_audio_bars: bool,

    /// Operator's hardware-decode preference for this display output.
    ///
    /// `None` (or `"auto"`) — pick the best HW backend the edge has
    /// compiled in and probed at startup, fall back to CPU if none are
    /// available. `"cpu"` forces software libavcodec, leaving HW
    /// resources free for transcode flows elsewhere on the node.
    /// `"nvdec"` / `"qsv"` force a specific HW backend; the edge
    /// rejects the output at start with `display_hw_decode_unavailable`
    /// when the host can't satisfy the choice (feature missing, driver
    /// missing, or session capacity exhausted at probe time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_decode: Option<HwDecodePreference>,
}

fn default_audio_channel_pair() -> [u8; 2] {
    [0, 1]
}

fn default_sync_mode() -> String {
    "vsync_to_display".to_string()
}

/// Per-display-output hardware-decode preference. Mirrors the encoder
/// codec selection on transcoding outputs — operators pick the backend
/// they want to dedicate to this confidence-monitor playout (or `Cpu`
/// to keep GPU resources free for a heavy transcode flow elsewhere).
///
/// String wire shape: `"auto"`, `"cpu"`, `"nvdec"`, `"qsv"`, `"vaapi"`.
/// Default is `Auto`. The deserialiser accepts the short forms only;
/// unknown strings fail validation with `display_hw_decode_invalid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HwDecodePreference {
    #[default]
    Auto,
    Cpu,
    Nvdec,
    Qsv,
    Vaapi,
}

/// How the display output picks the connector's KMS mode.
///
/// `MatchSource` (default) — autodetect on the first decoded frame and
/// re-modeset whenever the source's `(width, height)` changes; pick the
/// smallest connector mode that covers the source dims. Tightest A/V
/// timing, no scaling cost on the renderer.
///
/// `MonitorNative` — set the connector's preferred (panel-native) mode
/// once at task startup and hold it. Source frames are scaled up to fill
/// the panel via libswscale. Right choice for fixed-mode panels (HDCP,
/// signage) and most desktop monitors that handle non-native modes
/// poorly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplayScalingMode {
    #[default]
    MatchSource,
    MonitorNative,
}

// ── MXL (Media eXchange Layer) configuration ────────────────────────────
//
// Gated by the `mxl` Cargo feature (default off). MXL v1.0 carries three
// essence types over same-host shared memory: V210 4:2:2 10-bit video,
// Float32 PCM @ 48 kHz audio, and RFC 8331 ancillary data. PTP is
// mandatory — validation rejects `master_clock=wallclock` on any flow
// referencing an MXL input or output. See
// `docs/mxl-integration-plan.md` for the architectural rationale and
// `bilbycast-mxl-rs/CLAUDE.md` for the build prereq footprint.

/// Shared identifier for a MXL flow on a domain. Both producer (output)
/// and consumer (input) must agree on `(domain_path, flow_name)` for
/// libmxl to route grains between them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlDomainRef {
    /// Path to the MXL domain directory — typically `/dev/shm/<name>`.
    /// Must live on tmpfs or ramfs for libmxl's shared-memory perf model;
    /// the edge emits a `mxl_domain_not_tmpfs` Warning event otherwise.
    pub domain_path: String,
    /// MXL flow name (libmxl identifies flows by this string).
    pub flow_name: String,
}

/// MXL **video** input. Consumes V210 grains from the bus, decodes V210
/// into planar 4:2:2 10-bit YUV, encodes via the configured backend, and
/// publishes MPEG-TS onto the flow's broadcast channel.
///
/// `video_encode` is currently required at v1.0 — bilbycast's flows are
/// downstream-MPEG-TS-centric, so an MXL-in flow that wants to feed any
/// network output needs the encode step. (When M3 grows an MXL→MXL
/// passthrough output, the requirement relaxes.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlVideoInputConfig {
    /// Shared-memory domain + flow identifier.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Frame rate numerator (e.g. 30000 for 29.97).
    pub frame_rate_num: u32,
    /// Frame rate denominator (e.g. 1001 for 29.97).
    pub frame_rate_den: u32,
    /// PTP clock domain (0..=127). Inherits from the parent flow when
    /// omitted. Required for MXL — validation rejects
    /// `master_clock=wallclock` on any MXL flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Mandatory ingress video encode. Mirrors `St2110VideoInputConfig`.
    /// A feature-flagged backend (`video-encoder-x264` / `-x265` /
    /// `-nvenc` / `-qsv` / `-vaapi`) must be compiled in.
    pub video_encode: VideoEncodeConfig,
    /// MPEG-TS PID overrides for the synthesised TS.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
}

fn default_sdi_format() -> String {
    "auto".to_string()
}
fn default_sdi_pixel_format() -> String {
    "uyvy422".to_string()
}
fn default_sdi_audio_channels() -> u8 {
    2
}

/// SDI capture input via a Blackmagic DeckLink card (the `sdi` input type,
/// gated by the `sdi-decklink` Cargo feature). One device handle delivers
/// packed 4:2:2 video **and** embedded PCM audio; the input task encodes the
/// video via `video_encode`, optionally re-encodes the audio via
/// `audio_encode`, and muxes both into a single A+V MPEG-TS on the flow's
/// broadcast channel. Self-clocked — no PTP requirement (unlike MXL).
///
/// `device` is the DeckLink SDK display name, e.g. `"DeckLink Quad (1)"`, as
/// listed by the boot probe and on `HealthPayload.sdi_devices[]`.
/// Native SDI playout output via Blackmagic DeckLink (`sdi-decklink`
/// feature). Decodes the flow's video elementary stream and schedules the
/// frames against the DeckLink card's clock. Video-only today.
///
/// `device` is the SDK display name (as listed on
/// `HealthPayload.sdi_devices[]`). `mode` is REQUIRED and explicit — playout
/// has nothing to auto-detect from — and must match the decoded video's
/// raster (frames of any other size are dropped with an alarm, so a source
/// switch cannot emit a garbled picture).
///
/// On 8-port Quad cards note the physical↔software connector interleave and
/// the sub-device pair routing (playout from a sub-device whose own connector
/// carries an input emerges on its pair partner's connector) — see
/// `bilbycast-decklink-rs/CLAUDE.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdiOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag (max 64 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// DeckLink device display name, e.g. `"DeckLink Quad (1)"`.
    pub device: String,
    /// DeckLink mode FourCC to play out, e.g. `"Hi50"` / `"Hp25"`.
    /// Required; determines the raster + frame rate the card is opened at.
    /// Must match the decoded video's raster.
    pub mode: String,
    /// Wire pixel format. `"uyvy422"` (8-bit) is the only one implemented.
    #[serde(default = "default_sdi_pixel_format")]
    pub pixel_format: String,
    /// Embedded-audio channel count to play out (2 / 8 / 16). 0 = video-only.
    /// Audio is always scheduled at 48 kHz on the card; a decoded audio track
    /// at another rate is dropped with an alarm. Source audio is decoded
    /// (AAC / MP2 / AC-3 / E-AC-3 / Opus), interleaved into this channel
    /// count, and lip-synced to video via the shared playout clock.
    #[serde(default = "default_sdi_audio_channels")]
    pub audio_channels: u8,
    /// MPEG-TS program filter (1-based). `None` selects the lowest program
    /// in the PAT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_number: Option<u16>,
    /// Translate inbound SCTE-35 cues into SCTE-104 VANC on SDI playout.
    #[serde(default)]
    pub scte35_injection: bool,
    /// Operator A/V-sync trim for the embedded audio, in milliseconds.
    /// **Positive delays audio** (it plays later, correcting audio-early);
    /// **negative advances it** (it plays earlier, correcting audio-late).
    /// Applied as a constant shift to every scheduled audio block's card
    /// stream time; the internal drift-free sample counter is unaffected, so
    /// the trim never accumulates. `0` (default) = lip-sync straight off the
    /// shared 90 kHz playout clock. Validated to `-1000..=1000` ms.
    #[serde(default)]
    pub audio_offset_ms: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdiInputConfig {
    /// DeckLink device display name, e.g. `"DeckLink Quad (1)"`.
    pub device: String,
    /// SDI mode: `"auto"` (card input-format detection, **strongly preferred**)
    /// or a concrete DeckLink mode FourCC such as `"Hi50"` / `"Hp30"`. A forced
    /// mode that does not match the source makes the card report no signal and
    /// emit bars, so `"auto"` is the default and the right answer nearly always.
    #[serde(default = "default_sdi_format")]
    pub format: String,
    /// Wire pixel format. `"uyvy422"` (8-bit, default) is the only one
    /// implemented; `"v210"` (10-bit) is accepted by the card but has no
    /// unpacker yet and is rejected at validation.
    #[serde(default = "default_sdi_pixel_format")]
    pub pixel_format: String,
    /// Embedded-audio channel count to capture (2 / 8 / 16). 0 = video only.
    #[serde(default = "default_sdi_audio_channels")]
    pub audio_channels: u8,
    /// Translate SCTE-104 VANC triggers into an SCTE-35 PID in the egress TS.
    #[serde(default)]
    pub scte35_extraction: bool,
    /// Decode SMPTE 12M-2 ATC VANC timecode (DID `0x60`) into a live
    /// `InputStats.sdi_stats.timecode` field. Continuous state, not an
    /// event — a real source updates it every frame.
    #[serde(default)]
    pub timecode_extraction: bool,
    /// Detect CEA-608/708 caption presence from VANC (DID `0x61`) —
    /// presence and cc_count only, not caption text. Fires
    /// `sdi_captions_detected` once per session per caption type and
    /// surfaces `InputStats.sdi_stats.captions_cea608_present` /
    /// `captions_cea708_present`.
    #[serde(default)]
    pub captions_extraction: bool,
    /// Mandatory ingress video encode (mirrors `MxlVideoInputConfig`). A
    /// feature-flagged backend (`video-encoder-x264` / `-x265` / `-nvenc` /
    /// `-qsv` / `-vaapi`) must be compiled in.
    pub video_encode: VideoEncodeConfig,
    /// Optional embedded-audio re-encode into the muxed TS audio PID. When
    /// unset, audio is encoded with a sensible AAC-LC default so the flow
    /// carries sound; set explicitly to pick codec / bitrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// MPEG-TS PID overrides for the synthesised A+V TS.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
}

/// MXL **video** output. Decodes the flow's H.264/HEVC TS, scales to
/// 4:2:2 10-bit planar, packs into V210, publishes grains onto the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlVideoOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Shared-memory domain + flow identifier we publish onto.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// Output frame dimensions.
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
}

/// MXL **audio** input — consumes a Float32 PCM samples flow from the bus.
///
/// MXL v1.0 fixes audio at 48 kHz Float32. Channel count and packet time
/// are operator-configurable. Optional `audio_encode` synthesises an
/// audio-only MPEG-TS for TS-bearing outputs on the same flow (same
/// shape as `St2110AudioInputConfig::audio_encode`); when set, the
/// broadcast channel switches from raw PCM to TS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlAudioInputConfig {
    /// Shared-memory domain + flow identifier.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// Channel count (1, 2, 4, 8, 16).
    #[serde(default = "default_st2110_channels")]
    pub channels: u8,
    /// Packet time in microseconds. libmxl bundles samples per grain to
    /// match. Default 1000 (1ms, ST 2110-30 PM-compatible).
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    /// PTP clock domain (0..=127). Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Optional planar PCM transcode — see
    /// `St2110AudioInputConfig::transcode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// Optional ingress audio re-encode — see
    /// `St2110AudioInputConfig::audio_encode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_encode: Option<AudioEncodeConfig>,
    /// MPEG-TS PID overrides when `audio_encode` is set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::pid_overrides_serde"
    )]
    pub pid_overrides: Option<TsPidOverridesMap>,
}

/// MXL **audio** output — publishes Float32 PCM samples onto the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlAudioOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Shared-memory domain + flow identifier we publish onto.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// Channel count.
    #[serde(default = "default_st2110_channels")]
    pub channels: u8,
    /// Packet time in microseconds.
    #[serde(default = "default_st2110_packet_time_us")]
    pub packet_time_us: u32,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
    /// Optional planar PCM transcode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
}

/// MXL **ancillary** input — consumes RFC 8331 ANC grains from the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlAncInputConfig {
    /// Shared-memory domain + flow identifier.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
}

/// MXL **ancillary** output — publishes RFC 8331 ANC grains onto the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MxlAncOutputConfig {
    /// Unique output ID within this flow.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Whether this output is currently active.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Optional free-form group tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Shared-memory domain + flow identifier we publish onto.
    #[serde(flatten)]
    pub mxl: MxlDomainRef,
    /// PTP clock domain (0..=127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_domain: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `egress_pacing` is tri-state on the wire: absent = auto (`None`),
    /// explicit `forward` / `pcr` survive untouched. Existing configs
    /// that never set the field keep deserializing to `None` (and
    /// re-serialize without it) — the auto-resolution must never be
    /// persisted back into the config.
    #[test]
    fn egress_pacing_tri_state_serde_back_compat() {
        let base = r#"{
            "id": "udp-1",
            "name": "UDP out",
            "dest_addr": "239.1.2.3:5004"
        }"#;
        let parsed: UdpOutputConfig = serde_json::from_str(base).expect("parse absent");
        assert_eq!(
            parsed.egress_pacing, None,
            "absent field must stay None (auto)"
        );
        let reser = serde_json::to_string(&parsed).unwrap();
        assert!(
            !reser.contains("egress_pacing"),
            "auto (None) must not be persisted: {reser}"
        );

        for (literal, expect) in [
            ("forward", EgressPacingMode::Forward),
            ("pcr", EgressPacingMode::Pcr),
            ("servo", EgressPacingMode::Servo),
        ] {
            let json = format!(
                r#"{{
                    "id": "udp-1",
                    "name": "UDP out",
                    "dest_addr": "239.1.2.3:5004",
                    "egress_pacing": "{literal}"
                }}"#
            );
            let parsed: UdpOutputConfig =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {literal}: {e}"));
            assert_eq!(parsed.egress_pacing, Some(expect));
            let reser = serde_json::to_string(&parsed).unwrap();
            assert!(reser.contains(&format!("\"egress_pacing\":\"{literal}\"")));
        }
    }

    /// The auto-resolution rule: explicit always wins; unset resolves to
    /// `pcr` only when the flow carries a bonded input, else `forward`
    /// (the historical default).
    #[test]
    fn egress_pacing_auto_resolution_rule() {
        use EgressPacingMode::*;
        assert_eq!(EgressPacingMode::resolve_auto(None, false), Forward);
        assert_eq!(EgressPacingMode::resolve_auto(None, true), Pcr);
        // Explicit settings are never overridden — bonded or not.
        for mode in [Forward, Pcr, Servo] {
            assert_eq!(EgressPacingMode::resolve_auto(Some(mode), true), mode);
            assert_eq!(EgressPacingMode::resolve_auto(Some(mode), false), mode);
        }
    }

    /// `DisplayScalingMode` defaults to `MatchSource` (today's runtime
    /// behaviour) and serializes snake_case so the JSON wire shape
    /// matches the manager's validation regex (`match_source` /
    /// `monitor_native`).
    #[test]
    fn display_scaling_mode_default_and_serde_shape() {
        assert_eq!(
            DisplayScalingMode::default(),
            DisplayScalingMode::MatchSource
        );
        let s = serde_json::to_string(&DisplayScalingMode::MatchSource).unwrap();
        assert_eq!(s, "\"match_source\"");
        let s = serde_json::to_string(&DisplayScalingMode::MonitorNative).unwrap();
        assert_eq!(s, "\"monitor_native\"");
        let m: DisplayScalingMode = serde_json::from_str("\"monitor_native\"").unwrap();
        assert_eq!(m, DisplayScalingMode::MonitorNative);
    }

    /// A `DisplayOutputConfig` saved by an old edge (with `resolution` +
    /// `refresh_hz`, no `scaling_mode`) must still deserialize without
    /// error. The deprecated fields are accepted on the wire and the
    /// runtime defaults to `MatchSource`. Re-serialization preserves
    /// them so the round-trip is lossless on old configs (the manager
    /// UI re-saves them out the next time the operator edits).
    #[test]
    fn display_output_config_legacy_fields_round_trip() {
        let json = r#"{
            "id": "disp-1",
            "name": "Green Room HDMI",
            "active": true,
            "device": "HDMI-A-1",
            "audio_device": "hw:0,3",
            "audio_channel_pair": [0, 1],
            "resolution": "1920x1080",
            "refresh_hz": 60,
            "sync_mode": "vsync_to_display"
        }"#;
        let parsed: DisplayOutputConfig = serde_json::from_str(json).expect("parse legacy");
        assert_eq!(parsed.scaling_mode, DisplayScalingMode::MatchSource);
        assert_eq!(parsed.resolution.as_deref(), Some("1920x1080"));
        assert_eq!(parsed.refresh_hz, Some(60));
        let reser = serde_json::to_string(&parsed).unwrap();
        assert!(reser.contains("\"scaling_mode\":\"match_source\""));
        assert!(reser.contains("\"resolution\":\"1920x1080\""));
    }

    /// New configs that set `scaling_mode` directly round-trip cleanly
    /// without dragging the deprecated fields along.
    #[test]
    fn display_output_config_monitor_native_round_trip() {
        let json = r#"{
            "id": "disp-2",
            "name": "Studio Monitor",
            "device": "DP-1",
            "scaling_mode": "monitor_native"
        }"#;
        let parsed: DisplayOutputConfig = serde_json::from_str(json).expect("parse monitor-native");
        assert_eq!(parsed.scaling_mode, DisplayScalingMode::MonitorNative);
        assert!(parsed.resolution.is_none());
        assert!(parsed.refresh_hz.is_none());
        let reser = serde_json::to_string(&parsed).unwrap();
        assert!(reser.contains("\"scaling_mode\":\"monitor_native\""));
        assert!(!reser.contains("\"resolution\""));
        assert!(!reser.contains("\"refresh_hz\""));
    }

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
        assert!(matches!(
            &a.programs[0].streams[1].source,
            SlotSource::Hitless { .. }
        ));
        let reser = serde_json::to_string(&parsed).unwrap();
        let reparsed: FlowConfig = serde_json::from_str(&reser).unwrap();
        assert_eq!(parsed.assembly, reparsed.assembly);
    }

    /// The PID-bus manager UI (`node_config.html` + `assembly_editor.js`)
    /// writes `FlowAssembly` JSON in exactly the shape emitted by the edge
    /// here. This test pins that contract by deserialising every
    /// testbed probe config through `AppConfig`, then re-serialising and
    /// re-parsing. Any drift (field rename, new non-optional field,
    /// tag change on `SlotSource`) breaks the UI round-trip and this
    /// catches it loudly.
    #[test]
    fn pid_bus_testbed_configs_roundtrip_exactly() {
        // Five configs, one per probe harness: Phase 5 explicit-Pid,
        // Phase 6 Essence, MPTS, Phase 6.5 decoded-ES, Phase 7
        // runtime-swap target. Each exercises a different SPTS/MPTS +
        // SlotSource combination the UI must round-trip.
        let paths = [
            "../testbed/pid-bus-spts/config.json",
            "../testbed/pid-bus-spts/config-essence.json",
            "../testbed/pid-bus-spts/config-mpts.json",
            "../testbed/pid-bus-spts/config-decoded-es.json",
            "../testbed/pid-bus-spts/config-switch.json",
        ];
        for p in paths {
            let text = match std::fs::read_to_string(p) {
                Ok(t) => t,
                Err(_) => continue, // testbed may be absent in CI
            };
            let parsed: AppConfig =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {p}: {e}"));
            let reser = serde_json::to_string(&parsed).unwrap();
            let reparsed: AppConfig =
                serde_json::from_str(&reser).unwrap_or_else(|e| panic!("reparse {p}: {e}"));
            // Assembly blocks must survive round-trip byte-identically.
            for (a, b) in parsed.flows.iter().zip(reparsed.flows.iter()) {
                assert_eq!(a.assembly, b.assembly, "assembly drift in {p}");
            }
        }
    }

    /// Absent `assembly` must not appear in the JSON and must still
    /// parse back as `None` — keeps legacy flows byte-clean on the wire.
    #[test]
    fn flow_assembly_skipped_when_unset() {
        let json = r#"{"id":"f","name":"F","input_ids":["a"],"output_ids":["o"]}"#;
        let parsed: FlowConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.assembly.is_none());
        let reser = serde_json::to_string(&parsed).unwrap();
        assert!(
            !reser.contains("assembly"),
            "assembly leaked into JSON: {reser}"
        );
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
            assert!(
                reser.contains("\"pid_map\""),
                "pid_map stripped on reserialise: {reser}"
            );
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
        assert!(!parse(r#"{"type":"st2110_40","bind_addr":"127.0.0.1:5000"}"#).is_ts_carrier());
        assert!(!parse(
            r#"{"type":"rtp_audio","bind_addr":"127.0.0.1:5000","sample_rate":48000,"channels":2,"bit_depth":24}"#
        ).is_ts_carrier());

        // TS-carrying — analyzer must spawn for these.
        assert!(parse(r#"{"type":"udp","bind_addr":"127.0.0.1:5000"}"#).is_ts_carrier());
        assert!(parse(r#"{"type":"rtp","bind_addr":"127.0.0.1:5000"}"#).is_ts_carrier());
    }

    #[test]
    fn test_roundtrip_config() {
        let config = AppConfig {
            version: 2,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            setup_token: None,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            resource_limits: None,
            logging: None,
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
            nmos_registration: None,
            upgrades: None,
            cellular_uplinks: Vec::new(),
            starlink_uplinks: Vec::new(),
            bond_uplinks: Vec::new(),
            shared_leg_broker: None,
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "rtp-in-1".to_string(),
                name: "RTP Input".to_string(),
                config: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    external_address: None,
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
                    program_number: None,
                    pid_map: None,
                    pid_overrides: None,
                    interface_binding: None,
                    ingress_delay_ms: None,
                    ingress_dejitter_ms: None,
                    passthrough_clock: None,
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
                cbr_pad_to_kbps: None,
                pid_overrides: None,
                interface_binding: None,
                egress_pacing: None,
                egress_buffer_ms: None,
            })],
            flows: vec![FlowConfig {
                id: "test-flow".to_string(),
                name: "Test Flow".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                thumbnail_interval_secs: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["rtp-in-1".to_string()],
                output_ids: vec!["rtp-out-1".to_string()],
                assembly: None,
                content_analysis: None,
                recording: None,
                master_clock: None,
                bandwidth_profile: None,
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
                    external_address: None,
                    interface_addr: None,
                    source_addr: None,
                    audio_encode: None,
                    transcode: None,
                    video_encode: None,
                    program_number: None,
                    pid_map: None,
                    pid_overrides: None,
                    interface_binding: None,
                    ingress_delay_ms: None,
                    ingress_dejitter_ms: None,
                    passthrough_clock: None,
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
                cbr_pad_to_kbps: None,
                pid_overrides: None,
                interface_binding: None,
                egress_pacing: None,
                egress_buffer_ms: None,
            })],
            flows: vec![FlowConfig {
                id: "flow-1".to_string(),
                name: "Flow 1".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                thumbnail_interval_secs: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["in-1".to_string()],
                output_ids: vec!["out-1".to_string()],
                assembly: None,
                content_analysis: None,
                recording: None,
                master_clock: None,
                bandwidth_profile: None,
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
                thumbnail_interval_secs: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["nonexistent".to_string()],
                output_ids: vec![],
                assembly: None,
                content_analysis: None,
                recording: None,
                master_clock: None,
                bandwidth_profile: None,
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
                thumbnail_interval_secs: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["in-1".to_string()],
                output_ids: vec!["out-1".to_string(), "out-2".to_string()],
                assembly: None,
                content_analysis: None,
                recording: None,
                master_clock: None,
                bandwidth_profile: None,
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
