// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pre-bus Hitless merger for PID-bus `SlotSource::Hitless`.
//!
//! The merger sits between the [`FlowEsBus`] and the assembler: it
//! subscribes to two source bus keys (a `primary` and a `backup`) and
//! republishes onto a third synthetic key that the assembler's slot
//! points at. The assembler itself stays PID-level — zero changes to
//! `run_assembler`.
//!
//! # Dedup strategy — Phase 7 first-light
//!
//! Primary-preference failover with a configurable stall timer.
//! Primary packets are forwarded verbatim. If no primary packet arrives
//! for `stall_ms` (default 200 ms) the merger flips to backup. When
//! primary traffic resumes the merger switches back after a brief
//! hold-off to avoid flapping on bursty feeds.
//!
//! This is **not** SMPTE 2022-7 sequence-aware dedup — [`EsPacket`]
//! carries no upstream RTP sequence number today. Seq-aware dedup is a
//! follow-up that requires augmenting the bus shape. Failover semantics
//! are the common operator intent for "hot standby" and robust without
//! seq plumbing.
//!
//! # Invariants
//!
//! - No blocking on the data path. The merger task is a plain
//!   `tokio::select!` loop feeding a bus `broadcast::Sender`.
//! - Bus semantics: if downstream consumers lag, `broadcast` drops at
//!   the bus edge — same as every other bus publisher.
//! - Cancellation propagates from the parent flow token via
//!   `CancellationToken::child_token`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;

use super::ts_es_bus::FlowEsBus;

/// Default stall timer: if no primary packet arrives within this
/// window the merger flips to backup. Tight enough that standby cuts
/// over within one video frame at 25 fps (40 ms per frame × 5 frames)
/// but loose enough that bursty ingress jitter doesn't flap.
pub const DEFAULT_STALL_MS: u64 = 200;

/// Namespace for synthetic bus keys used by Hitless slots. The merger
/// publishes onto `(format!("{HITLESS_INPUT_PREFIX}{uid}"), 0)`. The
/// assembler's slot points at the same key, so the assembler sees a
/// single stream and stays PID-level.
pub const HITLESS_INPUT_PREFIX: &str = "hitless:";

/// Build the synthetic bus key `(input_id, source_pid)` for a Hitless
/// slot. The `uid` should be unique per Hitless slot within the flow —
/// convention is `"slot_{program_idx}_{slot_idx}"` so two Hitless slots
/// on the same flow never collide.
pub fn hitless_bus_key(uid: &str) -> (String, u16) {
    (format!("{HITLESS_INPUT_PREFIX}{uid}"), 0)
}

/// Spawn a Hitless merger task for one slot.
///
/// Subscribes to `(primary_input, primary_pid)` and `(backup_input,
/// backup_pid)` on the bus, deduplicates via primary-preference with a
/// `stall` timer, and republishes onto [`hitless_bus_key`] for `uid`.
pub fn spawn_hitless_es_merger(
    uid: String,
    primary: (String, u16),
    backup: (String, u16),
    bus: Arc<FlowEsBus>,
    stall: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    spawn_hitless_es_merger_modal(uid, primary, backup, bus, stall, /* seq_aware */ false, cancel)
}

/// Spawn a Hitless merger task with explicit mode selection.
///
/// `seq_aware = false` (default) keeps the legacy primary-preference
/// stall-timer path. `seq_aware = true` runs the SMPTE 2022-7 merger
/// from `redundancy::merger::HitlessMerger` over `EsPacket.upstream_seq`,
/// which dedupes both legs continuously and gap-fills sub-frame on
/// failover.
pub fn spawn_hitless_es_merger_modal(
    uid: String,
    primary: (String, u16),
    backup: (String, u16),
    bus: Arc<FlowEsBus>,
    stall: Duration,
    seq_aware: bool,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    spawn_hitless_es_merger_full(
        uid,
        primary,
        backup,
        bus,
        stall,
        seq_aware,
        /* path_differential_ms */ None,
        cancel,
    )
}

