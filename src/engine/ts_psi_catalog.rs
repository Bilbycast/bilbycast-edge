// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lightweight per-input PSI catalogue.
//!
//! One per TS-carrying input. Subscribes to the input's dedicated
//! broadcast channel and parses PAT + PMT sections only — no PES, no
//! decode — to publish a snapshot of programs and elementary streams
//! currently visible inside the stream. Runs independently of flow
//! activation so operators can inspect every configured input (active
//! **and** passive) and pick PIDs for the PID-bus UI.
//!
//! Non-TS inputs (RTMP, WebRTC, RTP carrying raw ES, ST 2110-30/-40)
//! must not spawn this observer — the TS classifier on [`InputConfig`]
//! is the gate.
//!
//! Allocation story: one catalog `Arc` per update, shared-readable via
//! [`PsiCatalogStore`]. PAT-version and PMT-version cached so version
//! bumps are the only trigger for a reparse + snapshot. Otherwise every
//! packet takes a cheap header check and does nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::packet::RtpPacket;
use super::ts_parse::*;

// Unix-microseconds timestamp helper, matching the rest of the stats layer.
fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Catalogue published per-input. Optional on the wire (absent = no
/// TS-bearing bytes seen yet).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PsiCatalog {
    /// Discovered programs (PAT entries other than the NIT).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<CatalogProgram>,
    /// Unix microseconds of the most recent update.
    pub last_updated_us: u64,
}

/// One program's catalogue entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogProgram {
    pub program_number: u16,
    pub pmt_pid: u16,
    /// PCR PID for this program (`None` on malformed/empty PMT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_pid: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<CatalogStream>,
}

/// One elementary stream in a program's PMT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogStream {
    pub pid: u16,
    /// MPEG-TS `stream_type` as declared in the PMT (e.g. `0x1B` =
    /// H.264, `0x0F` = AAC).
    pub stream_type: u8,
    /// Short human label inferred from `stream_type` — never `None`
    /// because the catalog is rendered in the UI; we always ship
    /// something, falling back to `"Unknown"`.
    pub codec: String,
    /// What broad family the codec belongs to — makes downstream UI
    /// code (and the future PID-bus assembler) simple to author
    /// without re-implementing the stream_type table.
    pub kind: CatalogStreamKind,
}

/// Broad categorisation of an ES. Same distinction the PID-bus
/// assembler will key on later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStreamKind {
    Video,
    Audio,
    Subtitle,
    Data,
    Unknown,
}

/// Map an MPEG-TS `stream_type` byte to a human name + broad kind.
/// Reference: ITU-T H.222.0 Table 2-34 and common industry practice
/// for private_data slots (Opus via codec_id, AC-3 on ATSC, ...).
fn classify_stream_type(stream_type: u8) -> (&'static str, CatalogStreamKind) {
    use CatalogStreamKind::*;
    match stream_type {
        0x01 => ("MPEG-1 Video", Video),
        0x02 => ("MPEG-2 Video", Video),
        0x03 => ("MPEG-1 Audio", Audio),
        0x04 => ("MPEG-2 Audio", Audio),
        0x06 => ("PES private", Data),
        0x0F => ("AAC (ADTS)", Audio),
        0x10 => ("MPEG-4 Part 2 Video", Video),
        0x11 => ("AAC-LATM", Audio),
        0x15 => ("Metadata (PES)", Data),
        0x1B => ("H.264 / AVC", Video),
        0x1C => ("AAC (raw)", Audio),
        0x20 => ("H.264 SVC", Video),
        0x21 => ("JPEG 2000", Video),
        0x24 => ("H.265 / HEVC", Video),
        0x32 => ("JPEG XS", Video),
        0x81 => ("AC-3 (ATSC)", Audio),
        0x82 => ("SCTE-35 splice info", Data),
        0x84 => ("E-AC-3 (DVB)", Audio),
        0x87 => ("E-AC-3 (ATSC)", Audio),
        0x8A => ("DTS", Audio),
        _ => ("Unknown", Unknown),
    }
}

