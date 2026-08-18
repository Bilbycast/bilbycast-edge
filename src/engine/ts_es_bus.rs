// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Elementary-stream bus primitives for the PID-bus runtime.
//!
//! This module provides the *plumbing* that the Phase 5 assembler will
//! consume: a [`NodeEsBus`] registry keyed by `(input_id, source_pid)`,
//! a lightweight [`EsPacket`] carrying exactly one 188-byte TS packet
//! plus enough metadata for downstream assembly, and a per-input
//! [`TsEsDemuxer`] task that reads the input's raw-TS broadcast and
//! dispatches packets onto the right per-PID channels.
//!
//! Scope evolution: this bus was originally per-flow. As of the node-bus
//! refactor (PES Switch plan, Phase 1), one [`NodeEsBus`] is owned by
//! [`crate::engine::manager::FlowManager`] and shared across every
//! assembled flow on the edge. Channels are still keyed by
//! `(input_id, source_pid)` — input IDs are globally unique on a node,
//! so the key shape already covers the new scope. The assignment-
//! uniqueness rule (one input → one flow) is still enforced today, so
//! channel keys still serialise to one publisher / consumer pair in
//! practice; Phase 2 lifts that and the bus picks up real fan-out.
//!
//! Phase 4 ships these primitives with unit tests and zero integration
//! into [`crate::engine::flow::FlowRuntime`]. Phase 5 owns the
//! integration and the per-output assembler. Keeping them separate
//! means today's passthrough pipeline never touches this code and there
//! is no regression risk while the assembler is under construction.
//!
//! Design notes:
//!
//! - `EsPacket.payload` is a full 188-byte TS packet, *not* unwrapped
//!   PES. That's deliberate: the assembler's job is PID-level, not
//!   PES-level, so it can rewrite the header PID field in place and
//!   emit the resulting TS byte-for-byte. This matches what
//!   [`crate::engine::ts_pid_remapper::TsPidRemapper`] already does for
//!   the simple-remap case.
//!
//! - Per-PID channels are `broadcast::channel(BUS_CHANNEL_CAPACITY)`.
//!   Slow consumers see `RecvError::Lagged(n)` and drop; nothing
//!   blocks the demuxer. Consistent with the flow's existing
//!   backpressure rule.
//!
//! - PAT and PMT PIDs are *not* published on the bus — those are a
//!   synthesis responsibility for the Phase 5 PSI generator, which
//!   builds fresh tables from the assembly plan. Null packets
//!   (PID 0x1FFF) are dropped.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::broadcast;

use super::packet::RtpPacket;
use super::ts_parse::*;

/// Bus capacity per PID — fallback when stream-type-aware sizing isn't
/// possible (subscriber arrives before the publisher has learned the
/// PMT, so we don't yet know whether the PID carries video, audio, or
/// data). Sized for the worst case (video) to avoid over-truncating
/// during the brief pre-PMT window. Kept as a public constant for
/// downstream callers / tests that want the video-class baseline.
///
/// **Right-sized values per stream class** — see
/// [`bus_capacity_for_stream_type`]. A single video PID carrying ~90 %
/// of a flow's compressed bitrate at a few hundred Mbps runs at
/// ~40 kpps; 8192 slots ≈ 200 ms of buffer at that rate — enough for
/// input switches, PMT updates, and 2022-7 dual-leg merge transients
/// without lagging consumers losing data. Memory cost: the ring is a
/// `Box<[Mutex<Slot<EsPacket>>]>` allocated eagerly at construction, and
/// `Mutex<Slot<EsPacket>>` measures **96 B** — so a video ring is
/// ~768 KiB, not the ~480 KB an earlier version of this comment claimed.
/// Audio and data PIDs use far smaller channels (see the helper) since
/// their bitrates are 1000× lower: ~96 KiB each, ~48 KiB for SCTE-35.
/// The map itself is append-only (see `NodeEsBus`), so these rings are
/// retained for the life of the process once minted.
#[allow(dead_code)]
pub const BUS_CHANNEL_CAPACITY: usize = 8192;

