// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Elementary-stream bus primitives for the PID-bus runtime.
//!
//! This module provides the *plumbing* that the Phase 5 assembler will
//! consume: a [`FlowEsBus`] registry keyed by `(input_id, source_pid)`,
//! a lightweight [`EsPacket`] carrying exactly one 188-byte TS packet
//! plus enough metadata for downstream assembly, and a per-input
//! [`TsEsDemuxer`] task that reads the input's raw-TS broadcast and
//! dispatches packets onto the right per-PID channels.
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

/// Bus capacity per PID. Matches the flow-level broadcast default so a
/// well-paced consumer stays well within budget; a lagging one drops.
pub const BUS_CHANNEL_CAPACITY: usize = 2048;

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
    /// Identifies which leg of a 2022-7 dual-leg group produced this
    /// packet (0 = primary, 1 = backup). `None` for single-leg inputs
    /// or non-2022-7 sources. Used by the seq-aware merger to attribute
    /// failover events.
    pub upstream_leg_id: Option<u8>,
}

/// Per-flow elementary-stream bus. Read-shared across every consumer
/// (PSI generator, assembler, analysis) via `Arc`.
///
/// Keyed by `(input_id, source_pid)` so a flow with N inputs registers
/// one logical namespace per input; the same PID on two different
/// inputs does not collide. Channels are created lazily on first
/// observation of a new PID and kept for the life of the flow.
pub struct FlowEsBus {
    channels: DashMap<(String, u16), broadcast::Sender<EsPacket>>,
}

impl FlowEsBus {
    pub fn new() -> Self {
        Self {
            channels: DashMap::new(),
        }
    }

    /// Resolve (or create) the broadcast sender for a given
    /// `(input_id, source_pid)` key. Subsequent publishes go through it.
    pub fn sender_for(&self, input_id: &str, source_pid: u16) -> broadcast::Sender<EsPacket> {
        if let Some(tx) = self.channels.get(&(input_id.to_string(), source_pid)) {
            return tx.value().clone();
        }
        let (tx, _) = broadcast::channel(BUS_CHANNEL_CAPACITY);
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
    pub fn subscribe(&self, input_id: &str, source_pid: u16) -> broadcast::Receiver<EsPacket> {
        self.sender_for(input_id, source_pid).subscribe()
    }

    /// Snapshot the currently-registered `(input_id, source_pid)` keys.
    /// Useful for debug logging / stats.
    #[allow(dead_code)]
    pub fn keys(&self) -> Vec<(String, u16)> {
        self.channels.iter().map(|e| e.key().clone()).collect()
    }
}

impl Default for FlowEsBus {
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
pub struct TsEsDemuxer {
    /// Flow-local input ID — used as the bus key together with the PID.
    input_id: String,
    /// Shared bus. Cloned from the flow's `FlowEsBus`.
    bus: Arc<FlowEsBus>,
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
    pub fn new(input_id: impl Into<String>, bus: Arc<FlowEsBus>) -> Self {
        Self {
            input_id: input_id.into(),
            bus,
            pmt_pids: std::collections::HashSet::new(),
            stream_types: std::collections::HashMap::new(),
            last_pat_version: None,
            last_pmt_versions: std::collections::HashMap::new(),
        }
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
            let stream_type = self.stream_types.get(&pid).copied().unwrap_or(0);
            let is_pusi = ts_pusi(ts);
            let pcr = extract_pcr(ts);
            let es = EsPacket {
                source_pid: pid,
                stream_type,
                payload: Bytes::copy_from_slice(ts),
                is_pusi,
                has_pcr: pcr.is_some(),
                pcr,
                recv_time_us: pkt.recv_time_us,
                upstream_seq: pkt.upstream_seq,
                upstream_leg_id: pkt.upstream_leg_id,
            };
            let tx = self.bus.sender_for(&self.input_id, pid);
            // `send` returns `Err` only when there are no active
            // subscribers — that's fine, we don't hold packets for the
            // future. Count attempts, not actual receivers.
            let _ = tx.send(es);
            published += 1;
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
            is_raw_ts: true,
        }
    }

    #[test]
    fn demuxer_publishes_es_packets_with_stream_types() {
        let bus = Arc::new(FlowEsBus::new());
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

    #[test]
    fn demuxer_keeps_two_inputs_isolated() {
        let bus = Arc::new(FlowEsBus::new());
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
        let bus = Arc::new(FlowEsBus::new());
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
        let bus = Arc::new(FlowEsBus::new());
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
            is_raw_ts: false,
        };
        demux.process(&pkt);
        let p = rx.try_recv().unwrap();
        assert_eq!(p.source_pid, 0x101);
        assert_eq!(p.stream_type, 0x1B);
    }

    #[test]
    fn bus_sender_and_subscribe_lazy_create_channel() {
        let bus = FlowEsBus::new();
        // Subscribe-first pattern — the Phase 5 assembler wires up before
        // the demuxer publishes, so channels must exist at subscribe time.
        let mut rx = bus.subscribe("in-a", 0x100);
        let tx = bus.sender_for("in-a", 0x100);
        let dummy = EsPacket {
            source_pid: 0x100,
            stream_type: 0x1B,
            payload: Bytes::from_static(&[0u8; TS_PACKET_SIZE]),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
            upstream_leg_id: None,
        };
        tx.send(dummy.clone()).unwrap();
        let got = rx.try_recv().unwrap();
        assert_eq!(got.source_pid, 0x100);
    }
}
