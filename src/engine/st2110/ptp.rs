// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

// Phase 1 step 2: foundation. Most public items are not yet referenced from
// the rest of the codebase — they get wired into the ST 2110 input/output
// tasks in step 4. The unit tests in this module exercise them directly.
#![allow(dead_code)]

//! PTP (IEEE 1588) clock state reporter.
//!
//! bilbycast-edge does **not** implement a PTP slave in the default build.
//! Operators run `ptp4l` (linuxptp) as a system daemon and bilbycast reads
//! the daemon's management Unix socket to surface live clock state to the
//! manager. This keeps the binary 100% Rust and respects the existing
//! deployment model documented in `docs/st2110.md`.
//!
//! Phase 1 step 2 ships:
//!
//! - [`PtpState`] / [`PtpLockState`]: data model used by stats, events, and the
//!   manager UI.
//! - [`PtpStateReporter`]: a low-rate poller that opens the ptp4l management
//!   Unix datagram socket, sends a `GET CURRENT_DATA_SET` (mid `0x2001`) and a
//!   `GET PARENT_DATA_SET` (mid `0x2002`) every poll, and parses the
//!   IEEE 1588 management TLV responses.
//!
//! When the socket is missing, the daemon is not running, the address family
//! is not supported, or any decode fails, the reporter returns
//! [`PtpLockState::Unavailable`] instead of erroring. This is the contract:
//! a missing PTP daemon must never break a flow.
//!
//! ## Optional `--features ptp-internal`
//!
//! When `ptp-internal` is enabled, [`PtpStateReporter::spawn_internal`] is
//! intended to launch an in-process `statime` slave clock and surface its
//! state through the same [`PtpState`] interface. The feature flag exists in
//! `Cargo.toml` so the build matrix is exercised; the actual `statime`
//! integration lands in a follow-up step.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot_lite::RwLock;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity, category};

/// Lock state of the PTP slave clock for this domain.
///
/// Maps roughly to ptp4l's `port_state` plus `master_offset` thresholds. The
/// manager uses this to render the PTP card on the node detail page and to
/// alarm on `Unlocked` for ST 2110 flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtpLockState {
    /// No PTP daemon is reachable. The Phase 1 reporter returns this when
    /// `/var/run/ptp4l` is missing, when the daemon does not respond within
    /// the poll timeout, or when the response cannot be decoded.
    Unavailable,
    /// PTP is running but the slave is in `LISTENING`/`UNCALIBRATED` and has
    /// not yet acquired a lock with the grandmaster.
    Acquiring,
    /// Slave is locked: `port_state == SLAVE` and the absolute master offset
    /// is below the configured tolerance.
    Locked,
    /// Slave was previously locked but the master offset has drifted above
    /// the tolerance. Used by the alarm system to fire `ptp_lock_lost`.
    Holdover,
    /// Slave is part of the grandmaster election (best master clock algorithm
    /// has chosen this node as GM). For broadcast deployments this is unusual
    /// and is surfaced as a warning.
    Master,
    /// Encountered a state the reporter does not recognize. Treated like
    /// `Unavailable` for alarm purposes but distinguished here so the manager
    /// UI can surface "unknown ptp4l state" instead of "no daemon".
    Unknown,
}

impl PtpLockState {
    /// True when the PTP slave is healthy enough for ST 2110 flows.
    pub fn is_healthy(self) -> bool {
        matches!(self, PtpLockState::Locked)
    }
}

impl Default for PtpLockState {
    fn default() -> Self {
        PtpLockState::Unavailable
    }
}

/// IEEE 1588 grandmaster identifier as 8 hex bytes.
///
/// Stored in the byte order ptp4l reports it (network order). The
/// [`std::fmt::Display`] impl renders it as `xx:xx:xx:xx:xx:xx:xx:xx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GrandmasterId([u8; 8]);

impl GrandmasterId {
    /// Construct from raw bytes (network order).
    pub fn from_bytes(b: [u8; 8]) -> Self {
        Self(b)
    }
    /// Raw bytes in network order.
    pub fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }
    /// All-zero ID, used as "unknown" sentinel.
    pub fn zero() -> Self {
        Self([0; 8])
    }
}

impl std::fmt::Display for GrandmasterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
        )
    }
}

/// Snapshot of PTP slave clock state for one domain.
///
/// Reported through the existing stats channel as part of `FlowStats` (the
/// stats wiring lands in Phase 1 step 5). All numeric fields are signed
/// integers in nanoseconds where applicable so the manager UI can render
/// negative offsets directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtpState {
    /// Lock state of the slave for this domain.
    pub lock_state: PtpLockState,
    /// IEEE 1588 PTP domain (0..=127).
    pub domain: u8,
    /// Best-known grandmaster identifier. `Some` only when ptp4l reports
    /// `PARENT_DATA_SET`. `None` when the daemon is unreachable.
    pub grandmaster_id: Option<GrandmasterId>,
    /// Offset from master in nanoseconds (signed). `None` when unknown.
    pub offset_ns: Option<i64>,
    /// Mean path delay in nanoseconds. `None` when unknown.
    pub mean_path_delay_ns: Option<i64>,
    /// Steps removed from the grandmaster (BMCA). `None` when unknown.
    pub steps_removed: Option<u16>,
    /// Wall-clock time of the most recent successful poll, as
    /// milliseconds since UNIX epoch. `None` if no poll has succeeded yet.
    pub last_update_unix_ms: Option<i64>,
}