/// Bus channel capacity (slots) appropriate for a PMT-declared
/// `stream_type`.
///
/// Audio and data PIDs at typical bitrates (~200 kbps audio, < 1 kbps
/// SCTE-35) consume two to four orders of magnitude less bandwidth
/// than video, so the bus channel for them can be much smaller and
/// still hold seconds of buffer. Right-sizing matters when an
/// assembled flow plan references many audio + data PIDs (typical
/// SCTE-35 broadcast feed: 1 video + 4 audio + 4 captions + 1 SCTE-35
/// = 10 PIDs) — the saving is ~75 % of the bus memory budget vs the
/// uniform-8192 model.
///
/// `0` (unknown stream_type) returns the video capacity to stay safe
/// — we'd rather over-allocate during the brief pre-PMT learning
/// window than risk truncating a video PID.
///
/// Reference: ISO/IEC 13818-1 Table 2-34, ATSC A/53, DVB EN 300 468,
/// SCTE 35.
pub const fn bus_capacity_for_stream_type(stream_type: u8) -> usize {
    match stream_type {
        // Video — MPEG-1/2 (0x01, 0x02), MPEG-4 (0x10), H.264 (0x1B),
        // SVC/MVC variants (0x20, 0x21, 0x24), JPEG 2000 (0x42),
        // H.265 / HEVC (0x52, 0xD1).
        0x01 | 0x02 | 0x10 | 0x1B | 0x20 | 0x21 | 0x24 | 0x42 | 0x52 | 0xD1 => 8192,
        // Audio — MPEG-1 (0x03), MPEG-2 (0x04), AAC ADTS (0x0F),
        // AAC LATM (0x11), MPEG-4 audio (0x1C), AC-3 (0x80/0x81),
        // DTS (0x82), DTS-HD (0x83, 0x86), E-AC-3 (0x87), and the
        // ATSC private-audio range (0x84, 0x85, 0x88, 0xC1, 0xC2).
        // At 96–384 kbps typical bitrate, 1024 slots × ~60 B = 61 KB
        // per audio PID and still ≥ 10 s of buffer at 200 kbps.
        0x03 | 0x04 | 0x0F | 0x11 | 0x1C | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85
        | 0x87 | 0x88 | 0xC1 | 0xC2 => 1024,
        // SCTE-35 splice info (0x86) — splice messages are tens of
        // bytes apart from sparse insertions; even 512 is generous.
        0x86 => 512,
        // Private data (0x05, 0x06), DSM-CC (0x0B/0x0C), AC-3 in the
        // private namespace, ANC, captions, teletext. Sparse-to-modest
        // bitrate; 1024 covers both teletext-class and ANC-class with
        // headroom.
        0x05 | 0x06 | 0x0B | 0x0C => 1024,
        // Unknown — pre-PMT window, or a stream_type the standards have
        // added since this table was last updated. Fall back to the
        // video-class default to avoid truncating a real video PID.
        _ => 8192,
    }
}

/// A single elementary-stream packet on the bus.
///
/// Carries one 188-byte TS packet (the source's raw bytes, unmodified)
/// plus the metadata the assembler needs to slot it into the egress TS
/// without any fresh parsing: source PID, PMT stream_type, PUSI flag,
/// PCR (when present), and the upstream ingress timestamp. The source
/// TS packet's continuity counter stays in-band (byte 3 of the payload).
#[derive(Debug, Clone)]
pub struct EsPacket {
    /// Original PID on the source stream. Useful for debugging and for
    /// PSI-version cross-checks; the assembler rewrites this to the
    /// configured `out_pid` before emission.
    #[allow(dead_code)]
    pub source_pid: u16,
    /// PMT-declared `stream_type` at the time this packet was published.
    /// Lets the assembler cross-check against its configured plan and
    /// surface a warning event on mismatch.
    pub stream_type: u8,
    /// Raw TS packet, exactly 188 bytes.
    pub payload: Bytes,
    /// Payload-Unit-Start Indicator — true when this TS packet begins a
    /// new PES.
    #[allow(dead_code)]
    pub is_pusi: bool,
    /// True when this packet carries a PCR in its adaptation field.
    pub has_pcr: bool,
    /// Extracted 27 MHz PCR value when `has_pcr`. Synthetic flag;
    /// bytes are still in `payload` for byte-exact re-emission.
    pub pcr: Option<u64>,
    /// `recv_time_us` carried through from the upstream `RtpPacket`
    /// — lets downstream PCR-accuracy monitors measure jitter.
    #[allow(dead_code)]
    pub recv_time_us: u64,
    /// Upstream RTP / SRT sequence number when known. `None` for inputs
    /// that don't carry a wire-level seq (raw TS over UDP, RIST raw TS).
    /// Stamped by `input_rtp` / `input_srt` so the pre-bus 2022-7
    /// seq-aware Hitless merger can dedup + gap-fill against the same
    /// algorithm used at the transport layer in `redundancy/merger.rs`.
    pub upstream_seq: Option<u16>,
}