/// Spawn a Hitless merger task. The seq-aware mode now supports a
/// path-differential buffer for true SMPTE 2022-7 compliance — set
/// `path_differential_ms` to `Some(ms)` to enable.
///
/// * `seq_aware = false`        → primary-preference stall timer (legacy)
/// * `seq_aware = true, pd None`  → bare dedup (faster, no asym-path protection)
/// * `seq_aware = true, pd Some(ms)` → buffered SMPTE 2022-7 (recommended)
pub fn spawn_hitless_es_merger_full(
    uid: String,
    primary: (String, u16),
    backup: (String, u16),
    bus: Arc<FlowEsBus>,
    stall: Duration,
    seq_aware: bool,
    path_differential_ms: Option<u32>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut primary_rx = bus.subscribe(&primary.0, primary.1);
    let mut backup_rx = bus.subscribe(&backup.0, backup.1);
    let (out_input, out_pid) = hitless_bus_key(&uid);
    let out_tx = bus.sender_for(&out_input, out_pid);

    tokio::spawn(async move {
        match (seq_aware, path_differential_ms) {
            (true, Some(ms)) => {
                run_buffered(
                    uid,
                    primary_rx,
                    backup_rx,
                    out_tx,
                    Duration::from_millis(ms as u64),
                    cancel,
                )
                .await;
            }
            (true, None) => {
                run_seq_aware(uid, primary_rx, backup_rx, out_tx, cancel).await;
            }
            (false, _) => {
                run_primary_preference(
                    uid,
                    &mut primary_rx,
                    &mut backup_rx,
                    out_tx,
                    stall,
                    cancel,
                )
                .await;
            }
        }
    })
}