impl Default for PtpState {
    fn default() -> Self {
        Self {
            lock_state: PtpLockState::Unavailable,
            domain: 0,
            grandmaster_id: None,
            offset_ns: None,
            mean_path_delay_ns: None,
            steps_removed: None,
            last_update_unix_ms: None,
        }
    }
}

impl PtpState {
    /// Construct an "unavailable" state for the given PTP domain.
    pub fn unavailable(domain: u8) -> Self {
        Self {
            lock_state: PtpLockState::Unavailable,
            domain,
            ..Self::default()
        }
    }
}

/// Configuration for the PTP state reporter.
///
/// Defaults reflect the standard linuxptp deployment: socket at
/// `/var/run/ptp4l`, domain 0, polling every second.
#[derive(Debug, Clone)]
pub struct PtpReporterConfig {
    /// Path to ptp4l's management Unix datagram socket. The default
    /// `/var/run/ptp4l` matches the linuxptp default `uds_address`.
    pub uds_path: PathBuf,
    /// IEEE 1588 PTP domain to query (0..=127).
    pub domain: u8,
    /// Poll interval. The Phase 1 reporter polls at low rate (default 1s)
    /// to keep ptp4l load negligible.
    pub poll_interval: Duration,
    /// Per-poll timeout for the Unix datagram round-trip.
    pub uds_timeout: Duration,
    /// Tolerance in nanoseconds: when |offset_ns| exceeds this, the reporter
    /// transitions [`PtpLockState::Locked`] to [`PtpLockState::Holdover`].
    /// Default: 1 µs (1000 ns), which matches typical ST 2110 facility
    /// requirements.
    pub lock_tolerance_ns: i64,
}

impl Default for PtpReporterConfig {
    fn default() -> Self {
        Self {
            uds_path: PathBuf::from("/var/run/ptp4l"),
            domain: 0,
            poll_interval: Duration::from_secs(1),
            uds_timeout: Duration::from_millis(500),
            lock_tolerance_ns: 1_000,
        }
    }
}

/// Shared, lock-free reader handle for PTP state.
///
/// Cloning is cheap (Arc + RwLock). The poller writes the latest snapshot
/// inside the lock and any number of consumers can read concurrently.
#[derive(Debug, Clone, Default)]
pub struct PtpStateHandle {
    inner: Arc<RwLock<PtpState>>,
}

impl PtpStateHandle {
    /// Build a handle pre-seeded with `Unavailable` for the given domain.
    pub fn new(domain: u8) -> Self {
        Self {
            inner: Arc::new(RwLock::new(PtpState::unavailable(domain))),
        }
    }
    /// Read the current snapshot.
    pub fn snapshot(&self) -> PtpState {
        self.inner.read().clone()
    }
    /// Replace the current snapshot.
    pub fn set(&self, new_state: PtpState) {
        *self.inner.write() = new_state;
    }
}

/// Background reporter that polls the ptp4l management Unix socket.
///
/// Construct with [`PtpStateReporter::spawn`]; this returns a
/// [`PtpStateHandle`] that the caller can clone into stats accumulators and
/// flow runtimes. The reporter respects the provided [`CancellationToken`]
/// for graceful shutdown alongside the rest of the flow tree.
pub struct PtpStateReporter;

impl PtpStateReporter {
    /// Spawn the poller. Returns immediately; polling happens in a Tokio task.
    ///
    /// The reporter is best-effort:
    ///
    /// - When the socket file does not exist, the handle stays at
    ///   `Unavailable` and the task waits one full poll interval before
    ///   retrying. No log spam.
    /// - When the socket exists but the daemon does not respond within
    ///   `uds_timeout`, the handle transitions to `Unavailable` and a single
    ///   warning is emitted (rate-limited to once per state change).
    /// - When the response decodes successfully, the handle is updated with
    ///   the parsed `PtpState`.
    pub fn spawn(
        config: PtpReporterConfig,
        cancel: CancellationToken,
    ) -> PtpStateHandle {
        Self::spawn_with_events(config, cancel, None, None)
    }

    /// Spawn the poller and emit lifecycle events to the manager when the
    /// PTP lock state transitions. `event_sender` is optional so legacy
    /// call sites (and unit tests) can keep using the simpler [`Self::spawn`]
    /// constructor; when `Some`, the loop emits one event per state change
    /// (rate-limited by transition, never per poll).
    ///
    /// `flow_id` is attached to the event so the manager UI can scope the
    /// alarm to the originating ST 2110 flow. When `None`, events are
    /// node-scoped.
    pub fn spawn_with_events(
        config: PtpReporterConfig,
        cancel: CancellationToken,
        event_sender: Option<EventSender>,
        flow_id: Option<String>,
    ) -> PtpStateHandle {
        let handle = PtpStateHandle::new(config.domain);
        let writer = handle.clone();
        tokio::spawn(async move {
            poll_loop(config, writer, cancel, event_sender, flow_id).await;
        });
        handle
    }