/// Node-wide elementary-stream bus. Read-shared across every consumer
/// (PSI generator, assembler, analysis) on the edge via `Arc`.
///
/// Keyed by `(input_id, source_pid)`. Input IDs are globally unique on
/// the node (top-level config entities), so the key shape already
/// scopes correctly without further qualification. Channels are
/// created lazily on first observation of a new PID and kept for the
/// life of the node.
///
/// Owned by [`crate::engine::manager::FlowManager`]; cloned into every
/// assembled flow's runtime via `Arc`. Passthrough flows never touch
/// the bus and pay zero cost (no map lookups, no allocations).
pub struct NodeEsBus {
    channels: DashMap<(String, u16), broadcast::Sender<EsPacket>>,
}

impl NodeEsBus {
    pub fn new() -> Self {
        Self {
            channels: DashMap::new(),
        }
    }

    /// Resolve (or create) the broadcast sender for a given
    /// `(input_id, source_pid)` key. Subsequent publishes go through it.
    ///
    /// `stream_type` is the PMT-declared codec class — used to size the
    /// channel appropriately on first creation (see
    /// [`bus_capacity_for_stream_type`]). Subsequent calls with the same
    /// key ignore the hint (channel capacity is fixed at construction).
    /// Pass `0` when the stream type is not yet known (subscribe-before-
    /// PMT path) — the channel falls back to the video-class default,
    /// which over-allocates briefly but never under-truncates.
    pub fn sender_for(
        &self,
        input_id: &str,
        source_pid: u16,
        stream_type: u8,
    ) -> broadcast::Sender<EsPacket> {
        if let Some(tx) = self.channels.get(&(input_id.to_string(), source_pid)) {
            return tx.value().clone();
        }
        let cap = bus_capacity_for_stream_type(stream_type);
        let (tx, _) = broadcast::channel(cap);
        self.channels
            .entry((input_id.to_string(), source_pid))
            .or_insert(tx)
            .value()
            .clone()
    }

    /// Subscribe to a given `(input_id, source_pid)`. Creates the channel
    /// if no publisher has touched it yet — that way the Phase 5
    /// assembler can wire up its consumers before the input task has
    /// seen any packets.
    ///
    /// Subscribe-before-publish creates the channel at the video-class
    /// default (8192) because the stream type isn't known yet; once the
    /// publisher arrives with a real `stream_type`, the channel already
    /// exists and is reused (capacity is fixed at construction).
    pub fn subscribe(&self, input_id: &str, source_pid: u16) -> broadcast::Receiver<EsPacket> {
        self.sender_for(input_id, source_pid, 0).subscribe()
    }

    /// Snapshot the currently-registered `(input_id, source_pid)` keys.
    /// Useful for debug logging / stats.
    #[allow(dead_code)]
    pub fn keys(&self) -> Vec<(String, u16)> {
        self.channels.iter().map(|e| e.key().clone()).collect()
    }
}

impl Default for NodeEsBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Demultiplexes a raw-TS input stream into per-PID `EsPacket`s.
///
/// Lifecycle: create one per input via [`TsEsDemuxer::new`], then call
/// [`TsEsDemuxer::process`] on every received `RtpPacket`. The demuxer
/// learns stream_types from the PAT + PMTs as they arrive; packets on
/// unknown PIDs (before their PMT has been observed) are published
/// with `stream_type = 0` and the assembler can fall back to the
/// configured plan.
/// Ceiling on distinct PIDs this demuxer will ever publish for one input.
///
/// Sized against real transponder captures rather than the 8192-PID
/// theoretical space: the widest MPTS in the repo's own corpus
/// (`astra192E-ts1080`, a full DVB-S transponder) carries 133 distinct
/// non-null PIDs, and typical MPTS run 22–48. 256 clears the worst real
/// capture by nearly 2× while bounding what a corrupt stream can cost.
const MAX_PIDS_PER_INPUT: usize = 256;

/// Ceiling on distinct PIDs that no PMT ever announced.
///
/// Far tighter than [`MAX_PIDS_PER_INPUT`] because legitimate content has
/// essentially none: an ES PID that no PMT declares is either a brief
/// pre-PAT window at stream start or garbage. It is garbage that matters
/// here — a 204-byte (188 + 16 Reed-Solomon) TS walked with this loop's
/// fixed 188-byte stride puts the sync byte at a sliding offset, and on
/// `BTS204.ts` in the media library that yields 658 distinct bogus PIDs.
/// At a video-class ring apiece that is ~500 MB of permanently-retained
/// channel for a stream carrying no valid PID at all.
///
/// Deliberately a cap and NOT a "publish only PMT-declared PIDs" rule:
/// `ingest_pmt` parses only the first TS packet of a PMT section, so a
/// multi-packet PMT's tail entries never reach `stream_types`, and a
/// declared-only rule would silently drop those ES — partial black on a
/// CA-heavy transponder. A cap tolerates that parser limit; a whitelist
/// would not.
const MAX_UNDECLARED_PIDS_PER_INPUT: usize = 32;

