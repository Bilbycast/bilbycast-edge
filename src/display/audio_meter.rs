// Multi-PID audio metering child task for the local-display output.
//
// Sibling broadcast subscriber. Walks the active program's PMT to
// discover every audio PID, lazily opens the right decoder per PID
// (`AacDecoder` for AAC, `FfAudioDecoder` for MP2 / AC-3 / E-AC-3 /
// Opus), and updates a shared `MeterSnapshot` on every decoded block.
//
// Drop semantics match every other analyser in the edge:
// - `Lagged(n)` resets per-PID PES buffers, otherwise no action — the
//   data path is *never* blocked by the meter.
// - PES buffers are size-capped (256 KB) so a stuck non-PUSI tail
//   never grows unbounded.
// - PIDs that haven't produced a decoded block in 5 s are dropped from
//   the snapshot (handles dynamic PMT changes mid-stream).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use video_engine::AudioDecoder as FfAudioDecoder;

use crate::engine::audio_decode::AacDecoder;
use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    ts_adaptation_field_control, ts_has_payload, ts_pid, ts_pusi, PAT_PID, RTP_HEADER_MIN_SIZE,
    SectionAssembler, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

use super::audio_bars::{
    clear_all_pids, new_shared_meter, remove_pid, silence_stale, update_levels, MeterPublisher,
    SharedMeter,
};

const PES_BUF_CAP: usize = 256 * 1024;
const STALE_PID_AFTER: Duration = Duration::from_secs(5);
const PRUNE_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn the metering child task. The caller hands over its own
/// resubscribed `broadcast::Receiver` (so this child runs as a sibling
/// subscriber — drop-on-`Lagged`, never blocks the data path). The
/// snapshot is shared via the supplied `SharedMeter` (created with
/// [`audio_bars::new_shared_meter`]).
pub fn spawn_audio_meter(
    mut rx: broadcast::Receiver<RtpPacket>,
    program_number: Option<u16>,
    snapshot: SharedMeter,
    cancel: CancellationToken,
    counters: std::sync::Arc<crate::stats::collector::DisplayStatsCounters>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_meter(&mut rx, program_number, snapshot, cancel, counters).await {
            tracing::warn!("display audio-meter exited with error: {e}");
        }
    })
}

/// Convenience constructor for callers that don't already hold a
/// shared snapshot (tests, mostly).
#[allow(dead_code)]
pub fn new_meter() -> SharedMeter {
    new_shared_meter()
}

async fn run_meter(
    rx: &mut broadcast::Receiver<RtpPacket>,
    program_number: Option<u16>,
    shared: SharedMeter,
    cancel: CancellationToken,
    counters: std::sync::Arc<crate::stats::collector::DisplayStatsCounters>,
) -> Result<()> {
    let mut state = MeterState::new(program_number);
    // The publisher owns the working `MeterSnapshot` exclusively, plus
    // the single-slot Arc reclaim buffer that lets consecutive
    // publishes reuse the previously-published `Arc<MeterSnapshot>`
    // when the display task has dropped its read guard. `update_levels`
    // / `prune_stale` mutate `publisher.local` and call
    // `publisher.publish()` — the display task observes only the
    // published Arc, lock-free.
    //
    // `last_published_publishes` snapshots the publisher's internal
    // counter so we can bump the operator-facing
    // `counters.meter_publishes` exactly once per fresh publish, even
    // when the publisher's reclaim path returns the existing Arc
    // unchanged (no fresh `publish()` call) — this gives the operator
    // a real "is the meter alive?" signal.
    let mut publisher = MeterPublisher::new(shared);
    let mut last_publish_count = publisher.publish_count();
    let mut prune_interval = tokio::time::interval(PRUNE_INTERVAL);
    prune_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = prune_interval.tick() => {
                // Silence rather than remove: stale PIDs keep their
                // label + empty bar slots so brief audio decoder gaps
                // don't blink the entire confidence strip off. Genuine
                // PID removal happens below in `parse_pat` /
                // `parse_pmt` when the PSI explicitly drops a PID.
                silence_stale(&mut publisher, STALE_PID_AFTER);
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => state.process_packet(&packet, &mut publisher),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.reset_assemblers();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        let now_count = publisher.publish_count();
        if now_count > last_publish_count {
            counters
                .meter_publishes
                .fetch_add(now_count - last_publish_count, Ordering::Relaxed);
            last_publish_count = now_count;
        }
    }
    Ok(())
}