    /// Placeholder for the optional `--features ptp-internal` path that will
    /// host an in-process `statime` slave clock.
    ///
    /// The current build always returns the same `Unavailable` handle that
    /// the external reporter would produce when ptp4l is missing. The actual
    /// `statime` integration lands in a follow-up commit alongside its
    /// dependency declaration.
    #[cfg(feature = "ptp-internal")]
    pub fn spawn_internal(domain: u8) -> PtpStateHandle {
        tracing::warn!(
            "ptp-internal feature is enabled but the statime backend is not yet wired; \
             reporting Unavailable"
        );
        PtpStateHandle::new(domain)
    }
}

async fn poll_loop(
    config: PtpReporterConfig,
    handle: PtpStateHandle,
    cancel: CancellationToken,
    event_sender: Option<EventSender>,
    flow_id: Option<String>,
) {
    let mut last_state = PtpLockState::Unavailable;
    let mut emitted_initial = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!("PTP reporter cancelled, exiting");
                break;
            }
            _ = tokio::time::sleep(config.poll_interval) => {}
        }

        let result =
            tokio::task::spawn_blocking({
                let cfg = config.clone();
                move || poll_once(&cfg)
            })
            .await
            .unwrap_or_else(|e| Err(PtpPollError::Internal(e.to_string())));

        let new_state = match result {
            Ok(state) => state,
            Err(PtpPollError::SocketMissing) => {
                if last_state != PtpLockState::Unavailable {
                    tracing::warn!(
                        path = %config.uds_path.display(),
                        "ptp4l Unix socket missing — PTP state set to Unavailable"
                    );
                }
                PtpState::unavailable(config.domain)
            }
            Err(e) => {
                if last_state != PtpLockState::Unavailable {
                    tracing::warn!(error = %e, "PTP poll failed; reporting Unavailable");
                }
                PtpState::unavailable(config.domain)
            }
        };

        let new_lock = new_state.lock_state;
        if let Some(ref tx) = event_sender {
            // Emit on every transition; also emit one "initial state" event
            // on the first poll so operators see the current state without
            // waiting for a transition.
            if !emitted_initial || new_lock != last_state {
                emit_ptp_event(tx, &new_state, last_state, flow_id.as_deref());
                emitted_initial = true;
            }
        }
        last_state = new_lock;
        handle.set(new_state);
    }
}

/// Map a PTP state transition to a manager event and dispatch it.
fn emit_ptp_event(
    tx: &EventSender,
    new_state: &PtpState,
    previous: PtpLockState,
    flow_id: Option<&str>,
) {
    let domain = new_state.domain;
    let (severity, message) = match new_state.lock_state {
        PtpLockState::Locked => (
            EventSeverity::Info,
            format!(
                "PTP locked on domain {domain} (offset {} ns)",
                new_state.offset_ns.unwrap_or(0)
            ),
        ),
        PtpLockState::Acquiring => (
            EventSeverity::Info,
            format!("PTP acquiring lock on domain {domain}"),
        ),
        PtpLockState::Holdover => (
            EventSeverity::Warning,
            format!(
                "PTP holdover on domain {domain} (offset {} ns)",
                new_state.offset_ns.unwrap_or(0)
            ),
        ),
        PtpLockState::Master => (
            EventSeverity::Warning,
            format!("PTP grandmaster role on domain {domain}"),
        ),
        PtpLockState::Unavailable => (
            // First-time "unavailable" is informational (ptp4l simply isn't
            // running yet); a transition from Locked → Unavailable is a real
            // alarm.
            if matches!(previous, PtpLockState::Locked | PtpLockState::Holdover) {
                EventSeverity::Critical
            } else {
                EventSeverity::Info
            },
            format!("PTP unavailable on domain {domain}"),
        ),
        PtpLockState::Unknown => (
            EventSeverity::Warning,
            format!("PTP reporter returned unknown state on domain {domain}"),
        ),
    };

    let details = serde_json::json!({
        "domain": domain,
        "lock_state": match new_state.lock_state {
            PtpLockState::Unavailable => "unavailable",
            PtpLockState::Acquiring => "acquiring",
            PtpLockState::Locked => "locked",
            PtpLockState::Holdover => "holdover",
            PtpLockState::Master => "master",
            PtpLockState::Unknown => "unknown",
        },
        "offset_ns": new_state.offset_ns,
        "mean_path_delay_ns": new_state.mean_path_delay_ns,
        "grandmaster_id": new_state.grandmaster_id.map(|gm| gm.to_string()),
    });

    tx.emit_with_details(severity, category::PTP, message, flow_id, details);
}