/// Largest parent datagram for which an `EsPacket` payload is taken as a
/// zero-copy `slice_ref` of the source rather than a fresh 188-byte copy.
///
/// `slice_ref` avoids one allocation + memcpy per TS packet — at 10 Gbps
/// that is millions of `malloc`s a second off a SCHED_FIFO thread. The
/// cost is that a retained slice keeps its whole parent alive, so a
/// datagram whose packets are mostly dropped (null / PAT / PMT) is held
/// for the few that are published. At the shipped `ts_packets_per_datagram`
/// of 5–7 the parent is ~1316 B and sharing it is strictly cheaper than
/// seven separate allocations; the field accepts up to 348, where a
/// single retained packet would pin 65 KB. This bound keeps the win for
/// every realistic configuration and caps worst-case retention at ~22×.
const MAX_SLICE_REF_PARENT: usize = 4096;

pub struct TsEsDemuxer {
    /// Node-globally-unique input ID — used as the bus key together with the PID.
    input_id: String,
    /// Shared node-wide bus. Cloned from the [`NodeEsBus`] owned by the
    /// `FlowManager`.
    bus: Arc<NodeEsBus>,
    /// Per-PID sender cache, so the hot path touches the node-wide
    /// `DashMap` once per PID instead of once per 188-byte TS packet.
    ///
    /// Sound because [`NodeEsBus::channels`] is append-only: it has no
    /// `remove`, `clear`, `retain` or bare `insert` anywhere in the tree,
    /// and [`NodeEsBus::subscribe`] resolves through the same
    /// [`NodeEsBus::sender_for`], so a cached `Sender` is the very
    /// instance any later subscriber attaches to. **If an eviction path is
    /// ever added to that map, this cache must be invalidated with it.**
    ///
    /// Positive resolutions only. Caching a refusal would make a PID
    /// rejected during the pre-PMT window rejected for the life of the
    /// demuxer — and that life is the *input's*, not the flow's — which is
    /// a session-long silent ES drop.
    senders: std::collections::HashMap<u16, broadcast::Sender<EsPacket>>,
    /// Count of cached PIDs that no PMT had declared when first seen.
    undeclared_pids: usize,
    /// Latch so a sustained cap breach reports once, not per packet.
    pid_cap_reported: bool,
    /// Optional operator-facing event channel. `None` in tests.
    events: Option<crate::manager::events::EventSender>,
    /// Flow this demuxer's input belongs to, for event attribution.
    flow_id: String,
    /// Known PMT PIDs, learned from PAT.
    pmt_pids: std::collections::HashSet<u16>,
    /// Learned `stream_type` per source PID, keyed from PMT entries.
    stream_types: std::collections::HashMap<u16, u8>,
    /// Last PAT version applied — skips reparse on duplicates.
    last_pat_version: Option<u8>,
    /// Last PMT version per PMT PID — skips reparse on duplicates.
    last_pmt_versions: std::collections::HashMap<u16, u8>,
}

impl TsEsDemuxer {
    pub fn new(input_id: impl Into<String>, bus: Arc<NodeEsBus>) -> Self {
        Self {
            input_id: input_id.into(),
            bus,
            senders: std::collections::HashMap::new(),
            undeclared_pids: 0,
            pid_cap_reported: false,
            events: None,
            flow_id: String::new(),
            pmt_pids: std::collections::HashSet::new(),
            stream_types: std::collections::HashMap::new(),
            last_pat_version: None,
            last_pmt_versions: std::collections::HashMap::new(),
        }
    }

    /// Attach the operator-facing event channel used to report a PID-cap
    /// breach. Without it the breach is still enforced and still logged,
    /// but only to the process log.
    pub fn with_events(
        mut self,
        events: crate::manager::events::EventSender,
        flow_id: impl Into<String>,
    ) -> Self {
        self.events = Some(events);
        self.flow_id = flow_id.into();
        self
    }