/// Per-input store. Cloneable and cheap to read on the stats snapshot
/// path. Writes happen only on PAT/PMT version bumps.
#[derive(Default)]
pub struct PsiCatalogStore {
    /// Latest catalog (if any). Cloned on reads via `load()`.
    inner: std::sync::RwLock<Option<Arc<PsiCatalog>>>,
    /// Monotonic tick bumped on every catalog update — lets the stats
    /// layer decide whether to re-serialise.
    update_counter: AtomicU64,
}

impl PsiCatalogStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current catalog (cheap `Arc` clone). Returns `None`
    /// until the observer has seen at least one valid PAT+PMT pair.
    pub fn load(&self) -> Option<Arc<PsiCatalog>> {
        self.inner.read().ok().and_then(|g| g.clone())
    }

    /// Replace the catalog and bump the update counter.
    fn store(&self, cat: PsiCatalog) {
        if let Ok(mut g) = self.inner.write() {
            *g = Some(Arc::new(cat));
            self.update_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read the update counter — useful for drop-equal change detection.
    pub fn tick(&self) -> u64 {
        self.update_counter.load(Ordering::Relaxed)
    }

    /// Test-only: seed the store with a catalogue without going through
    /// the observer. Used by Phase 6 resolver tests.
    #[cfg(test)]
    pub fn seed_for_test(store: &Self, cat: PsiCatalog) {
        store.store(cat);
    }
}

/// Spawn the per-input observer. Subscribes to the input's broadcast
/// channel and keeps the store fresh. Returns the join handle so the
/// flow runtime can cancel it with the rest of the input's children.
pub fn spawn_psi_catalog_observer(
    input_id: String,
    flow_id: String,
    broadcast_rx: broadcast::Sender<RtpPacket>,
    store: Arc<PsiCatalogStore>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_rx.subscribe();
    tokio::spawn(async move {
        let mut state = ObserverState::default();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = rx.recv() => match msg {
                    Ok(pkt) => observe_packet(&pkt, &mut state, &store),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Lag doesn't matter for a PSI observer — PSI is periodic,
                        // we'll re-parse the next PAT/PMT cycle.
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        tracing::trace!(
            flow = %flow_id,
            input = %input_id,
            "psi catalog observer stopped"
        );
    })
}

/// Mutable scratch state for the observer.
#[derive(Default)]
struct ObserverState {
    /// Last observed PAT version; reparse skipped on duplicates.
    last_pat_version: Option<u8>,
    /// The `transport_stream_id` carried in the last PAT (informational;
    /// retained for future UI use).
    _transport_stream_id: u16,
    /// Discovered PMT PIDs from the latest PAT, in program order.
    pmt_by_program: Vec<(u16, u16)>,
    /// Latest per-PMT version seen, keyed by PMT PID. `None` means not
    /// yet observed on the wire.
    pmt_versions: std::collections::HashMap<u16, u8>,
    /// Partial catalog accumulated as each PMT arrives. Published when
    /// it changes.
    current: PsiCatalog,
}

fn observe_packet(pkt: &RtpPacket, state: &mut ObserverState, store: &PsiCatalogStore) {
    // Walk every TS packet inside this datagram. Handles 7×188
    // MPEG-TS batches and RTP-wrapped + raw-TS equally.
    let data: &[u8] = if pkt.is_raw_ts {
        &pkt.data
    } else if pkt.data.len() > RTP_HEADER_MIN_SIZE {
        &pkt.data[RTP_HEADER_MIN_SIZE..]
    } else {
        return;
    };

    let mut off = 0;
    while off + TS_PACKET_SIZE <= data.len() {
        let ts = &data[off..off + TS_PACKET_SIZE];
        off += TS_PACKET_SIZE;
        if ts[0] != TS_SYNC_BYTE {
            continue;
        }
        let pid = ts_pid(ts);
        if pid == PAT_PID && ts_pusi(ts) {
            handle_pat(ts, state, store);
        } else if ts_pusi(ts) && state.pmt_by_program.iter().any(|(_, p)| *p == pid) {
            handle_pmt(ts, pid, state, store);
        }
    }
}

fn handle_pat(pkt: &[u8], state: &mut ObserverState, store: &PsiCatalogStore) {
    let (version, ts_id) = match read_pat_header(pkt) {
        Some(v) => v,
        None => return,
    };
    if state.last_pat_version == Some(version) {
        return;
    }
    let programs = parse_pat_programs(pkt);
    state.last_pat_version = Some(version);
    state._transport_stream_id = ts_id;
    state.pmt_by_program = programs;

    // Prune previously-known programs that are no longer in the PAT
    // and re-align the catalogue's program ordering.
    state
        .current
        .programs
        .retain(|p| state.pmt_by_program.iter().any(|(pn, _)| *pn == p.program_number));
    for (pn, pmt_pid) in &state.pmt_by_program {
        if let Some(slot) = state
            .current
            .programs
            .iter_mut()
            .find(|p| p.program_number == *pn)
        {
            slot.pmt_pid = *pmt_pid; // PMT PID can migrate on version bumps
        } else {
            state.current.programs.push(CatalogProgram {
                program_number: *pn,
                pmt_pid: *pmt_pid,
                pcr_pid: None,
                streams: Vec::new(),
            });
        }
    }
    state.current.programs.sort_by_key(|p| p.program_number);
    state.current.last_updated_us = now_us();

    // Drop PMT versions for programs that have disappeared so the next
    // fresh PMT on a reused PID isn't skipped as "same version".
    let live: std::collections::HashSet<u16> =
        state.pmt_by_program.iter().map(|(_, pid)| *pid).collect();
    state.pmt_versions.retain(|pid, _| live.contains(pid));

    store.store(state.current.clone());
}

fn handle_pmt(pkt: &[u8], pmt_pid: u16, state: &mut ObserverState, store: &PsiCatalogStore) {
    let (version, pcr_pid, streams) = match parse_pmt_section(pkt) {
        Some(v) => v,
        None => return,
    };
    if state.pmt_versions.get(&pmt_pid) == Some(&version) {
        return;
    }
    state.pmt_versions.insert(pmt_pid, version);

    // Find the program this PMT belongs to by PMT PID.
    let slot = match state.current.programs.iter_mut().find(|p| p.pmt_pid == pmt_pid) {
        Some(s) => s,
        None => return,
    };
    slot.pcr_pid = if pcr_pid == 0x1FFF {
        None
    } else {
        Some(pcr_pid)
    };
    slot.streams = streams
        .into_iter()
        .map(|(stream_type, pid)| {
            let (codec, kind) = classify_stream_type(stream_type);
            CatalogStream {
                pid,
                stream_type,
                codec: codec.to_string(),
                kind,
            }
        })
        .collect();
    state.current.last_updated_us = now_us();
    store.store(state.current.clone());
}

/// Read PAT header fields (version, transport_stream_id) from a PUSI
/// PAT packet, or return `None` when the section is truncated.
fn read_pat_header(pkt: &[u8]) -> Option<(u8, u16)> {
    let mut sec_off = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        sec_off = 5 + af_len;
    }
    if sec_off >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[sec_off] as usize;
    sec_off += 1 + pointer;
    if sec_off + 8 > TS_PACKET_SIZE {
        return None;
    }
    if pkt[sec_off] != 0x00 {
        return None;
    }
    let ts_id = ((pkt[sec_off + 3] as u16) << 8) | pkt[sec_off + 4] as u16;
    let version = (pkt[sec_off + 5] >> 1) & 0x1F;
    Some((version, ts_id))
}

/// Parse a whole-packet PMT into `(version, pcr_pid, streams)`. Returns
/// `None` when the section is malformed or spans multiple TS packets
/// (rare; we rely on periodic re-emission for recovery).
fn parse_pmt_section(pkt: &[u8]) -> Option<(u8, u16, Vec<(u8, u16)>)> {
    let mut sec_off = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        sec_off = 5 + af_len;
    }
    if sec_off >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[sec_off] as usize;
    sec_off += 1 + pointer;
    if sec_off + 12 > TS_PACKET_SIZE {
        return None;
    }
    if pkt[sec_off] != 0x02 {
        return None;
    }
    let section_length =
        (((pkt[sec_off + 1] & 0x0F) as usize) << 8) | (pkt[sec_off + 2] as usize);
    let version = (pkt[sec_off + 5] >> 1) & 0x1F;
    let pcr_pid = ((pkt[sec_off + 8] as u16 & 0x1F) << 8) | pkt[sec_off + 9] as u16;
    let program_info_length =
        (((pkt[sec_off + 10] & 0x0F) as usize) << 8) | (pkt[sec_off + 11] as usize);

    let data_start = sec_off + 12 + program_info_length;
    let data_end = (sec_off + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut streams = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        streams.push((stream_type, pid));
        pos += 5 + es_info_length;
    }
    Some((version, pcr_pid, streams))
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

    fn drive(state: &mut ObserverState, store: &PsiCatalogStore, bytes: &[u8]) {
        let pkt = RtpPacket {
            data: bytes::Bytes::copy_from_slice(bytes),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
        };
        observe_packet(&pkt, state, store);
    }

    #[test]
    fn catalog_populated_after_pat_and_pmt() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100), (2, 0x200)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101), (0x0F, 0x102)], 0x101, 0));
        buf.extend_from_slice(&build_pmt(0x200, &[(0x24, 0x201), (0x81, 0x202)], 0x201, 0));
        drive(&mut state, &store, &buf);

        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs.len(), 2);
        let p1 = &cat.programs[0];
        assert_eq!(p1.program_number, 1);
        assert_eq!(p1.pmt_pid, 0x100);
        assert_eq!(p1.pcr_pid, Some(0x101));
        assert_eq!(p1.streams.len(), 2);
        assert_eq!(p1.streams[0].pid, 0x101);
        assert_eq!(p1.streams[0].codec, "H.264 / AVC");
        assert_eq!(p1.streams[0].kind, CatalogStreamKind::Video);
        assert_eq!(p1.streams[1].codec, "AAC (ADTS)");
        assert_eq!(p1.streams[1].kind, CatalogStreamKind::Audio);
        let p2 = &cat.programs[1];
        assert_eq!(p2.streams[0].codec, "H.265 / HEVC");
        assert_eq!(p2.streams[1].codec, "AC-3 (ATSC)");
    }

    #[test]
    fn catalog_unknown_stream_type_labelled() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0xEE, 0x101)], 0x101, 0));
        drive(&mut state, &store, &buf);
        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs[0].streams[0].codec, "Unknown");
        assert_eq!(cat.programs[0].streams[0].kind, CatalogStreamKind::Unknown);
    }

    #[test]
    fn catalog_refreshes_on_pmt_version_bump() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        drive(&mut state, &store, &buf);
        assert_eq!(store.load().unwrap().programs[0].streams.len(), 1);
        let tick1 = store.tick();

        // New PMT version adds an audio stream.
        let mut buf2 = Vec::new();
        buf2.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101), (0x0F, 0x102)], 0x101, 1));
        drive(&mut state, &store, &buf2);
        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs[0].streams.len(), 2);
        assert!(store.tick() > tick1);
    }

    #[test]
    fn catalog_drops_program_on_pat_bump_when_program_disappears() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x100), (2, 0x200)], 0));
        buf.extend_from_slice(&build_pmt(0x100, &[(0x1B, 0x101)], 0x101, 0));
        buf.extend_from_slice(&build_pmt(0x200, &[(0x24, 0x201)], 0x201, 0));
        drive(&mut state, &store, &buf);
        assert_eq!(store.load().unwrap().programs.len(), 2);

        // PAT v1 drops program 2.
        let mut buf2 = Vec::new();
        buf2.extend_from_slice(&build_pat(&[(1, 0x100)], 1));
        drive(&mut state, &store, &buf2);
        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs.len(), 1);
        assert_eq!(cat.programs[0].program_number, 1);
    }
}
