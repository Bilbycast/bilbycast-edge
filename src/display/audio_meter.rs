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
use std::time::Duration;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use video_codec::AudioDecoderCodec;
use video_engine::AudioDecoder as FfAudioDecoder;

use crate::engine::audio_decode::AacDecoder;
use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    ts_adaptation_field_control, ts_has_payload, ts_pid, ts_pusi, PAT_PID, RTP_HEADER_MIN_SIZE,
    TS_PACKET_SIZE, TS_SYNC_BYTE,
};

use super::audio_bars::{new_shared_meter, prune_stale, update_levels, SharedMeter};

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
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_meter(&mut rx, program_number, snapshot, cancel).await {
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
    snapshot: SharedMeter,
    cancel: CancellationToken,
) -> Result<()> {
    let mut state = MeterState::new(program_number);
    let mut prune_interval = tokio::time::interval(PRUNE_INTERVAL);
    prune_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = prune_interval.tick() => {
                prune_stale(&snapshot, STALE_PID_AFTER);
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => state.process_packet(&packet, &snapshot),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.reset_assemblers();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
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

    fn process_packet(&mut self, packet: &RtpPacket, snapshot: &SharedMeter) {
        let payload = strip_rtp_header(packet);
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= payload.len() {
            let pkt = &payload[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                self.process_ts_packet(pkt, snapshot);
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn process_ts_packet(&mut self, pkt: &[u8], snapshot: &SharedMeter) {
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
                self.parse_pat(payload);
            }
            return;
        }
        if Some(pid) == self.selected_pmt_pid && pusi {
            self.parse_pmt(payload);
            return;
        }
        if let Some(pid_state) = self.audio_pids.get_mut(&pid) {
            pid_state.observe_ts(pusi, payload, snapshot);
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
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
            // so the new PMT can repopulate cleanly.
            self.audio_pids.clear();
            self.selected_pmt_pid = chosen;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
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
            if is_audio_stream_type(stream_type) {
                discovered.insert(es_pid, stream_type);
            }
            i += 5 + es_info_length;
        }
        for (pid, stype) in discovered.iter() {
            self.audio_pids
                .entry(*pid)
                .or_insert_with(|| MeterPidState::new(*pid, *stype));
        }
        self.audio_pids.retain(|pid, _| discovered.contains_key(pid));
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

    fn observe_ts(&mut self, pusi: bool, ts_payload: &[u8], snapshot: &SharedMeter) {
        if pusi {
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
                return;
            }
        }
        match self.stream_type {
            0x0F | 0x11 => self.drain_aac(snapshot),
            0x03 | 0x04 | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x87 | 0x88 | 0x8A
            | 0xC1 | 0xC2 => self.drain_ff(snapshot),
            _ => {}
        }
    }

    fn drain_aac(&mut self, snapshot: &SharedMeter) {
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
                update_levels(&planar, self.pid, self.codec_label, snapshot);
            }
            consume += frame_len;
        }
        if consume > 0 {
            self.pes_buf.drain(..consume);
        }
    }

    fn drain_ff(&mut self, snapshot: &SharedMeter) {
        // Lazily open the FFmpeg-backed decoder. MP2/AC-3/E-AC-3 frame
        // boundaries live inside the PES payload; FFmpeg's parsers
        // resync on the codec sync word, so feed the whole PES blob.
        if self.ff_decoder.is_none() {
            let codec = match self.stream_type {
                0x03 | 0x04 => AudioDecoderCodec::Mp2,
                0x80 | 0x81 | 0xC1 => AudioDecoderCodec::Ac3,
                0x87 | 0xC2 => AudioDecoderCodec::Eac3,
                _ => return,
            };
            match FfAudioDecoder::open(codec) {
                Ok(d) => self.ff_decoder = Some(d),
                Err(_) => {
                    self.pes_buf.clear();
                    self.capturing_pes = false;
                    return;
                }
            }
        }
        let Some(ref mut dec) = self.ff_decoder else {
            return;
        };
        if dec.send_packet(&self.pes_buf, 0).is_ok() {
            while let Ok(frame) = dec.receive_frame() {
                update_levels(&frame.planar, self.pid, self.codec_label, snapshot);
            }
        }
        // Whether the send succeeded or not, this PES is consumed.
        self.pes_buf.clear();
        self.capturing_pes = false;
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

fn codec_label(stream_type: u8) -> &'static str {
    match stream_type {
        0x03 => "MP1",
        0x04 => "MP2",
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