    /// Admit `pid` to the per-input PID set, or refuse it if a cap is hit.
    ///
    /// Evaluated on the cache-miss path only, so the steady-state cost is
    /// nil. Reports the first refusal and stays quiet thereafter.
    fn admit_pid(&mut self, pid: u16, declared: bool) -> bool {
        if self.senders.len() >= MAX_PIDS_PER_INPUT
            || (!declared && self.undeclared_pids >= MAX_UNDECLARED_PIDS_PER_INPUT)
        {
            if !self.pid_cap_reported {
                self.pid_cap_reported = true;
                let detail = format!(
                    "input '{}' reached the elementary-stream PID cap ({} distinct PIDs, \
                     {} of them never announced by a PMT); PID 0x{:04X} and any further \
                     new PID are dropped. This normally means the stream is not \
                     188-byte-aligned MPEG-TS — a 204-byte (Reed-Solomon) capture is \
                     the usual cause.",
                    self.input_id,
                    self.senders.len(),
                    self.undeclared_pids,
                    pid,
                );
                tracing::warn!("{}", detail);
                if let Some(events) = &self.events {
                    events.emit_flow_with_details(
                        crate::manager::events::EventSeverity::Warning,
                        crate::manager::events::category::FLOW,
                        detail,
                        &self.flow_id,
                        serde_json::json!({
                            "error_code": "pid_bus_pid_cap_reached",
                            "input_id": self.input_id,
                            "first_refused_pid": pid,
                            "distinct_pids": self.senders.len(),
                            "undeclared_pids": self.undeclared_pids,
                            "max_pids": MAX_PIDS_PER_INPUT,
                            "max_undeclared_pids": MAX_UNDECLARED_PIDS_PER_INPUT,
                        }),
                    );
                }
            }
            return false;
        }
        if !declared {
            self.undeclared_pids += 1;
        }
        true
    }

    /// Feed one `RtpPacket` into the demuxer. Dispatches each embedded
    /// 188-byte TS packet whose PID is an elementary stream onto the
    /// bus; PAT/PMT PIDs feed the internal catalogue but are NOT
    /// re-emitted (the assembler synthesises fresh PAT/PMT from its
    /// own plan). Null packets are dropped.
    ///
    /// Returns the number of ES packets published. Useful for metrics.
    pub fn process(&mut self, pkt: &RtpPacket) -> usize {
        let data: &[u8] = if pkt.is_raw_ts {
            &pkt.data
        } else if pkt.data.len() > RTP_HEADER_MIN_SIZE {
            &pkt.data[RTP_HEADER_MIN_SIZE..]
        } else {
            return 0;
        };
        let mut published = 0;
        let mut off = 0;
        while off + TS_PACKET_SIZE <= data.len() {
            let ts = &data[off..off + TS_PACKET_SIZE];
            off += TS_PACKET_SIZE;
            if ts[0] != TS_SYNC_BYTE {
                continue;
            }
            let pid = ts_pid(ts);
            if pid == PAT_PID {
                if ts_pusi(ts) {
                    self.ingest_pat(ts);
                }
                continue;
            }
            if pid == NULL_PID {
                continue;
            }
            if self.pmt_pids.contains(&pid) {
                if ts_pusi(ts) {
                    self.ingest_pmt(pid, ts);
                }
                continue;
            }
            // Elementary stream packet — publish onto the bus.
            let declared = self.stream_types.contains_key(&pid);
            let stream_type = self.stream_types.get(&pid).copied().unwrap_or(0);

            // Resolve the channel. The cap is checked before the cache is
            // written, never after, so a refused PID is re-evaluated on its
            // next packet rather than being negatively cached.
            if !self.senders.contains_key(&pid) {
                if !self.admit_pid(pid, declared) {
                    continue;
                }
                let tx = self.bus.sender_for(&self.input_id, pid, stream_type);
                self.senders.insert(pid, tx);
            }

            let is_pusi = ts_pusi(ts);
            let pcr = extract_pcr(ts);
            // Zero-copy where the parent is small enough to share; see
            // `MAX_SLICE_REF_PARENT`. `ts` is always a subslice of
            // `pkt.data` (`data` is either `&pkt.data` or `&pkt.data[12..]`),
            // which is what `slice_ref` requires.
            let payload = if pkt.data.len() <= MAX_SLICE_REF_PARENT {
                pkt.data.slice_ref(ts)
            } else {
                Bytes::copy_from_slice(ts)
            };
            let es = EsPacket {
                source_pid: pid,
                stream_type,
                payload,
                is_pusi,
                has_pcr: pcr.is_some(),
                pcr,
                recv_time_us: pkt.recv_time_us,
                upstream_seq: pkt.upstream_seq,
            };
            // `send` returns `Err` only when there are no active
            // subscribers — that's fine, we don't hold packets for the
            // future. Count attempts, not actual receivers.
            if let Some(tx) = self.senders.get(&pid) {
                let _ = tx.send(es);
                published += 1;
            }
        }
        published
    }