// PSI section reassembly lives in `engine::ts_parse::SectionAssembler`
// — shared with `ts_demux` so both TS pipelines tolerate PAT / PMT
// sections spanning multiple TS packets (real broadcast MPTS muxes).

struct MeterState {
    target_program: Option<u16>,
    /// Map of program_number → PMT PID, populated from the latest PAT.
    pmt_pids: HashMap<u16, u16>,
    /// Selected program (lowest in PAT when `target_program` is None,
    /// otherwise the requested program). `None` until a PAT is parsed.
    selected_pmt_pid: Option<u16>,
    /// Per-PID metering state (decoder + PES buffer + codec label).
    audio_pids: HashMap<u16, MeterPidState>,
    /// Cross-packet section reassembly for the PAT / selected PMT.
    pat_section: SectionAssembler,
    pmt_section: SectionAssembler,
}

impl MeterState {
    fn new(target_program: Option<u16>) -> Self {
        Self {
            target_program,
            pmt_pids: HashMap::new(),
            selected_pmt_pid: None,
            audio_pids: HashMap::new(),
            pat_section: SectionAssembler::new(),
            pmt_section: SectionAssembler::new(),
        }
    }

    fn reset_assemblers(&mut self) {
        for pid in self.audio_pids.values_mut() {
            pid.pes_buf.clear();
            pid.capturing_pes = false;
        }
        // A broadcast Lagged dropped packets — any half-assembled PSI
        // section is no longer continuable.
        self.pat_section.reset();
        self.pmt_section.reset();
    }