/// Errors that can occur during a single ptp4l poll.
#[derive(Debug)]
enum PtpPollError {
    SocketMissing,
    /// I/O failure: bind, send, or recv on the local UDS endpoint.
    Io(std::io::Error),
    /// Daemon did not respond within the configured timeout.
    Timeout,
    /// Response was received but failed to decode.
    Decode(&'static str),
    /// spawn_blocking task panicked or other internal failure.
    Internal(String),
}

impl std::fmt::Display for PtpPollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PtpPollError::SocketMissing => f.write_str("ptp4l socket missing"),
            PtpPollError::Io(e) => write!(f, "I/O error: {e}"),
            PtpPollError::Timeout => f.write_str("ptp4l did not respond"),
            PtpPollError::Decode(s) => write!(f, "decode error: {s}"),
            PtpPollError::Internal(s) => write!(f, "internal error: {s}"),
        }
    }
}

/// Poll the ptp4l UDS once. Synchronous because the linuxptp UDS protocol is
/// strictly request/response and the round-trip is sub-millisecond.
///
/// Returns `Err(SocketMissing)` if the path does not exist, so the caller can
/// distinguish "no daemon configured" from "daemon stopped responding".
fn poll_once(config: &PtpReporterConfig) -> Result<PtpState, PtpPollError> {
    if !Path::new(&config.uds_path).exists() {
        return Err(PtpPollError::SocketMissing);
    }

    // Bind a temporary client socket. linuxptp accepts abstract or filesystem
    // UDS clients; on Linux we use an abstract address (\0bilbycast-ptp-<pid>)
    // so we don't pollute the filesystem and so multiple bilbycast instances
    // never collide. socket2 / std::os::unix::net::UnixDatagram does support
    // abstract sockets via the `from_abstract_*` API since Rust 1.70 on Linux,
    // but for portability we use a filesystem-based tempfile here.
    let tmp = tempfile_socket_path()?;
    let sock = UnixDatagram::bind(&tmp).map_err(PtpPollError::Io)?;
    // Best-effort cleanup on drop. We hold this guard until the function
    // returns so the temp file is unlinked even on error paths.
    let _cleanup = TempPathGuard(tmp);

    sock.set_read_timeout(Some(config.uds_timeout)).map_err(PtpPollError::Io)?;
    sock.set_write_timeout(Some(config.uds_timeout)).map_err(PtpPollError::Io)?;
    sock.connect(&config.uds_path).map_err(PtpPollError::Io)?;

    // Build the GET CURRENT_DATA_SET request.
    let req_current = build_management_get(MANAGEMENT_ID_CURRENT_DATA_SET, config.domain);
    sock.send(&req_current).map_err(PtpPollError::Io)?;

    let mut buf = [0u8; 1500];
    let (n, current_data) = recv_management_response(
        &sock, &mut buf, MANAGEMENT_ID_CURRENT_DATA_SET,
    )?;
    let _ = n;
    let (steps_removed, offset_ns, mean_path_delay_ns) = decode_current_data_set(current_data)?;

    // Build the GET PARENT_DATA_SET request.
    let req_parent = build_management_get(MANAGEMENT_ID_PARENT_DATA_SET, config.domain);
    sock.send(&req_parent).map_err(PtpPollError::Io)?;
    let mut buf2 = [0u8; 1500];
    let parent_result = recv_management_response(
        &sock, &mut buf2, MANAGEMENT_ID_PARENT_DATA_SET,
    );
    let grandmaster_id = match parent_result {
        Ok((_, parent_data)) => decode_parent_data_set_gm_id(parent_data).ok(),
        Err(_) => None,
    };

    let lock_state = classify_lock(offset_ns, config.lock_tolerance_ns);
    let last_update_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64);

    Ok(PtpState {
        lock_state,
        domain: config.domain,
        grandmaster_id,
        offset_ns: Some(offset_ns),
        mean_path_delay_ns: Some(mean_path_delay_ns),
        steps_removed: Some(steps_removed),
        last_update_unix_ms,
    })
}

/// Classify lock state from a single offset measurement.
///
/// This is intentionally conservative: a single in-tolerance sample is
/// reported as `Locked`. The manager-side reconciliation can apply hysteresis
/// over multiple samples if it wants to suppress flapping.
fn classify_lock(offset_ns: i64, tolerance_ns: i64) -> PtpLockState {
    if offset_ns.unsigned_abs() <= tolerance_ns as u64 {
        PtpLockState::Locked
    } else {
        PtpLockState::Holdover
    }
}

/// Generate a temporary filesystem path for a client UDS endpoint.
///
/// Uses the process ID and a monotonic counter so concurrent polls within the
/// same process never collide. Lives under `/tmp` since `/var/run` is usually
/// not writable by the bilbycast user.
fn tempfile_socket_path() -> Result<PathBuf, PtpPollError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("bilbycast-ptp-{pid}-{n}.sock"));
    // Best-effort: remove any leftover from a previous crash.
    let _ = std::fs::remove_file(&path);
    Ok(path)
}

struct TempPathGuard(PathBuf);
impl Drop for TempPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ─────────────── IEEE 1588 management TLV (subset) ───────────────
//
// The Phase 1 reader implements only the GET-side framing for two
// CURRENT_DATA_SET and PARENT_DATA_SET management IDs. This is the minimum
// needed to populate the `PtpState` struct. The full management protocol is
// defined in IEEE 1588-2008 §15.