    fn ingest_pat(&mut self, pkt: &[u8]) {
        let mut sec_off = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            return;
        }
        let pointer = pkt[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 8 > TS_PACKET_SIZE {
            return;
        }
        if pkt[sec_off] != 0x00 {
            return;
        }
        let version = (pkt[sec_off + 5] >> 1) & 0x1F;
        if self.last_pat_version == Some(version) {
            return;
        }
        self.last_pat_version = Some(version);
        let programs = parse_pat_programs(pkt);
        self.pmt_pids = programs.iter().map(|(_, p)| *p).collect();
        // Drop stale PMT versions for PIDs no longer in the PAT so the
        // next PMT on a reused PID isn't skipped as "same version".
        self.last_pmt_versions.retain(|pid, _| self.pmt_pids.contains(pid));
    }

    fn ingest_pmt(&mut self, pmt_pid: u16, pkt: &[u8]) {
        let mut sec_off = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            sec_off = 5 + af_len;
        }
        if sec_off >= TS_PACKET_SIZE {
            return;
        }
        let pointer = pkt[sec_off] as usize;
        sec_off += 1 + pointer;
        if sec_off + 12 > TS_PACKET_SIZE {
            return;
        }
        if pkt[sec_off] != 0x02 {
            return;
        }
        let version = (pkt[sec_off + 5] >> 1) & 0x1F;
        if self.last_pmt_versions.get(&pmt_pid) == Some(&version) {
            return;
        }
        self.last_pmt_versions.insert(pmt_pid, version);
        let section_length =
            (((pkt[sec_off + 1] & 0x0F) as usize) << 8) | (pkt[sec_off + 2] as usize);
        let program_info_length =
            (((pkt[sec_off + 10] & 0x0F) as usize) << 8) | (pkt[sec_off + 11] as usize);
        let data_start = sec_off + 12 + program_info_length;
        let data_end = (sec_off + 3 + section_length)
            .min(TS_PACKET_SIZE)
            .saturating_sub(4);
        let mut pos = data_start;
        while pos + 5 <= data_end {
            let stream_type = pkt[pos];
            let pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
            let es_info_length =
                (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
            self.stream_types.insert(pid, stream_type);
            pos += 5 + es_info_length;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pat(programs: &[(u16, u16)], version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let section_length = 5 + 4 * programs.len() + 4;
        pkt[5] = 0x00;
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let mut pos = 13;
        for (pn, pmt_pid) in programs {
            pkt[pos] = (pn >> 8) as u8;
            pkt[pos + 1] = (pn & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        pkt
    }

    fn build_pmt(pmt_pid: u16, streams: &[(u8, u16)], pcr_pid: u16, version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let section_data_len = 9 + 5 * streams.len() + 4;
        let section_length = section_data_len as u16;
        pkt[5] = 0x02;
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1);
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        let mut pos = 17;
        for (st, pid) in streams {
            pkt[pos] = *st;
            pkt[pos + 1] = 0xE0 | (((pid >> 8) as u8) & 0x1F);
            pkt[pos + 2] = (pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        pkt
    }

    fn build_es(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        pkt
    }

    fn wrap(bytes: Vec<u8>) -> RtpPacket {
        RtpPacket {
            data: Bytes::from(bytes),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 12345,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
            is_raw_ts: true,
        }
    }

    #[test]
    fn demuxer_publishes_es_packets_with_stream_types() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx_video = bus.subscribe("in-a", 0x101);
        let mut rx_audio = bus.subscribe("in-a", 0x102);

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101), (0x0F, 0x102)], 0x101, 0));
        buf.extend_from_slice(&build_es(0x101, 3));
        buf.extend_from_slice(&build_es(0x102, 7));
        buf.extend_from_slice(&build_es(0x1FFF, 0)); // null — dropped
        let n = demux.process(&wrap(buf));
        assert_eq!(n, 2, "only the two ES packets should be published");

        let v = rx_video.try_recv().expect("video packet");
        assert_eq!(v.source_pid, 0x101);
        assert_eq!(v.stream_type, 0x1B);
        assert_eq!(v.recv_time_us, 12345);

        let a = rx_audio.try_recv().expect("audio packet");
        assert_eq!(a.source_pid, 0x102);
        assert_eq!(a.stream_type, 0x0F);
    }

    /// The published payload must be the exact 188 bytes of the source TS
    /// packet. Nothing else in the suite reads `EsPacket.payload` back, so
    /// without this an off-by-N in the zero-copy `slice_ref` path — which
    /// derives the payload from an *offset into the parent datagram* rather
    /// than from a fresh copy — would ship green.
    #[test]
    fn published_payload_is_the_exact_source_packet() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        let es_a = build_es(0x101, 3);
        let es_b = build_es(0x101, 4);
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        buf.extend_from_slice(&es_a);
        buf.extend_from_slice(&es_b);
        assert_eq!(demux.process(&wrap(buf)), 2);

