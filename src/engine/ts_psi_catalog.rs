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
    /// Raw ES_info descriptor bytes from the source PMT, hex-encoded
    /// (empty when the loop was empty). 0x06 private-PES streams are
    /// unidentifiable without these — the DVB AC-3 / E-AC-3 / AAC /
    /// teletext / subtitling descriptors are the only codec signal —
    /// so they drive descriptor-aware `kind` classification here and
    /// are copied through onto assembled-output PMTs so downstream
    /// receivers can bind a decoder to the ES.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub descriptors_hex: String,
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

/// Descriptor-aware classification. For `stream_type 0x06` (PES private)
/// the bare stream_type table can only say "data" — the real identity
/// lives in the ES_info descriptor loop (DVB AC-3 0x6A, E-AC-3 0x7A,
/// DTS 0x7B, AAC 0x7C, registration 0x05, AC-4 via 0x7F/0x15, teletext
/// 0x56, subtitling 0x59). Without this, descriptor-tagged DVB audio
/// catalogues as `kind: data` and every kind-matched bus operation
/// (switcher Take → bus_route, matrix Swap) skips the audio slot.
/// Text services are checked FIRST so a teletext/subtitle ES can never
/// classify as audio (mirrors the pid_overrides binder discipline).
fn classify_stream(stream_type: u8, descriptors: &[u8]) -> (String, CatalogStreamKind) {
    use crate::engine::ts_parse::{
        descriptor_audio_kind, descriptors_indicate_text_service, PrivateEsAudioKind,
    };
    if stream_type == 0x06 && !descriptors.is_empty() {
        if descriptors_indicate_text_service(descriptors) {
            return ("DVB text (teletext/subtitles)".to_string(), CatalogStreamKind::Subtitle);
        }
        if let Some(kind) = descriptor_audio_kind(descriptors) {
            let name = match kind {
                PrivateEsAudioKind::Ac3 => "AC-3 (DVB)",
                PrivateEsAudioKind::Eac3 => "E-AC-3 (DVB)",
                PrivateEsAudioKind::AacLatm => "AAC-LATM (DVB)",
                PrivateEsAudioKind::Dts => "DTS (DVB)",
                PrivateEsAudioKind::Opus => "Opus (DVB)",
                PrivateEsAudioKind::Smpte302m => "SMPTE 302M",
                PrivateEsAudioKind::Ac4 => "AC-4 (DVB)",
            };
            return (name.to_string(), CatalogStreamKind::Audio);
        }
    }
    let (name, kind) = classify_stream_type(stream_type);
    (name.to_string(), kind)
}

/// Minimal hex codec for `CatalogStream::descriptors_hex` — avoids a new
/// crate dependency for a field that is a few dozen bytes per ES.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Tolerant inverse of [`hex_encode`] — odd-length or non-hex input
/// yields `None` (callers treat that as "no descriptors").
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
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

impl std::fmt::Debug for PsiCatalogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Stores ride inside `PlanCommand` (Debug-derived); dumping the
        // whole catalogue would spam logs — the tick identifies state.
        f.debug_struct("PsiCatalogStore")
            .field("tick", &self.tick())
            .finish_non_exhaustive()
    }
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
    /// In-flight cross-packet PMT sections, keyed by PMT PID. A PMT for a
    /// real DVB program (video + several audio + subs + teletext, each
    /// with descriptors) routinely exceeds the 184-byte single-packet
    /// payload — without reassembly those programs sat in the catalogue
    /// with an empty stream list FOREVER (periodic re-emission never
    /// helps; the section never fits in one packet).
    pmt_partials: std::collections::HashMap<u16, PartialPmtSection>,
    /// Partial catalog accumulated as each PMT arrives. Published when
    /// it changes.
    current: PsiCatalog,
}

