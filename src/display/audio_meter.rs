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
    TS_PACKET_SIZE, TS_SYNC_BYTE,
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

struct MeterState {
    target_program: Option<u16>,
    /// Map of program_number → PMT PID, populated from the latest PAT.
    pmt_pids: HashMap<u16, u16>,
    /// Selected program (lowest in PAT when `target_program` is None,
    /// otherwise the requested program). `None` until a PAT is parsed.
    selected_pmt_pid: Option<u16>,
    /// Per-PID metering state (decoder + PES buffer + codec label).
    audio_pids: HashMap<u16, MeterPidState>,
}

impl MeterState {
    fn new(target_program: Option<u16>) -> Self {
        Self {
            target_program,
            pmt_pids: HashMap::new(),
            selected_pmt_pid: None,
            audio_pids: HashMap::new(),
        }
    }

    fn reset_assemblers(&mut self) {
        for pid in self.audio_pids.values_mut() {
            pid.pes_buf.clear();
            pid.capturing_pes = false;
        }
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
            if pusi {
                self.parse_pat(payload, publisher);
            }
            return;
        }
        if Some(pid) == self.selected_pmt_pid && pusi {
            self.parse_pmt(payload, publisher);
            return;
        }
        if let Some(pid_state) = self.audio_pids.get_mut(&pid) {
            pid_state.observe_ts(pusi, payload, publisher);
        }
    }

    fn parse_pat(&mut self, payload: &[u8], publisher: &mut MeterPublisher) {
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        if 1 + pointer + 8 > payload.len() {
            return;
        }
        let section = &payload[1 + pointer..];
        if section.is_empty() || section[0] != 0x00 {
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

    fn parse_pmt(&mut self, payload: &[u8], publisher: &mut MeterPublisher) {
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        if 1 + pointer + 12 > payload.len() {
            return;
        }
        let section = &payload[1 + pointer..];
        if section.is_empty() || section[0] != 0x02 {
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
            self.audio_pids
                .entry(*pid)
                .or_insert_with(|| MeterPidState::new(*pid, *stype));
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
    let stream_id = payload[3];
    if !(0xC0..=0xEF).contains(&stream_id) {
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
/// caption / DVB subtitling carried on the same private-data marker).
///
/// Keeps the same descriptor parser behaviour as
/// `ts_demux::detect_private_audio_descriptor` so the two TS pipelines
/// in this binary surface the same audio PIDs.
fn resolve_private_audio_stream_type(descriptors: &[u8]) -> Option<u8> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            break;
        }
        match tag {
            // DVB AC-3 descriptor — ETSI TS 101 154 § 5.3.
            0x6A => return Some(0x81),
            // DVB E-AC-3 descriptor — ETSI TS 101 154 § 5.3.
            0x7A => return Some(0x87),
            // AC-4 descriptor — ETSI TS 101 154 § 5.7. AC-4 has no
            // open-source decoder; skip from the meter so we don't
            // open a libavcodec decoder that won't decode anything.
            0xAC => return None,
            // Registration descriptor — used for Opus and AC-4.
            0x05 if len >= 4 => {
                let id = &descriptors[pos + 2..pos + 6];
                match id {
                    b"Opus" => return Some(0x06), // drain_ff routes via Opus
                    b"AC-4" => return None,        // no decoder
                    _ => {}
                }
            }
            _ => {}
        }
        pos += 2 + len;
    }
    None
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