        // Both packets, in order, byte-for-byte — and specifically NOT the
        // PAT or PMT that preceded them in the same parent buffer.
        let first = rx.try_recv().expect("first ES");
        assert_eq!(first.payload.len(), TS_PACKET_SIZE);
        assert_eq!(&first.payload[..], &es_a[..], "first payload must match");
        let second = rx.try_recv().expect("second ES");
        assert_eq!(&second.payload[..], &es_b[..], "second payload must match");
    }

    /// Same guarantee on the non-raw-TS arm, where `data` is a slice taken
    /// 12 bytes into the parent. This is the arm most likely to be got
    /// wrong by a zero-copy rewrite, because the payload offset and the
    /// parent offset differ.
    #[test]
    fn published_payload_is_exact_behind_an_rtp_header() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        let es = build_es(0x101, 9);
        let mut buf = vec![0u8; RTP_HEADER_MIN_SIZE];
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        buf.extend_from_slice(&es);

        let mut pkt = wrap(buf);
        pkt.is_raw_ts = false;
        assert_eq!(demux.process(&pkt), 1);

        let got = rx.try_recv().expect("ES packet");
        assert_eq!(&got.payload[..], &es[..], "payload must skip the RTP header");
    }

    /// A parent larger than `MAX_SLICE_REF_PARENT` takes the copying path
    /// so one retained packet cannot pin a large datagram. The bytes must
    /// be identical either way — this pins the fallback, not the offset.
    #[test]
    fn large_parent_falls_back_to_copy_with_identical_bytes() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        let marker = build_es(0x101, 5);
        buf.extend_from_slice(&marker);
        // Pad past the slice_ref threshold with null packets.
        while buf.len() <= MAX_SLICE_REF_PARENT {
            buf.extend_from_slice(&build_es(NULL_PID, 0));
        }
        assert!(buf.len() > MAX_SLICE_REF_PARENT);

        assert_eq!(demux.process(&wrap(buf)), 1);
        let got = rx.try_recv().expect("ES packet");
        assert_eq!(&got.payload[..], &marker[..]);
    }

    /// A stream that is not 188-byte aligned (the 204-byte Reed-Solomon
    /// case) walks the PID space and would otherwise allocate a permanent
    /// broadcast ring per bogus PID. The undeclared-PID cap bounds it.
    #[test]
    fn undeclared_pid_cap_bounds_channel_allocation() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());

        // No PAT/PMT at all, so every PID is undeclared.
        let mut buf = Vec::new();
        for pid in 0x200..0x200 + (MAX_UNDECLARED_PIDS_PER_INPUT as u16 * 4) {
            buf.extend_from_slice(&build_es(pid, 0));
        }
        let published = demux.process(&wrap(buf));

        assert_eq!(
            published, MAX_UNDECLARED_PIDS_PER_INPUT,
            "only the first {MAX_UNDECLARED_PIDS_PER_INPUT} undeclared PIDs may publish"
        );
        assert_eq!(demux.senders.len(), MAX_UNDECLARED_PIDS_PER_INPUT);
        assert!(demux.pid_cap_reported, "the breach must be reported once");
    }

    /// The cap must not negatively cache: a PID refused before its PMT
    /// arrived has to be admitted once the PMT declares it, or an ES goes
    /// silent for the life of the input.
    #[test]
    fn a_declared_pid_is_still_admitted_after_the_undeclared_cap_is_hit() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        // Exhaust the undeclared budget first.
        let mut junk = Vec::new();
        for pid in 0x200..0x200 + (MAX_UNDECLARED_PIDS_PER_INPUT as u16 * 2) {
            junk.extend_from_slice(&build_es(pid, 0));
        }
        demux.process(&wrap(junk));
        assert!(demux.pid_cap_reported);

        // Now a real PAT/PMT arrives declaring 0x101, followed by its ES.
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        buf.extend_from_slice(&build_es(0x101, 1));
        assert_eq!(
            demux.process(&wrap(buf)),
            1,
            "a PMT-declared PID must still be admitted"
        );
        assert_eq!(rx.try_recv().expect("ES packet").source_pid, 0x101);
    }

    #[test]
    fn demuxer_keeps_two_inputs_isolated() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux_a = TsEsDemuxer::new("in-a", bus.clone());
        let mut demux_b = TsEsDemuxer::new("in-b", bus.clone());
        let mut rx_a = bus.subscribe("in-a", 0x100);
        let mut rx_b = bus.subscribe("in-b", 0x100);

        let mut a = Vec::new();
        a.extend_from_slice(&build_pat(&[(1, 0x50)], 0));
        a.extend_from_slice(&build_pmt(0x50, &[(0x1B, 0x100)], 0x100, 0));
        a.extend_from_slice(&build_es(0x100, 1));
        demux_a.process(&wrap(a));

        let mut b = Vec::new();
        b.extend_from_slice(&build_pat(&[(1, 0x51)], 0));
        b.extend_from_slice(&build_pmt(0x51, &[(0x24, 0x100)], 0x100, 0));
        b.extend_from_slice(&build_es(0x100, 9));
        demux_b.process(&wrap(b));

        let pa = rx_a.try_recv().unwrap();
        let pb = rx_b.try_recv().unwrap();
        assert_eq!(pa.source_pid, 0x100);
        assert_eq!(pa.stream_type, 0x1B);
        assert_eq!(pb.source_pid, 0x100);
        assert_eq!(pb.stream_type, 0x24);
    }

    #[test]
    fn demuxer_marks_pusi_and_pcr() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));

        // Build a PCR-bearing ES packet manually.
        let mut pcr_pkt = [0xFFu8; TS_PACKET_SIZE];
        pcr_pkt[0] = TS_SYNC_BYTE;
        pcr_pkt[1] = 0x40 | (((0x101u16 >> 8) as u8) & 0x1F);
        pcr_pkt[2] = (0x101u16 & 0xFF) as u8;
        // AFC = 11 (adaptation + payload), CC = 0
        pcr_pkt[3] = 0x30;
        // Adaptation field length = 7 (flags + PCR)
        pcr_pkt[4] = 7;
        // flags: PCR present
        pcr_pkt[5] = 0x10;
        // 42-bit PCR base + 6 reserved + 9-bit ext = e.g. base 27_000_000
        let base: u64 = 27_000_000;
        pcr_pkt[6] = ((base >> 25) & 0xFF) as u8;
        pcr_pkt[7] = ((base >> 17) & 0xFF) as u8;
        pcr_pkt[8] = ((base >> 9) & 0xFF) as u8;
        pcr_pkt[9] = ((base >> 1) & 0xFF) as u8;
        pcr_pkt[10] = ((base & 0x01) << 7) as u8 | 0x7E; // 6 reserved bits
        pcr_pkt[11] = 0x00; // extension lo

        buf.extend_from_slice(&pcr_pkt);
        demux.process(&wrap(buf));

        let p = rx.try_recv().unwrap();
        assert!(p.is_pusi, "PUSI must be propagated");
        assert!(p.has_pcr, "has_pcr must be set");
        assert!(p.pcr.is_some());
    }

    #[test]
    fn demuxer_ignores_rtp_header_when_not_raw_ts() {
        let bus = Arc::new(NodeEsBus::new());
        let mut demux = TsEsDemuxer::new("in-a", bus.clone());
        let mut rx = bus.subscribe("in-a", 0x101);

        let mut inner = Vec::new();
        inner.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        inner.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        inner.extend_from_slice(&build_es(0x101, 0));

        // Prepend a fake 12-byte RTP header.
        let mut with_hdr = vec![0u8; RTP_HEADER_MIN_SIZE];
        with_hdr.extend_from_slice(&inner);

        let pkt = RtpPacket {
            data: Bytes::from(with_hdr),
            sequence_number: 1,
            rtp_timestamp: 1,
            recv_time_us: 1,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
            is_raw_ts: false,
        };
        demux.process(&pkt);
        let p = rx.try_recv().unwrap();
        assert_eq!(p.source_pid, 0x101);
        assert_eq!(p.stream_type, 0x1B);
    }

    #[test]
    fn bus_sender_and_subscribe_lazy_create_channel() {
        let bus = NodeEsBus::new();
        // Subscribe-first pattern — the Phase 5 assembler wires up before
        // the demuxer publishes, so channels must exist at subscribe time.
        let mut rx = bus.subscribe("in-a", 0x100);
        let tx = bus.sender_for("in-a", 0x100, 0x1B);
        let dummy = EsPacket {
            source_pid: 0x100,
            stream_type: 0x1B,
            payload: Bytes::from_static(&[0u8; TS_PACKET_SIZE]),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
        };
        tx.send(dummy.clone()).unwrap();
        let got = rx.try_recv().unwrap();
        assert_eq!(got.source_pid, 0x100);
    }
}