async fn run_buffered(
    uid: String,
    mut primary_rx: broadcast::Receiver<EsPacketAlias>,
    mut backup_rx: broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    max_path_diff: Duration,
    cancel: CancellationToken,
) {
    use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger};
    let mut merger: BufferedHitlessMerger<EsPacketAlias> =
        BufferedHitlessMerger::new(max_path_diff);
    // Far-future placeholder used when the buffer is empty so the
    // tokio::select! `sleep_until` arm doesn't fire spuriously.
    let far_future = std::time::Instant::now() + Duration::from_secs(86_400);

    loop {
        let drain_at = merger.next_deadline().unwrap_or(far_future);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = sleep_until(tokio::time::Instant::from_std(drain_at)) => {
                for (_, es) in merger.drain_expired(std::time::Instant::now()) {
                    let _ = out_tx.send(es);
                }
            }
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => {
                            tracing::warn!(
                                uid = %uid,
                                "pid_bus hitless buffered: primary leg has no upstream_seq — \
                                 forwarding without buffer (bypass path)"
                            );
                            let _ = out_tx.send(es);
                            continue;
                        }
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest(seq, ActiveLeg::Leg1, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => continue,
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest(seq, ActiveLeg::Leg2, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

async fn run_primary_preference(
    uid: String,
    primary_rx: &mut broadcast::Receiver<EsPacketAlias>,
    backup_rx: &mut broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    stall: Duration,
    cancel: CancellationToken,
) {
    let mut using_primary = true;
    let mut last_primary_at = Instant::now();

    loop {
        let stall_deadline = last_primary_at + stall;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    last_primary_at = Instant::now();
                    if !using_primary {
                        tracing::info!(
                            uid = %uid,
                            "pid_bus hitless: primary resumed — switching from backup"
                        );
                        using_primary = true;
                    }
                    let _ = out_tx.send(es);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    if !using_primary {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = sleep_until(stall_deadline), if using_primary => {
                tracing::warn!(
                    uid = %uid,
                    "pid_bus hitless: primary stalled {:?} — switching to backup",
                    stall
                );
                using_primary = false;
            }
        }
    }
}

async fn run_seq_aware(
    uid: String,
    mut primary_rx: broadcast::Receiver<EsPacketAlias>,
    mut backup_rx: broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    cancel: CancellationToken,
) {
    use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
    let mut merger = HitlessMerger::new();
    let mut current_leg: Option<ActiveLeg> = None;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => {
                            tracing::warn!(
                                uid = %uid,
                                "pid_bus hitless seq_aware: primary leg has no upstream_seq — \
                                 falling back to forward (this should be rejected at config validation)"
                            );
                            let _ = out_tx.send(es);
                            continue;
                        }
                    };
                    if let Some(chosen) = merger.try_merge(seq, ActiveLeg::Leg1) {
                        if Some(chosen) != current_leg {
                            tracing::info!(uid = %uid, "pid_bus hitless seq_aware: leg = {:?}", chosen);
                            current_leg = Some(chosen);
                        }
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => continue,
                    };
                    if let Some(chosen) = merger.try_merge(seq, ActiveLeg::Leg2) {
                        if Some(chosen) != current_leg {
                            tracing::info!(uid = %uid, "pid_bus hitless seq_aware: leg = {:?}", chosen);
                            current_leg = Some(chosen);
                        }
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

// Type alias used in helpers above to keep the function signatures
// readable without a large lifetime ceremony — `EsPacket` is `Clone` so
// the broadcast channel of it is fine to pass through by value.
type EsPacketAlias = crate::engine::ts_es_bus::EsPacket;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_es_bus::EsPacket;
    use bytes::Bytes;

    fn es(pid: u16, byte: u8) -> EsPacket {
        let mut payload = vec![0u8; 188];
        payload[0] = 0x47;
        payload[1] = ((pid >> 8) & 0x1F) as u8;
        payload[2] = (pid & 0xFF) as u8;
        payload[3] = 0x10;
        for b in &mut payload[4..] {
            *b = byte;
        }
        EsPacket {
            source_pid: pid,
            stream_type: 0,
            payload: Bytes::from(payload),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
            upstream_leg_id: None,
        }
    }

    #[tokio::test]
    async fn primary_preference_when_primary_live() {
        let bus = Arc::new(FlowEsBus::new());
        let cancel = CancellationToken::new();
        let _handle = spawn_hitless_es_merger(
            "slot_0_0".to_string(),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(500),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
        let pri = bus.sender_for("in-a", 0x100);
        let bak = bus.sender_for("in-b", 0x200);

        // Give the merger time to subscribe.
        tokio::time::sleep(Duration::from_millis(20)).await;

        pri.send(es(0x100, 0xAA)).unwrap();
        bak.send(es(0x200, 0xBB)).unwrap();

        let first = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("merger emits within 200ms")
            .expect("channel open");
        assert_eq!(first.payload[4], 0xAA, "primary packet must be forwarded");

        // Backup packet must NOT be forwarded while primary is live.
        let second = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(second.is_err(), "backup must be suppressed while primary is live");

        cancel.cancel();
    }

    #[tokio::test]
    async fn failover_to_backup_on_stall() {
        let bus = Arc::new(FlowEsBus::new());
        let cancel = CancellationToken::new();
        let _handle = spawn_hitless_es_merger(
            "slot_0_0".to_string(),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(50),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
        let pri = bus.sender_for("in-a", 0x100);
        let bak = bus.sender_for("in-b", 0x200);
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Prime primary so the merger anchors.
        pri.send(es(0x100, 0xAA)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;

        // Stall primary past 50ms, backup should take over.
        tokio::time::sleep(Duration::from_millis(120)).await;
        bak.send(es(0x200, 0xBB)).unwrap();
        let after = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("merger emits backup packet after stall")
            .expect("channel open");
        assert_eq!(after.payload[4], 0xBB, "backup must forward after primary stall");

        cancel.cancel();
    }

    #[tokio::test]
    async fn switches_back_to_primary_on_resume() {
        let bus = Arc::new(FlowEsBus::new());
        let cancel = CancellationToken::new();
        let _handle = spawn_hitless_es_merger(
            "slot_0_0".to_string(),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(50),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
        let pri = bus.sender_for("in-a", 0x100);
        let bak = bus.sender_for("in-b", 0x200);
        tokio::time::sleep(Duration::from_millis(20)).await;

        pri.send(es(0x100, 0xAA)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;

        // Stall → backup takes over.
        tokio::time::sleep(Duration::from_millis(120)).await;
        bak.send(es(0x200, 0xBB)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await;

        // Primary resumes; next primary packet must surface and the
        // subsequent backup packet must stop flowing.
        pri.send(es(0x100, 0xCC)).unwrap();
        let pc = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("primary resume")
            .expect("channel open");
        assert_eq!(pc.payload[4], 0xCC);

        bak.send(es(0x200, 0xDD)).unwrap();
        let blocked = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(
            blocked.is_err(),
            "backup must be re-suppressed once primary resumes"
        );

        cancel.cancel();
    }
}