/// One PMT section mid-reassembly: bytes from `table_id` onward, the total
/// length promised by the 3-byte section header, and the continuity
/// counter of the last packet folded in (a CC gap aborts the partial —
/// latching a stream list assembled across a loss would be worse than
/// staying empty until the next emission).
struct PartialPmtSection {
    buf: Vec<u8>,
    needed: usize,
    last_cc: u8,
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
        } else if state.pmt_by_program.iter().any(|(_, p)| *p == pid) {
            // PUSI and continuation packets both route through the
            // reassembler — multi-packet PMT sections need the latter.
            handle_pmt_packet(ts, pid, state, store);
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
    state.pmt_partials.retain(|pid, _| live.contains(pid));

    store.store(state.current.clone());
}

/// Route one TS packet on a PMT PID into the section reassembler.
fn handle_pmt_packet(pkt: &[u8], pmt_pid: u16, state: &mut ObserverState, store: &PsiCatalogStore) {
    if !ts_has_payload(pkt) {
        return;
    }
    let cc = ts_cc(pkt);
    let off = ts_payload_offset(pkt);
    if off >= TS_PACKET_SIZE {
        state.pmt_partials.remove(&pmt_pid);
        return;
    }

    if ts_pusi(pkt) {
        let pointer = pkt[off] as usize;
        let tail_start = off + 1;
        let sec_start = tail_start + pointer;
        if sec_start > TS_PACKET_SIZE {
            state.pmt_partials.remove(&pmt_pid);
            return;
        }
        // Bytes before the pointer target are the TAIL of the previous
        // section — fold them into an in-flight partial if CC is contiguous.
        if let Some(mut part) = state.pmt_partials.remove(&pmt_pid) {
            if pointer > 0 && cc == ((part.last_cc + 1) & 0x0F) {
                part.buf.extend_from_slice(&pkt[tail_start..sec_start]);
                if part.buf.len() >= part.needed {
                    complete_pmt_section(part.buf, part.needed, true, pmt_pid, state, store);
                }
            }
            // CC gap or no tail → the partial is unrecoverable; drop it.
        }
        start_pmt_section(&pkt[sec_start..TS_PACKET_SIZE], cc, pmt_pid, state, store);
    } else if let Some(part) = state.pmt_partials.get_mut(&pmt_pid) {
        if cc != ((part.last_cc + 1) & 0x0F) {
            // Lost a middle packet — abort rather than latch garbage.
            state.pmt_partials.remove(&pmt_pid);
            return;
        }
        part.last_cc = cc;
        part.buf.extend_from_slice(&pkt[off..TS_PACKET_SIZE]);
        if part.buf.len() >= part.needed {
            let part = state
                .pmt_partials
                .remove(&pmt_pid)
                .expect("partial present — just mutated");
            complete_pmt_section(part.buf, part.needed, true, pmt_pid, state, store);
        }
    }
}

/// Begin a fresh section at a PUSI packet's pointer target. Sections that
/// fit entirely in this packet complete immediately (the pre-reassembly
/// fast path, CRC not enforced to match historical behaviour); longer ones
/// are stashed for the continuation packets.
fn start_pmt_section(
    sec: &[u8],
    cc: u8,
    pmt_pid: u16,
    state: &mut ObserverState,
    store: &PsiCatalogStore,
) {
    if sec.len() < 3 || sec[0] != 0x02 {
        return;
    }
    let section_length = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
    let needed = 3 + section_length;
    if needed <= sec.len() {
        complete_pmt_section(sec[..needed].to_vec(), needed, false, pmt_pid, state, store);
    } else {
        state.pmt_partials.insert(
            pmt_pid,
            PartialPmtSection { buf: sec.to_vec(), needed, last_cc: cc },
        );
    }
}

/// Finalise a fully-buffered section: trim stuffing, CRC-verify when the
/// section was reassembled across packets (a CC gap we failed to notice
/// must not latch a garbage stream list under the version dedup), then
/// parse + publish.
fn complete_pmt_section(
    mut buf: Vec<u8>,
    needed: usize,
    verify_crc: bool,
    pmt_pid: u16,
    state: &mut ObserverState,
    store: &PsiCatalogStore,
) {
    if buf.len() < needed || needed < 16 {
        return;
    }
    buf.truncate(needed);
    if verify_crc {
        let want = u32::from_be_bytes([
            buf[needed - 4],
            buf[needed - 3],
            buf[needed - 2],
            buf[needed - 1],
        ]);
        if mpeg2_crc32(&buf[..needed - 4]) != want {
            return;
        }
    }
    let (version, pcr_pid, streams) = match parse_pmt_from_section(&buf) {
        Some(v) => v,
        None => return,
    };
    apply_parsed_pmt(version, pcr_pid, streams, pmt_pid, state, store);
}