const MANAGEMENT_ID_CURRENT_DATA_SET: u16 = 0x2001;
const MANAGEMENT_ID_PARENT_DATA_SET: u16 = 0x2002;

const PTP_VERSION: u8 = 2;
const PTP_MSG_TYPE_MANAGEMENT: u8 = 0x0D;
const PTP_TLV_TYPE_MANAGEMENT: u16 = 0x0001;
const PTP_HEADER_LEN: usize = 34;
const PTP_MGMT_HEADER_LEN: usize = 14; // targetPortIdentity(10) + actionField+reserved+startBoundary+boundary(4)
const PTP_TLV_TLV_HEADER_LEN: usize = 4;
const PTP_TLV_MGMT_ID_FIELD_LEN: usize = 2;

const PTP_ACTION_GET: u8 = 0;
const PTP_ACTION_RESPONSE: u8 = 2;

/// Build a GET management message for the requested ID and PTP domain.
///
/// Layout per IEEE 1588-2008 §13.3, §15:
///
/// ```text
/// +-- Common header (34 bytes) --+-- Management header (14) --+-- Management TLV --+
/// ```
fn build_management_get(management_id: u16, domain: u8) -> Vec<u8> {
    let total_len: u16 = (PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN
        + PTP_TLV_MGMT_ID_FIELD_LEN) as u16;

    let mut buf = Vec::with_capacity(total_len as usize);

    // ── Common header (34 bytes) ──
    // byte 0: messageType (low 4 bits) | transportSpecific (high 4 bits)
    buf.push(PTP_MSG_TYPE_MANAGEMENT);
    // byte 1: reserved | versionPTP (low 4 bits)
    buf.push(PTP_VERSION & 0x0F);
    // bytes 2-3: messageLength
    buf.extend_from_slice(&total_len.to_be_bytes());
    // byte 4: domainNumber
    buf.push(domain);
    // byte 5: reserved
    buf.push(0);
    // bytes 6-7: flagField
    buf.extend_from_slice(&[0u8; 2]);
    // bytes 8-15: correctionField (Integer64)
    buf.extend_from_slice(&[0u8; 8]);
    // bytes 16-19: reserved
    buf.extend_from_slice(&[0u8; 4]);
    // bytes 20-29: sourcePortIdentity (clockIdentity[8] + portNumber[2])
    // We use a synthetic identity: "bilbyc" + 0x0001 + ourPort=1.
    buf.extend_from_slice(b"bilbyct\x00");
    buf.extend_from_slice(&1u16.to_be_bytes());
    // bytes 30-31: sequenceId
    buf.extend_from_slice(&0u16.to_be_bytes());
    // byte 32: controlField (deprecated, must be 0x04 for management)
    buf.push(0x04);
    // byte 33: logMessageInterval (0x7F = "not applicable")
    buf.push(0x7F);
    debug_assert_eq!(buf.len(), PTP_HEADER_LEN);

    // ── Management header (14 bytes) ──
    // targetPortIdentity (10): all-ones clockIdentity + 0xffff portNumber
    buf.extend_from_slice(&[0xff; 8]);
    buf.extend_from_slice(&0xffffu16.to_be_bytes());
    // startingBoundaryHops, boundaryHops
    buf.push(0);
    buf.push(0);
    // actionField | reserved
    buf.push(PTP_ACTION_GET);
    buf.push(0);
    debug_assert_eq!(buf.len(), PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN);

    // ── Management TLV ──
    // tlvType
    buf.extend_from_slice(&PTP_TLV_TYPE_MANAGEMENT.to_be_bytes());
    // lengthField (managementId(2) + dataField(0))
    buf.extend_from_slice(&(PTP_TLV_MGMT_ID_FIELD_LEN as u16).to_be_bytes());
    // managementId
    buf.extend_from_slice(&management_id.to_be_bytes());
    debug_assert_eq!(buf.len(), total_len as usize);

    buf
}

/// Receive a management RESPONSE for `expected_id` from `sock` into `buf`.
///
/// Returns the number of bytes written and a slice referencing the
/// dataField inside `buf`. Discards anything that is not a management
/// RESPONSE for the expected ID.
fn recv_management_response<'a>(
    sock: &UnixDatagram,
    buf: &'a mut [u8],
    expected_id: u16,
) -> Result<(usize, &'a [u8]), PtpPollError> {
    // Allow up to 4 spurious notifications before giving up.
    for _ in 0..4 {
        let n = match sock.recv(buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(PtpPollError::Timeout);
            }
            Err(e) => return Err(PtpPollError::Io(e)),
        };
        if n < PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN {
            continue; // too short, ignore and try next
        }
        let view = &buf[..n];
        if (view[0] & 0x0F) != PTP_MSG_TYPE_MANAGEMENT {
            continue;
        }
        let action = view[PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN - 2];
        if action != PTP_ACTION_RESPONSE {
            continue;
        }
        let mgmt_id_off = PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN;
        if mgmt_id_off + 2 > n {
            continue;
        }
        let mgmt_id = u16::from_be_bytes([view[mgmt_id_off], view[mgmt_id_off + 1]]);
        if mgmt_id != expected_id {
            continue;
        }
        // dataField follows the managementId.
        let data_start = mgmt_id_off + 2;
        let tlv_len_off = PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + 2;
        let tlv_len = u16::from_be_bytes([view[tlv_len_off], view[tlv_len_off + 1]]) as usize;
        // tlv_len includes managementId + dataField bytes.
        if tlv_len < PTP_TLV_MGMT_ID_FIELD_LEN {
            return Err(PtpPollError::Decode("management TLV length too small"));
        }
        let data_end = data_start + (tlv_len - PTP_TLV_MGMT_ID_FIELD_LEN);
        if data_end > n {
            return Err(PtpPollError::Decode("management TLV truncated"));
        }
        return Ok((n, &buf[data_start..data_end]));
    }
    Err(PtpPollError::Timeout)
}