    fn process_packet(
        &mut self,
        packet: &RtpPacket,
        publisher: &mut MeterPublisher,
    ) {
        let payload = strip_rtp_header(packet);
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= payload.len() {
            let pkt = &payload[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                self.process_ts_packet(pkt, publisher);
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn process_ts_packet(
        &mut self,
        pkt: &[u8],
        publisher: &mut MeterPublisher,
    ) {
        let pid = ts_pid(pkt);
        let pusi = ts_pusi(pkt);
        let adaptation = ts_adaptation_field_control(pkt);
        if !ts_has_payload(pkt) {
            return;
        }
        let payload_offset = if adaptation & 0x02 != 0 {
            5 + pkt[4] as usize
        } else {
            4
        };
        if payload_offset >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_offset..];
        if pid == PAT_PID {
            if let Some(section) = self.pat_section.feed(pusi, payload) {
                let section = section.to_vec();
                self.parse_pat(&section, publisher);
            }
            return;
        }
        if Some(pid) == self.selected_pmt_pid {
            if let Some(section) = self.pmt_section.feed(pusi, payload) {
                let section = section.to_vec();
                self.parse_pmt(&section, publisher);
            }
            return;
        }
        if let Some(pid_state) = self.audio_pids.get_mut(&pid) {
            pid_state.observe_ts(pusi, payload, publisher);
        }
    }

    /// Parse a complete PAT section (starting at `table_id`), as
    /// delivered by [`SectionAssembler::feed`].
    fn parse_pat(&mut self, section: &[u8], publisher: &mut MeterPublisher) {
        if section.len() < 12 || section[0] != 0x00 {
            return;
        }
        let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        if 3 + section_length > section.len() || section_length < 9 {
            return;
        }
        let body_end = 3 + section_length - 4;
        let mut i = 8;
        self.pmt_pids.clear();
        while i + 4 <= body_end {
            let program_number = ((section[i] as u16) << 8) | section[i + 1] as u16;
            let pmt_pid = (((section[i + 2] as u16) & 0x1F) << 8) | section[i + 3] as u16;
            if program_number != 0 {
                self.pmt_pids.insert(program_number, pmt_pid);
            }
            i += 4;
        }
        // Pick the program: requested, or the lowest if unspecified.
        let chosen = match self.target_program {
            Some(prog) => self.pmt_pids.get(&prog).copied(),
            None => self
                .pmt_pids
                .iter()
                .min_by_key(|(prog, _)| *prog)
                .map(|(_, pid)| *pid),
        };
        if chosen != self.selected_pmt_pid {
            // Program changed (or first lock) — drop all per-PID state
            // so the new PMT can repopulate cleanly. The published
            // snapshot is cleared explicitly here (rather than waiting
            // for silence_stale to coast the entries down) because a
            // program change is a hard semantic boundary: the labels
            // from the previous program are no longer meaningful.
            self.audio_pids.clear();
            self.selected_pmt_pid = chosen;
            clear_all_pids(publisher);
        }
    }

    /// Parse a complete PMT section (starting at `table_id`), as
    /// delivered by [`SectionAssembler::feed`].
    fn parse_pmt(&mut self, section: &[u8], publisher: &mut MeterPublisher) {
        if section.len() < 16 || section[0] != 0x02 {
            return;
        }
        let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        if 3 + section_length > section.len() || section_length < 13 {
            return;
        }
        let program_info_length =
            (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
        let es_start = 12 + program_info_length;
        let body_end = 3 + section_length - 4;
        if es_start >= body_end {
            return;
        }

        let mut discovered: HashMap<u16, u8> = HashMap::new();
        let mut i = es_start;
        while i + 5 <= body_end {
            let stream_type = section[i];
            let es_pid = (((section[i + 1] as u16) & 0x1F) << 8) | section[i + 2] as u16;
            let es_info_length =
                (((section[i + 3] as usize) & 0x0F) << 8) | section[i + 4] as usize;
            // For `stream_type = 0x06` (PES private_data) the actual codec
            // is signalled by an ES-info descriptor — see
            // `ts_demux::detect_private_audio_descriptor`. Parse the
            // descriptor loop and remap the stream_type to the
            // synthetic ATSC-style marker the meter dispatch + decoder
            // already understand:
            //   tag 0x6A (DVB AC-3 desc)            → 0x81 (AC-3)
            //   tag 0x7A (DVB E-AC-3 desc)          → 0x87 (E-AC-3)
            //   tag 0xAC or reg "AC-4"              → skip (no decoder)
            //   tag 0x05 reg "Opus"                 → keep 0x06 (drain_ff
            //                                          dispatches via Opus)
            // Without this, every DVB-T capture (Ten.ts, BBC, ARD…)
            // carrying AC-3 on stream_type 0x06 was invisible to the
            // meter and the audio bars stayed empty.
            let descriptor_end = (i + 5 + es_info_length).min(body_end);
            let effective_type = if stream_type == 0x06 {
                resolve_private_audio_stream_type(&section[i + 5..descriptor_end])
            } else {
                Some(stream_type)
            };
            if let Some(st) = effective_type {
                if is_audio_stream_type(st) {
                    discovered.insert(es_pid, st);
                }
            }
            i += 5 + es_info_length;
        }
        for (pid, stype) in discovered.iter() {
            match self.audio_pids.get(pid) {
                // Same PID, same codec — keep the warmed-up decoder +
                // PES assembly state.
                Some(existing) if existing.stream_type == *stype => {}
                // New PID, or the SAME PID re-announced with a
                // DIFFERENT codec. The latter is the normal shape of an
                // input switch on this edge: remuxed SPTS sources all
                // put audio on the same PID (ffmpeg defaults to 0x101),
                // so an AAC → AC-3 switch arrives as "0x101 changed
                // stream_type". The old `entry().or_insert_with()`
                // kept the stale decoder, which then scanned the new
                // codec's bytes for the old codec's sync words forever
                // — the operator saw the bars freeze empty after every
                // cross-codec switch (2026-06-11). Replacing the state
                // drops the stale decoder + PES buffer so the next PES
                // on the PID relatches cleanly.
                _ => {
                    self.audio_pids
                        .insert(*pid, MeterPidState::new(*pid, *stype));
                }
            }
        }
        // Drop the PMT-removed PIDs from the published snapshot
        // explicitly so the operator sees them disappear at the moment
        // the PMT updated — silence_stale would otherwise leave the
        // labels coasting at the dBFS floor until the next PMT cycle.
        let removed: Vec<u16> = self
            .audio_pids
            .keys()
            .copied()
            .filter(|pid| !discovered.contains_key(pid))
            .collect();
        self.audio_pids.retain(|pid, _| discovered.contains_key(pid));
        for pid in removed {
            remove_pid(publisher, pid);
        }
    }
}

struct MeterPidState {
    pid: u16,
    stream_type: u8,
    codec_label: &'static str,
    aac_decoder: Option<AacDecoder>,
    ff_decoder: Option<FfAudioDecoder>,
    pes_buf: Vec<u8>,
    capturing_pes: bool,
}

impl MeterPidState {
    fn new(pid: u16, stream_type: u8) -> Self {
        Self {
            pid,
            stream_type,
            codec_label: codec_label(stream_type),
            aac_decoder: None,
            ff_decoder: None,
            pes_buf: Vec::with_capacity(16_384),
            capturing_pes: false,
        }
    }

    fn observe_ts(
        &mut self,
        pusi: bool,
        ts_payload: &[u8],
        publisher: &mut MeterPublisher,
    ) {
        if pusi {
            // The next PES is starting — drain whatever the previous PES
            // accumulated. MP2 / AC-3 / E-AC-3 frames routinely span
            // multiple TS packets, so per-packet draining (the old
            // behaviour) would feed the FFmpeg decoder a 178-byte
            // fragment and silently throw away every continuation
            // packet. Draining on the PUSI boundary guarantees the
            // decoder sees complete frames.
            if self.capturing_pes && !self.pes_buf.is_empty() {
                self.drain(publisher);
            }
            let start = pes_payload_offset(ts_payload);
            self.pes_buf.clear();
            self.capturing_pes = true;
            if start < ts_payload.len() {
                self.pes_buf.extend_from_slice(&ts_payload[start..]);
            }
        } else if self.capturing_pes {
            self.pes_buf.extend_from_slice(ts_payload);
            if self.pes_buf.len() > PES_BUF_CAP {
                // Runaway buffer: drop and resync on next PUSI.
                self.pes_buf.clear();
                self.capturing_pes = false;
            }
        }
    }

    fn drain(&mut self, publisher: &mut MeterPublisher) {
        match self.stream_type {
            // ADTS-framed AAC — fdk-aac via the in-house `AacDecoder`.
            0x0F => self.drain_aac(publisher),
            // LATM/LOAS-framed AAC — libavcodec's `AAC_LATM` decoder.
            // ADTS sync `0xFFF` never appears at LATM frame boundaries
            // (sync is `0x2B7` → bytes `0x56 0xE0…`), so routing 0x11
            // through `drain_aac` would walk the PES byte-by-byte
            // forever and never publish a level — bars stay empty for
            // every Brazilian / Asian / Australian DVB-T AAC service.
            0x11 => self.drain_ff(publisher),
            // Opus on `stream_type = 0x06` — `parse_pmt` only routes
            // 0x06 here when the registration descriptor flagged it as
            // Opus (DVB AC-3 / E-AC-3 are remapped to 0x81 / 0x87 in
            // the PMT pass). drain_ff dispatches via Opus.
            0x06 => self.drain_ff(publisher),
            0x03 | 0x04 | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x87 | 0x88 | 0x8A
            | 0xC1 | 0xC2 => self.drain_ff(publisher),
            _ => {}
        }
    }

    fn drain_aac(&mut self, publisher: &mut MeterPublisher) {
        let mut consume = 0;
        loop {
            let buf = &self.pes_buf[consume..];
            if buf.len() < 7 {
                break;
            }
            if !(buf[0] == 0xFF && (buf[1] & 0xF0) == 0xF0) {
                consume += 1;
                continue;
            }
            let frame_len = (((buf[3] as usize) & 0x03) << 11)
                | ((buf[4] as usize) << 3)
                | ((buf[5] as usize) >> 5);
            if frame_len < 7 || frame_len > buf.len() {
                break;
            }
            let has_crc = (buf[1] & 0x01) == 0;
            let header_len = if has_crc { 9 } else { 7 };
            if frame_len <= header_len {
                consume += frame_len;
                continue;
            }
            let profile = (buf[2] >> 6) & 0x03;
            let sr_index = (buf[2] >> 2) & 0x0F;
            let channel_config = ((buf[2] & 0x01) << 2) | ((buf[3] >> 6) & 0x03);

            if self.aac_decoder.is_none()
                && channel_config >= 1
                && channel_config <= 7
                && let Ok(dec) = AacDecoder::from_adts_config(profile, sr_index, channel_config)
            {
                self.aac_decoder = Some(dec);
            }

            let frame = &buf[header_len..frame_len];
            if let Some(ref mut dec) = self.aac_decoder
                && let Ok(planar) = dec.decode_frame(frame)
            {
                update_levels(&planar, self.pid, self.codec_label, publisher);
            }
            consume += frame_len;
        }
        if consume > 0 {
            self.pes_buf.drain(..consume);
        }
    }

    fn drain_ff(&mut self, publisher: &mut MeterPublisher) {
        // Lazily open the FFmpeg-backed decoder.
        let Some(codec) =
            crate::engine::audio_decode::ff_codec_for_stream_type(self.stream_type)
        else {
            return;
        };
        if self.ff_decoder.is_none() {
            match FfAudioDecoder::open(codec) {
                Ok(d) => self.ff_decoder = Some(d),
                Err(_) => return,
            }
        }
        let Some(ref mut dec) = self.ff_decoder else {
            return;
        };
        // A PES typically carries 2–6 audio frames concatenated. ffmpeg
        // `avcodec_send_packet` decodes only one access unit per call —
        // feeding the whole blob silently throws away every frame past
        // the first, so the audio-bars overlay never sees recent levels.
        for frame_slice in
            crate::engine::audio_decode::split_audio_codec_frames(&self.pes_buf, codec)
        {
            if dec.send_packet(frame_slice, 0).is_ok() {
                while let Ok(frame) = dec.receive_frame() {
                    update_levels(&frame.planar, self.pid, self.codec_label, publisher);
                }
            }
        }
        // pes_buf is cleared by the caller (`observe_ts` on the next
        // PUSI). Don't reset state here — it would discard continuation
        // packets and starve the decoder.
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn strip_rtp_header(packet: &RtpPacket) -> &[u8] {
    let data = packet.data.as_ref();
    if packet.is_raw_ts {
        return data;
    }
    if data.len() < RTP_HEADER_MIN_SIZE {
        return &[];
    }
    let csrc_count = (data[0] & 0x0F) as usize;
    let ext_flag = (data[0] & 0x10) != 0;
    let mut offset = RTP_HEADER_MIN_SIZE + 4 * csrc_count;
    if ext_flag && offset + 4 <= data.len() {
        let ext_len_words = ((data[offset + 2] as usize) << 8) | data[offset + 3] as usize;
        offset += 4 + 4 * ext_len_words;
    }
    if offset > data.len() {
        return &[];
    }
    &data[offset..]
}

fn pes_payload_offset(payload: &[u8]) -> usize {
    if payload.len() < 9 {
        return 0;
    }
    if payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
        return 0;
    }
    // 0xC0–0xDF: MPEG audio stream ids (MP2 / AAC). 0xBD:
    // private_stream_1 — how DVB / ATSC carry AC-3, E-AC-3, DTS and
    // LPCM (same PES header layout). Without 0xBD every AC-3 PES
    // header rode into the ES buffer as leading garbage and the
    // sync-word splitter had to resync past it on every PES.
    let stream_id = payload[3];
    if !((0xC0..=0xEF).contains(&stream_id) || stream_id == 0xBD) {
        return 0;
    }
    let hdr_len = payload[8] as usize;
    let es_start = 9 + hdr_len;
    if es_start > payload.len() {
        return payload.len();
    }
    es_start
}

fn is_audio_stream_type(st: u8) -> bool {
    matches!(
        st,
        0x03 | 0x04
            | 0x06   // DVB Opus carrier (resolved via descriptor in parse_pmt)
            | 0x0F
            | 0x11
            | 0x80
            | 0x81
            | 0x82
            | 0x83
            | 0x84
            | 0x85
            | 0x87
            | 0x88
            | 0x8A
            | 0xC1
            | 0xC2
    )
}

/// Inspect the ES-info descriptor loop for an audio codec carried on
/// `stream_type = 0x06` (PES private_data). Returns the synthetic
/// stream_type the meter's `drain` dispatch + `ff_codec_for_stream_type`
/// can consume, or `None` for non-audio private streams (e.g. ARIB
/// caption / DVB subtitling carried on the same private-data marker)
/// and for audio families this binary has no decoder for (AC-4, DTS,
/// SMPTE 302M).
///
/// Delegates to the shared `ts_parse::descriptor_audio_kind` — the same
/// classifier the PSI catalogue, the pid-override rewriter, and the PTS
/// rewriter use — so the meter recognises every signalling style they
/// do. The hand-rolled parser this replaces missed the
/// `registration_descriptor` forms (`"AC-3"` / `"EAC3"` / `"DTS1"`…),
/// which is exactly what an ffmpeg `mpegts` re-mux emits for E-AC-3:
/// the meter classified the PID as non-audio, dropped it from the
/// snapshot, and the operator watched the bars strip go empty after
/// every Take onto such a source (2026-06-11, S4 / France-mux feed).
fn resolve_private_audio_stream_type(descriptors: &[u8]) -> Option<u8> {
    use crate::engine::ts_parse::PrivateEsAudioKind;
    match crate::engine::ts_parse::descriptor_audio_kind(descriptors)? {
        PrivateEsAudioKind::Ac3 => Some(0x81),
        PrivateEsAudioKind::Eac3 => Some(0x87),
        // LATM/LOAS AAC — drain dispatch routes 0x11 through drain_ff
        // (libavcodec AAC_LATM); ADTS sync-scan would never lock.
        PrivateEsAudioKind::AacLatm => Some(0x11),
        // drain_ff routes bare 0x06 via Opus.
        PrivateEsAudioKind::Opus => Some(0x06),
        // No decoder in this binary — keep the PID off the meter so we
        // don't open a libavcodec context that can't produce levels.
        PrivateEsAudioKind::Ac4
        | PrivateEsAudioKind::Dts
        | PrivateEsAudioKind::Smpte302m => None,
    }
}

fn codec_label(stream_type: u8) -> &'static str {
    match stream_type {
        0x03 => "MP1",
        0x04 => "MP2",
        0x06 => "OPUS",
        0x0F => "AAC",
        0x11 => "AAC",
        0x80 | 0x81 | 0xC1 => "AC3",
        0x82 => "DTS",
        0x83 => "LPCM",
        0x87 | 0xC2 => "EAC3",
        _ => "AUD",
    }
}

// Suppress unused-Arc lint when feature compiled as a no-op shim.
#[allow(dead_code)]
fn _hold_arc(_: Arc<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal PMT section: one ES entry, no descriptors, fake CRC.
    fn pmt_section(stream_type: u8, es_pid: u16) -> Vec<u8> {
        let es_loop = vec![
            stream_type,
            0xE0 | ((es_pid >> 8) as u8 & 0x1F),
            (es_pid & 0xFF) as u8,
            0xF0,
            0x00, // es_info_length = 0
        ];
        // section_length = fixed-after-length (9) + es_loop + CRC (4)
        let section_length = 9 + es_loop.len() + 4;
        let mut s = vec![
            0x02,
            0xB0 | ((section_length >> 8) as u8 & 0x0F),
            (section_length & 0xFF) as u8,
            0x00,
            0x01, // program_number 1
            0xC1, // version/current_next
            0x00,
            0x00, // section / last_section
            0xE1,
            0x00, // PCR PID
            0xF0,
            0x00, // program_info_length = 0
        ];
        s.extend_from_slice(&es_loop);
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC (not validated)
        s
    }

    /// An input switch that re-announces the SAME audio PID with a
    /// DIFFERENT codec must replace the per-PID state (decoder +
    /// framing), not keep the stale one — the keep-stale behaviour left
    /// the bars empty after every cross-codec switch.
    #[test]
    fn pmt_codec_change_on_same_pid_replaces_state() {
        let mut state = MeterState::new(None);
        let mut publisher = MeterPublisher::new(new_shared_meter());
        state.parse_pmt(&pmt_section(0x0F, 0x101), &mut publisher);
        assert_eq!(state.audio_pids.get(&0x101).unwrap().stream_type, 0x0F);
        // Same codec re-announce: state object is kept.
        state.parse_pmt(&pmt_section(0x0F, 0x101), &mut publisher);
        assert_eq!(state.audio_pids.get(&0x101).unwrap().stream_type, 0x0F);
        // Cross-codec switch (AAC → AC-3) on the same PID: replaced.
        state.parse_pmt(&pmt_section(0x81, 0x101), &mut publisher);
        assert_eq!(state.audio_pids.get(&0x101).unwrap().stream_type, 0x81);
        assert_eq!(state.audio_pids.len(), 1);
    }

    /// Like [`pmt_section`] but with an ES-info descriptor loop, so the
    /// tests can express DVB/ffmpeg-style `stream_type = 0x06` audio.
    fn pmt_section_with_descs(stream_type: u8, es_pid: u16, descs: &[u8]) -> Vec<u8> {
        let mut es_loop = vec![
            stream_type,
            0xE0 | ((es_pid >> 8) as u8 & 0x1F),
            (es_pid & 0xFF) as u8,
            0xF0 | ((descs.len() >> 8) as u8 & 0x0F),
            (descs.len() & 0xFF) as u8,
        ];
        es_loop.extend_from_slice(descs);
        let section_length = 9 + es_loop.len() + 4;
        let mut s = vec![
            0x02,
            0xB0 | ((section_length >> 8) as u8 & 0x0F),
            (section_length & 0xFF) as u8,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0xE1,
            0x00,
            0xF0,
            0x00,
        ];
        s.extend_from_slice(&es_loop);
        s.extend_from_slice(&[0, 0, 0, 0]);
        s
    }

    /// `stream_type = 0x06` audio signalled ONLY via a registration
    /// descriptor (the shape ffmpeg's mpegts muxer emits for E-AC-3,
    /// and what the assembler copy-through forwards) must be metered.
    /// The pre-fix parser recognised only DVB 0x6A/0x7A tags — the PID
    /// was classified non-audio, dropped from the snapshot, and the
    /// operator watched the bars strip go blank after a Take onto such
    /// a source, indistinguishable from "overlay died" (2026-06-11).
    #[test]
    fn registration_descriptor_audio_is_discovered() {
        let mut state = MeterState::new(None);
        let mut publisher = MeterPublisher::new(new_shared_meter());
        // reg "EAC3" + ISO-639 'fre' — the captured assembled-PMT shape.
        let descs: &[u8] = &[
            0x05, 0x04, b'E', b'A', b'C', b'3', // registration "EAC3"
            0x0A, 0x04, b'f', b'r', b'e', 0x00, // ISO-639 language
        ];
        state.parse_pmt(&pmt_section_with_descs(0x06, 0x102, descs), &mut publisher);
        let pid = state.audio_pids.get(&0x102).expect("PID must be discovered");
        assert_eq!(pid.stream_type, 0x87, "reg EAC3 must map to E-AC-3");

        // reg "AC-3" likewise.
        let mut state = MeterState::new(None);
        let descs: &[u8] = &[0x05, 0x04, b'A', b'C', b'-', b'3'];
        state.parse_pmt(&pmt_section_with_descs(0x06, 0x103, descs), &mut publisher);
        assert_eq!(state.audio_pids.get(&0x103).unwrap().stream_type, 0x81);

        // DVB AAC descriptor (0x7C) → LATM routing.
        let mut state = MeterState::new(None);
        let descs: &[u8] = &[0x7C, 0x01, 0x00];
        state.parse_pmt(&pmt_section_with_descs(0x06, 0x104, descs), &mut publisher);
        assert_eq!(state.audio_pids.get(&0x104).unwrap().stream_type, 0x11);

        // AC-4 (no decoder) must stay OFF the meter.
        let mut state = MeterState::new(None);
        let descs: &[u8] = &[0x05, 0x04, b'A', b'C', b'-', b'4'];
        state.parse_pmt(&pmt_section_with_descs(0x06, 0x105, descs), &mut publisher);
        assert!(state.audio_pids.is_empty(), "AC-4 must not be metered");
    }

    /// A PSI section spanning multiple TS packets must reassemble; the
    /// old parse-PUSI-packet-only behaviour dropped every PMT longer
    /// than one packet (real MPTS muxes), leaving zero audio PIDs.
    #[test]
    fn section_assembler_reassembles_multi_packet_sections() {
        let mut asm = SectionAssembler::new();
        // 300-byte section_length → 303 total bytes.
        let mut section = vec![0x02, 0xB1, 0x2C];
        section.resize(303, 0xAB);
        // PUSI packet carries pointer_field 0 + first 183 bytes.
        let mut first = vec![0u8];
        first.extend_from_slice(&section[..183]);
        assert!(asm.feed(true, &first).is_none());
        // Continuation completes it.
        let got = asm.feed(false, &section[183..]).expect("complete").to_vec();
        assert_eq!(got.len(), 303);
        assert_eq!(got[..3], section[..3]);

        // Single-packet section completes immediately (with pointer).
        let small = pmt_section(0x0F, 0x101);
        let mut pkt = vec![0u8];
        pkt.extend_from_slice(&small);
        let got = asm.feed(true, &pkt).expect("single-packet complete");
        assert_eq!(got.len(), small.len());

        // Continuation without a PUSI start is ignored.
        asm.reset();
        assert!(asm.feed(false, &[0xAB; 100]).is_none());
    }

    /// AC-3 / E-AC-3 / DTS ride PES private_stream_1 (0xBD); the header
    /// must be stripped exactly like the 0xC0–0xEF MPEG audio ids.
    #[test]
    fn pes_payload_offset_handles_private_stream_1() {
        // 00 00 01 BD len len flags flags hdr_len(5) [5 hdr bytes] ES…
        let pes = [
            0x00, 0x00, 0x01, 0xBD, 0x00, 0x20, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00,
            0x01, 0x0B, 0x77, 0xAA,
        ];
        assert_eq!(pes_payload_offset(&pes), 14);
        assert_eq!(pes[pes_payload_offset(&pes)], 0x0B); // AC-3 sync starts the ES
        // MPEG audio id still works.
        let mut mp2 = pes;
        mp2[3] = 0xC0;
        assert_eq!(pes_payload_offset(&mp2), 14);
        // Video-range id is accepted (existing behaviour), unknown ids are not.
        let mut other = pes;
        other[3] = 0xBE; // padding_stream
        assert_eq!(pes_payload_offset(&other), 0);
    }
}