fn apply_parsed_pmt(
    version: u8,
    pcr_pid: u16,
    streams: Vec<(u8, u16, Vec<u8>)>,
    pmt_pid: u16,
    state: &mut ObserverState,
    store: &PsiCatalogStore,
) {
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
        .map(|(stream_type, pid, descriptors)| {
            let (codec, kind) = classify_stream(stream_type, &descriptors);
            CatalogStream {
                pid,
                stream_type,
                codec,
                kind,
                descriptors_hex: hex_encode(&descriptors),
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

/// Parse a complete PMT section (bytes from `table_id`, any length up to
/// the 4096-byte section cap) into `(version, pcr_pid, streams)` where
/// each stream entry is `(stream_type, pid, es_info_descriptor_bytes)`.
fn parse_pmt_from_section(sec: &[u8]) -> Option<(u8, u16, Vec<(u8, u16, Vec<u8>)>)> {
    if sec.len() < 16 || sec[0] != 0x02 {
        return None;
    }
    let section_length = (((sec[1] & 0x0F) as usize) << 8) | (sec[2] as usize);
    let version = (sec[5] >> 1) & 0x1F;
    let pcr_pid = ((sec[8] as u16 & 0x1F) << 8) | sec[9] as u16;
    let program_info_length = (((sec[10] & 0x0F) as usize) << 8) | (sec[11] as usize);

    let data_start = 12 + program_info_length;
    let data_end = (3 + section_length).min(sec.len()).saturating_sub(4);

    let mut streams = Vec::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = sec[pos];
        let pid = ((sec[pos + 1] as u16 & 0x1F) << 8) | sec[pos + 2] as u16;
        let es_info_length = (((sec[pos + 3] & 0x0F) as usize) << 8) | (sec[pos + 4] as usize);
        // Clamp the descriptor slice to the section body — a corrupt
        // es_info_length must not read into the CRC or past the buffer.
        let es_start = pos + 5;
        let es_end = (es_start + es_info_length).min(data_end);
        let descriptors = sec.get(es_start..es_end).map(|s| s.to_vec()).unwrap_or_default();
        streams.push((stream_type, pid, descriptors));
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

    /// Build a PMT section of arbitrary length (valid CRC32) split across
    /// as many 188-byte TS packets as needed — PUSI + pointer_field on the
    /// first, sequential CC throughout.
    fn build_pmt_multi(
        pmt_pid: u16,
        streams: &[(u8, u16)],
        pcr_pid: u16,
        version: u8,
        start_cc: u8,
    ) -> Vec<[u8; TS_PACKET_SIZE]> {
        let section_data_len = 9 + 5 * streams.len() + 4;
        let mut sec = Vec::with_capacity(3 + section_data_len);
        sec.push(0x02);
        sec.push(0xB0 | (((section_data_len >> 8) as u8) & 0x0F));
        sec.push((section_data_len & 0xFF) as u8);
        sec.extend_from_slice(&[0x00, 0x01]);
        sec.push(0xC1 | ((version & 0x1F) << 1));
        sec.push(0x00);
        sec.push(0x00);
        sec.push(0xE0 | (((pcr_pid >> 8) as u8) & 0x1F));
        sec.push((pcr_pid & 0xFF) as u8);
        sec.push(0xF0);
        sec.push(0x00);
        for (st, pid) in streams {
            sec.push(*st);
            sec.push(0xE0 | (((pid >> 8) as u8) & 0x1F));
            sec.push((pid & 0xFF) as u8);
            sec.push(0xF0);
            sec.push(0x00);
        }
        let crc = mpeg2_crc32(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());

        let mut pkts = Vec::new();
        let mut off = 0usize;
        let mut cc = start_cc;
        let mut first = true;
        while off < sec.len() {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];
            pkt[0] = TS_SYNC_BYTE;
            pkt[1] = (if first { 0x40 } else { 0x00 }) | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[2] = (pmt_pid & 0xFF) as u8;
            pkt[3] = 0x10 | (cc & 0x0F);
            let payload_start = if first {
                pkt[4] = 0x00; // pointer_field
                5
            } else {
                4
            };
            let n = (TS_PACKET_SIZE - payload_start).min(sec.len() - off);
            pkt[payload_start..payload_start + n].copy_from_slice(&sec[off..off + n]);
            off += n;
            cc = (cc + 1) & 0x0F;
            first = false;
            pkts.push(pkt);
        }
        pkts
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
            sender_timestamp_us: None,
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
    fn catalog_reassembles_multi_packet_pmt() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();
        drive(&mut state, &store, &build_pat(&[(1, 0x100)], 0));

        // 40 ES entries → 216-byte section → spans 2 TS packets. This is
        // the shape the old single-packet parser permanently dropped.
        let streams: Vec<(u8, u16)> = (0..40).map(|i| (0x0F, 0x101 + i as u16)).collect();
        let pkts = build_pmt_multi(0x100, &streams, 0x101, 0, 0);
        assert!(pkts.len() >= 2, "section must span multiple packets");
        for p in &pkts {
            drive(&mut state, &store, p);
        }

        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs[0].streams.len(), 40);
        assert_eq!(cat.programs[0].pcr_pid, Some(0x101));
        assert_eq!(cat.programs[0].streams[39].pid, 0x101 + 39);
    }

    #[test]
    fn multi_packet_pmt_cc_gap_aborts_then_recovers() {
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();
        drive(&mut state, &store, &build_pat(&[(1, 0x100)], 0));

        // 80 ES entries → 416-byte section → 3 TS packets.
        let streams: Vec<(u8, u16)> = (0..80).map(|i| (0x0F, 0x101 + i as u16)).collect();
        let pkts = build_pmt_multi(0x100, &streams, 0x101, 0, 0);
        assert!(pkts.len() >= 3, "section must span three packets");

        // Drop the middle packet — the partial must abort, not latch garbage.
        drive(&mut state, &store, &pkts[0]);
        drive(&mut state, &store, &pkts[2]);
        let cat = store.load().expect("catalog present");
        assert!(cat.programs[0].streams.is_empty(), "CC gap must not produce streams");

        // The next complete emission (fresh CC run) parses fine.
        let pkts2 = build_pmt_multi(0x100, &streams, 0x101, 0, 4);
        for p in &pkts2 {
            drive(&mut state, &store, p);
        }
        let cat = store.load().expect("catalog present");
        assert_eq!(cat.programs[0].streams.len(), 80);
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

    // ── descriptor-aware 0x06 classification ───────────────────────────

    /// Build a raw PMT section (not a TS packet) with per-ES descriptor
    /// loops, CRC appended, for `parse_pmt_from_section`.
    fn build_pmt_section(streams: &[(u8, u16, &[u8])], pcr_pid: u16, version: u8) -> Vec<u8> {
        let es_total: usize = streams.iter().map(|(_, _, d)| 5 + d.len()).sum();
        let section_length = 9 + es_total + 4;
        let mut sec = Vec::new();
        sec.push(0x02);
        sec.push(0xB0 | (((section_length >> 8) as u8) & 0x0F));
        sec.push((section_length & 0xFF) as u8);
        sec.extend_from_slice(&[0x00, 0x01]); // program_number
        sec.push(0xC1 | ((version & 0x1F) << 1));
        sec.extend_from_slice(&[0x00, 0x00]); // section/last_section
        sec.push(0xE0 | (((pcr_pid >> 8) as u8) & 0x1F));
        sec.push((pcr_pid & 0xFF) as u8);
        sec.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0
        for (st, pid, desc) in streams {
            sec.push(*st);
            sec.push(0xE0 | (((pid >> 8) as u8) & 0x1F));
            sec.push((pid & 0xFF) as u8);
            sec.push(0xF0 | (((desc.len() >> 8) as u8) & 0x0F));
            sec.push((desc.len() & 0xFF) as u8);
            sec.extend_from_slice(desc);
        }
        let crc = mpeg2_crc32(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        sec
    }

    #[test]
    fn parse_pmt_section_captures_es_descriptors() {
        let ac3_desc: &[u8] = &[0x6A, 0x01, 0x00]; // DVB AC-3 descriptor
        let sec = build_pmt_section(&[(0x1B, 0x101, &[]), (0x06, 0x102, ac3_desc)], 0x101, 3);
        let (version, pcr_pid, streams) = parse_pmt_from_section(&sec).expect("parses");
        assert_eq!(version, 3);
        assert_eq!(pcr_pid, 0x101);
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0], (0x1B, 0x101, vec![]));
        assert_eq!(streams[1], (0x06, 0x102, ac3_desc.to_vec()));
    }

    #[test]
    fn classify_0x06_descriptor_audio_as_audio() {
        let (codec, kind) = classify_stream(0x06, &[0x6A, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Audio);
        assert_eq!(codec, "AC-3 (DVB)");
        let (codec, kind) = classify_stream(0x06, &[0x7A, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Audio);
        assert_eq!(codec, "E-AC-3 (DVB)");
        let (codec, kind) = classify_stream(0x06, &[0x7C, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Audio);
        assert_eq!(codec, "AAC-LATM (DVB)");
    }

    #[test]
    fn classify_0x06_text_service_wins_over_audio() {
        // Teletext tag present → Subtitle even if an audio tag follows.
        let (_, kind) = classify_stream(0x06, &[0x56, 0x00, 0x6A, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Subtitle);
        // Subtitling descriptor.
        let (_, kind) = classify_stream(0x06, &[0x59, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Subtitle);
    }

    #[test]
    fn classify_0x06_bare_stays_data() {
        let (codec, kind) = classify_stream(0x06, &[]);
        assert_eq!(kind, CatalogStreamKind::Data);
        assert_eq!(codec, "PES private");
        // Unrecognised descriptor → still data.
        let (_, kind) = classify_stream(0x06, &[0x0A, 0x04, 0x65, 0x6E, 0x67, 0x00]);
        assert_eq!(kind, CatalogStreamKind::Data);
    }

    #[test]
    fn hex_codec_round_trips() {
        let bytes = [0x6A, 0x01, 0xFF, 0x00];
        let s = hex_encode(&bytes);
        assert_eq!(s, "6a01ff00");
        assert_eq!(hex_decode(&s).as_deref(), Some(&bytes[..]));
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn end_to_end_0x06_audio_lands_in_catalogue_with_kind_audio() {
        // Full observer path: PAT + single-packet PMT carrying a 0x06 ES
        // with a DVB AC-3 descriptor. This is the Ten.ts shape that made
        // the input invisible to kind-matched bus routing.
        let mut state = ObserverState::default();
        let store = PsiCatalogStore::new();
        let ac3: &[u8] = &[0x6A, 0x00];
        let none: &[u8] = &[];
        let sec = build_pmt_section(&[(0x1B, 0x100, none), (0x06, 0x101, ac3)], 0x100, 0);
        // Wrap the section into one TS packet on PMT PID 0x20.
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | 0x00;
        pkt[2] = 0x20;
        pkt[3] = 0x10;
        pkt[4] = 0x00; // pointer
        pkt[5..5 + sec.len()].copy_from_slice(&sec);

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(&[(1, 0x20)], 0));
        buf.extend_from_slice(&pkt);
        drive(&mut state, &store, &buf);

        let cat = store.load().expect("catalog present");
        let streams = &cat.programs[0].streams;
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[1].stream_type, 0x06);
        assert_eq!(streams[1].kind, CatalogStreamKind::Audio);
        assert_eq!(streams[1].codec, "AC-3 (DVB)");
        assert_eq!(streams[1].descriptors_hex, "6a00");
    }
}