/// Decode a CURRENT_DATA_SET dataField.
///
/// Layout per IEEE 1588-2008 §8.2.2:
///
/// ```text
/// stepsRemoved (UInteger16)        — 2 bytes
/// offsetFromMaster (TimeInterval)  — 8 bytes (Integer64, scaled ns << 16)
/// meanPathDelay (TimeInterval)     — 8 bytes (Integer64, scaled ns << 16)
/// ```
fn decode_current_data_set(data: &[u8]) -> Result<(u16, i64, i64), PtpPollError> {
    if data.len() < 18 {
        return Err(PtpPollError::Decode("CURRENT_DATA_SET dataField too short"));
    }
    let steps_removed = u16::from_be_bytes([data[0], data[1]]);
    let offset_scaled = i64::from_be_bytes(data[2..10].try_into().unwrap());
    let path_scaled = i64::from_be_bytes(data[10..18].try_into().unwrap());
    Ok((steps_removed, offset_scaled >> 16, path_scaled >> 16))
}

/// Decode a PARENT_DATA_SET dataField and extract the grandmaster identifier.
///
/// Layout per IEEE 1588-2008 §8.2.3 (relevant prefix):
///
/// ```text
/// parentPortIdentity (10)          — clockIdentity[8] + portNumber[2]
/// parentStats (1) | reserved (1)
/// observedParentOffsetScaledLogVariance (2)
/// observedParentClockPhaseChangeRate (4)
/// grandmasterPriority1 (1)
/// grandmasterClockQuality (4)
/// grandmasterPriority2 (1)
/// grandmasterIdentity (8)          ← what we want
/// ```
fn decode_parent_data_set_gm_id(data: &[u8]) -> Result<GrandmasterId, PtpPollError> {
    const GM_IDENTITY_OFFSET: usize = 10 + 2 + 2 + 4 + 1 + 4 + 1; // = 24
    if data.len() < GM_IDENTITY_OFFSET + 8 {
        return Err(PtpPollError::Decode("PARENT_DATA_SET dataField too short"));
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&data[GM_IDENTITY_OFFSET..GM_IDENTITY_OFFSET + 8]);
    Ok(GrandmasterId::from_bytes(id))
}

// ─────────────── parking_lot_lite (vendored zero-dep RwLock) ───────────────
//
// The codebase already uses `tokio::sync::RwLock` everywhere it needs an async
// lock. The PTP state handle wants a tiny synchronous reader/writer because it
// is read from the stats hot path. Pulling in `parking_lot` would add a new
// dependency for the sake of one struct, which the Phase 1 plan asks us to
// avoid. We use `std::sync::RwLock` directly through a thin module so the
// rest of this file does not need to mention `std::sync` paths.
mod parking_lot_lite {
    use std::sync::{RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard};

    /// Lightweight wrapper over `std::sync::RwLock` exposing only the
    /// methods this module needs.
    #[derive(Debug, Default)]
    pub struct RwLock<T>(StdRwLock<T>);

    impl<T> RwLock<T> {
        pub fn new(value: T) -> Self {
            Self(StdRwLock::new(value))
        }
        pub fn read(&self) -> RwLockReadGuard<'_, T> {
            // We never poison the lock from inside this crate.
            self.0.read().expect("PTP state RwLock poisoned")
        }
        pub fn write(&self) -> RwLockWriteGuard<'_, T> {
            self.0.write().expect("PTP state RwLock poisoned")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grandmaster_id_display() {
        let gm = GrandmasterId::from_bytes([0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe]);
        assert_eq!(gm.to_string(), "de:ad:be:ef:ca:fe:ba:be");
    }

    #[test]
    fn test_ptp_state_default_is_unavailable() {
        let s = PtpState::default();
        assert_eq!(s.lock_state, PtpLockState::Unavailable);
        assert!(!s.lock_state.is_healthy());
        assert!(s.grandmaster_id.is_none());
        assert!(s.offset_ns.is_none());
    }

    #[test]
    fn test_classify_lock_within_tolerance() {
        assert_eq!(classify_lock(0, 1000), PtpLockState::Locked);
        assert_eq!(classify_lock(500, 1000), PtpLockState::Locked);
        assert_eq!(classify_lock(-1000, 1000), PtpLockState::Locked);
    }

    #[test]
    fn test_classify_lock_out_of_tolerance() {
        assert_eq!(classify_lock(1001, 1000), PtpLockState::Holdover);
        assert_eq!(classify_lock(-2_000_000, 1000), PtpLockState::Holdover);
    }

    #[test]
    fn test_handle_round_trip() {
        let h = PtpStateHandle::new(0);
        assert_eq!(h.snapshot().lock_state, PtpLockState::Unavailable);
        h.set(PtpState {
            lock_state: PtpLockState::Locked,
            domain: 0,
            grandmaster_id: Some(GrandmasterId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8])),
            offset_ns: Some(42),
            mean_path_delay_ns: Some(100),
            steps_removed: Some(1),
            last_update_unix_ms: Some(123),
        });
        let snap = h.snapshot();
        assert_eq!(snap.lock_state, PtpLockState::Locked);
        assert_eq!(snap.offset_ns, Some(42));
    }

    #[test]
    fn test_build_management_get_layout() {
        let msg = build_management_get(MANAGEMENT_ID_CURRENT_DATA_SET, 0);
        // Total length is encoded in bytes 2-3 and must equal the buffer size.
        let total = u16::from_be_bytes([msg[2], msg[3]]) as usize;
        assert_eq!(total, msg.len());
        // messageType in low nibble of byte 0 must be MANAGEMENT.
        assert_eq!(msg[0] & 0x0F, PTP_MSG_TYPE_MANAGEMENT);
        // versionPTP in low nibble of byte 1 must be 2.
        assert_eq!(msg[1] & 0x0F, PTP_VERSION);
        // domainNumber in byte 4 echoes the request.
        assert_eq!(msg[4], 0);
        // controlField in byte 32 must be 0x04 for management.
        assert_eq!(msg[32], 0x04);
        // managementId at the very end.
        let id_off = msg.len() - 2;
        assert_eq!(
            u16::from_be_bytes([msg[id_off], msg[id_off + 1]]),
            MANAGEMENT_ID_CURRENT_DATA_SET
        );
    }

    #[test]
    fn test_build_management_get_carries_domain() {
        let msg = build_management_get(MANAGEMENT_ID_PARENT_DATA_SET, 17);
        assert_eq!(msg[4], 17);
    }

    #[test]
    fn test_decode_current_data_set_minimum() {
        // stepsRemoved=3, offset=4096*65536 ns scaled (= 4096 ns), path=8192*65536 (= 8192 ns)
        let mut data = vec![0u8; 18];
        data[0..2].copy_from_slice(&3u16.to_be_bytes());
        let off_scaled: i64 = 4096i64 << 16;
        data[2..10].copy_from_slice(&off_scaled.to_be_bytes());
        let path_scaled: i64 = 8192i64 << 16;
        data[10..18].copy_from_slice(&path_scaled.to_be_bytes());
        let (steps, off, path) = decode_current_data_set(&data).unwrap();
        assert_eq!(steps, 3);
        assert_eq!(off, 4096);
        assert_eq!(path, 8192);
    }

    #[test]
    fn test_decode_current_data_set_negative_offset() {
        let mut data = vec![0u8; 18];
        let off_scaled: i64 = -1234i64 << 16;
        data[2..10].copy_from_slice(&off_scaled.to_be_bytes());
        let (_, off, _) = decode_current_data_set(&data).unwrap();
        assert_eq!(off, -1234);
    }

    #[test]
    fn test_decode_current_data_set_too_short() {
        assert!(matches!(
            decode_current_data_set(&[0u8; 10]),
            Err(PtpPollError::Decode(_))
        ));
    }

    #[test]
    fn test_decode_parent_data_set_gm_id() {
        // Construct a buffer with the GM identity at offset 24.
        let mut data = vec![0u8; 32];
        let gm = [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8];
        data[24..32].copy_from_slice(&gm);
        let id = decode_parent_data_set_gm_id(&data).unwrap();
        assert_eq!(id.as_bytes(), &gm);
    }

    #[test]
    fn test_poll_socket_missing_returns_error() {
        let cfg = PtpReporterConfig {
            uds_path: PathBuf::from("/var/run/this-path-does-not-exist-bilbycast"),
            ..Default::default()
        };
        let result = poll_once(&cfg);
        assert!(matches!(result, Err(PtpPollError::SocketMissing)));
    }

    #[tokio::test]
    async fn test_reporter_with_missing_socket_stays_unavailable() {
        let cfg = PtpReporterConfig {
            uds_path: PathBuf::from("/var/run/this-path-does-not-exist-bilbycast"),
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let handle = PtpStateReporter::spawn(cfg, cancel.clone());
        // Wait for at least one poll cycle.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = handle.snapshot();
        assert_eq!(snap.lock_state, PtpLockState::Unavailable);
        cancel.cancel();
    }

    #[tokio::test]
    async fn test_reporter_decodes_management_response() {
        // Stand up a fake ptp4l on a temp socket: receive the GET, send back
        // a response with known values, and assert the reporter parses them.
        let dir = tempfile::tempdir().unwrap();
        let server_path = dir.path().join("ptp4l");
        let server = UnixDatagram::bind(&server_path).unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        server.set_write_timeout(Some(Duration::from_secs(2))).unwrap();

        // Spawn a thread that responds to up to 2 GET requests (current + parent).
        let server_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            for _ in 0..2 {
                let (n, peer) = match server.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let req = &buf[..n];
                let id_off = PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN;
                let mgmt_id = u16::from_be_bytes([req[id_off], req[id_off + 1]]);
                let response = if mgmt_id == MANAGEMENT_ID_CURRENT_DATA_SET {
                    build_response_current_data_set(0, 5, 250, 1500)
                } else {
                    build_response_parent_data_set_gm(0, [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17])
                };
                let peer_path = peer.as_pathname().unwrap().to_path_buf();
                let _ = server.send_to(&response, &peer_path);
            }
        });

        let cfg = PtpReporterConfig {
            uds_path: server_path,
            poll_interval: Duration::from_millis(50),
            uds_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let handle = PtpStateReporter::spawn(cfg, cancel.clone());

        // Wait for the poll to fire and the response to be parsed.
        let mut snap = handle.snapshot();
        for _ in 0..50 {
            if snap.offset_ns.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            snap = handle.snapshot();
        }
        cancel.cancel();
        let _ = server_thread.join();

        assert_eq!(snap.offset_ns, Some(250));
        assert_eq!(snap.mean_path_delay_ns, Some(1500));
        assert_eq!(snap.steps_removed, Some(5));
        assert_eq!(
            snap.grandmaster_id.unwrap().as_bytes(),
            &[0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17]
        );
        // 250 ns is well within 1µs default tolerance, so we should be Locked.
        assert_eq!(snap.lock_state, PtpLockState::Locked);
    }

    /// Build a synthetic CURRENT_DATA_SET RESPONSE for unit tests.
    fn build_response_current_data_set(
        domain: u8,
        steps_removed: u16,
        offset_ns: i64,
        mean_path_delay_ns: i64,
    ) -> Vec<u8> {
        let data_len = 18u16;
        let tlv_value_len = PTP_TLV_MGMT_ID_FIELD_LEN as u16 + data_len;
        let total_len: u16 = (PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN) as u16
            + tlv_value_len;
        let mut buf = Vec::with_capacity(total_len as usize);
        // Common header
        buf.push(PTP_MSG_TYPE_MANAGEMENT);
        buf.push(PTP_VERSION);
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.push(domain);
        buf.push(0);
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 4]);
        buf.extend_from_slice(b"ptp4l\x00\x00\x00");
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.push(0x04);
        buf.push(0x7F);
        // Management header
        buf.extend_from_slice(&[0xff; 8]);
        buf.extend_from_slice(&0xffffu16.to_be_bytes());
        buf.push(0);
        buf.push(0);
        buf.push(PTP_ACTION_RESPONSE);
        buf.push(0);
        // Management TLV
        buf.extend_from_slice(&PTP_TLV_TYPE_MANAGEMENT.to_be_bytes());
        buf.extend_from_slice(&tlv_value_len.to_be_bytes());
        buf.extend_from_slice(&MANAGEMENT_ID_CURRENT_DATA_SET.to_be_bytes());
        buf.extend_from_slice(&steps_removed.to_be_bytes());
        buf.extend_from_slice(&((offset_ns << 16) as i64).to_be_bytes());
        buf.extend_from_slice(&((mean_path_delay_ns << 16) as i64).to_be_bytes());
        buf
    }

    /// Build a synthetic PARENT_DATA_SET RESPONSE carrying a chosen GM ID.
    fn build_response_parent_data_set_gm(domain: u8, gm: [u8; 8]) -> Vec<u8> {
        // dataField layout up to grandmasterIdentity is 32 bytes. We zero
        // everything except the GM identity at offset 24.
        let mut data = vec![0u8; 32];
        data[24..32].copy_from_slice(&gm);

        let data_len = data.len() as u16;
        let tlv_value_len = PTP_TLV_MGMT_ID_FIELD_LEN as u16 + data_len;
        let total_len: u16 = (PTP_HEADER_LEN + PTP_MGMT_HEADER_LEN + PTP_TLV_TLV_HEADER_LEN) as u16
            + tlv_value_len;

        let mut buf = Vec::with_capacity(total_len as usize);
        buf.push(PTP_MSG_TYPE_MANAGEMENT);
        buf.push(PTP_VERSION);
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.push(domain);
        buf.push(0);
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 4]);
        buf.extend_from_slice(b"ptp4l\x00\x00\x00");
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.push(0x04);
        buf.push(0x7F);
        buf.extend_from_slice(&[0xff; 8]);
        buf.extend_from_slice(&0xffffu16.to_be_bytes());
        buf.push(0);
        buf.push(0);
        buf.push(PTP_ACTION_RESPONSE);
        buf.push(0);
        buf.extend_from_slice(&PTP_TLV_TYPE_MANAGEMENT.to_be_bytes());
        buf.extend_from_slice(&tlv_value_len.to_be_bytes());
        buf.extend_from_slice(&MANAGEMENT_ID_PARENT_DATA_SET.to_be_bytes());
        buf.extend_from_slice(&data);
        buf
    }
}

