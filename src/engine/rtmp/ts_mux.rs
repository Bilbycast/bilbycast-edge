// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MPEG-TS muxer for H.264 video and AAC audio.
//!
//! Takes raw H.264 NALUs and AAC frames (extracted from RTMP FLV tags)
//! and muxes them into 188-byte MPEG-TS packets suitable for RTP transport
//! or SRT output.
//!
//! ## TS packet structure
//!
//! Each 188-byte TS packet has a 4-byte header:
//! - Sync byte (0x47)
//! - Transport Error Indicator, Payload Unit Start Indicator, Priority
//! - PID (13 bits)
//! - Scrambling, Adaptation field control, Continuity counter (4 bits)
//!
//! We emit:
//! - PID 0x0000: PAT (Program Association Table)
//! - PID 0x1000: PMT (Program Map Table)
//! - PID 0x0100: Video PES (H.264)
//! - PID 0x0101: Audio PES (AAC)
use std::time::Instant;

use bytes::{BufMut, Bytes, BytesMut};

// TS constants
const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
/// Default PMT PID when no override is supplied. Matches ffmpeg's mpegts
/// muxer default.
pub const DEFAULT_PMT_PID: u16 = 0x1000;
/// Default video PES PID when no override is supplied.
pub const DEFAULT_VIDEO_PID: u16 = 0x0100;
/// Default audio PES PID when no override is supplied.
pub const DEFAULT_AUDIO_PID: u16 = 0x0101;
/// Default SCTE-35 section PID.
pub const DEFAULT_SCTE35_PID: u16 = 0x01FC;

/// Maximum wall-clock interval between PAT/PMT emissions.
/// TR-101290 Priority 1 requires PSI tables to repeat at least every
/// 500 ms; 360 ms gives a comfortable margin against jittered call-site
/// cadence. Wall-clock (not media PTS) is the right reference because
/// audio and video may use independent RTP base times so a media-PTS
/// delta isn't a meaningful measure of real time.
const PAT_PMT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(360);

/// H.264 stream type in PMT (ISO 14496-10).
const STREAM_TYPE_H264: u8 = 0x1B;
/// H.265/HEVC stream type in PMT (ITU-T H.265).
pub const STREAM_TYPE_H265: u8 = 0x24;
/// AAC stream type in PMT (ISO 13818-7).
const STREAM_TYPE_AAC: u8 = 0x0F;

/// MPEG-TS muxer state.
pub struct TsMuxer {
    /// PMT PID — `0x1000` by default. Overridable via `set_pids()` so an
    /// operator can pin every transcoded input to the same wire layout
    /// for switcher use (downstream decoders never re-tune).
    pmt_pid: u16,
    /// Video PES PID — `0x0100` by default. Overridable.
    video_pid: u16,
    /// Audio PES PID — `0x0101` by default. Overridable.
    audio_pid: u16,
    scte35_pid: u16,
    /// Optional explicit PCR PID. When `None`, PCR is carried on the
    /// video PID if video is present, otherwise the audio PID — the
    /// historical behaviour. When `Some`, this PID is advertised in the
    /// PMT and used in PCR-bearing TS packets.
    pcr_pid_override: Option<u16>,
    /// Continuity counters per PID.
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    cc_audio: u8,
    cc_scte35: u8,
    /// Whether the stream includes video (affects PMT content).
    has_video: bool,
    /// Whether the stream includes audio (affects PMT content).
    has_audio: bool,
    has_scte35: bool,
    /// Video stream type for PMT (H.264=0x1B, H.265=0x24).
    video_stream_type: u8,
    /// Audio stream type for PMT (default: AAC = 0x0F). Override to 0x06
    /// for SMPTE 302M private PES, or other ISO/IEC 13818-1 stream_type
    /// values.
    audio_stream_type: u8,
    /// Optional 4-byte registration descriptor format_identifier for the
    /// audio elementary stream. Set to `Some(*b"BSSD")` for SMPTE 302M
    /// LPCM. When `None`, no registration descriptor is written.
    audio_registration: Option<[u8; 4]>,
    /// Frame counter for periodic PAT/PMT emission. Used as a fallback
    /// when no time clock is available — see `last_pat_pmt_pts_90khz`
    /// for the time-based primary path.
    pat_pmt_counter: u32,
    /// Whether PAT/PMT have been emitted at least once.
    pat_pmt_sent: bool,
    /// Wall-clock instant of the last PAT/PMT emission. Used to enforce
    /// the [`PAT_PMT_INTERVAL`] cadence required by TR-101290 P1.
    /// `None` means we have not emitted yet.
    last_pat_pmt_at: Option<Instant>,
    /// Last audio PES PTS emitted, in 90 kHz ticks. Used to clamp the
    /// audio elementary stream so the DTS we hand the downstream muxer
    /// is strictly monotonic.
    ///
    /// Why this exists: RTMP carries millisecond-resolution timestamps
    /// per chunk stream, but real AAC frames are spaced 21.333 ms apart
    /// (1024 samples @ 48 kHz). Some publishers (notably ffmpeg with
    /// `-f flv` against an AAC encoder) emit timestamps that occasionally
    /// dip backward by 1 ms when the rounding crosses a boundary. AAC
    /// has no B-frames so PTS == DTS, and any backward step lands as a
    /// "non monotonically increasing dts" error in every downstream
    /// muxer (`tsanalyze`, `ffmpeg -f null`, hardware decoders).
    ///
    /// Clamping at the muxer is the right layer because the same
    /// `TsMuxer` is shared by RTMP, RTSP, and WebRTC inputs (see the
    /// monorepo CLAUDE.md `engine/rtmp/ts_mux.rs` notes), so every
    /// upstream gets the protection without each one re-implementing
    /// the same state.
    last_audio_pts_90khz: Option<u64>,
    /// PMT `version_number` field (5 bits, `0..=31`). Bumped by the caller
    /// across a layout-change boundary (e.g. media-player playlist
    /// transition where the new file's codec/PID layout differs) so
    /// receivers re-parse the PMT instead of caching the previous one.
    /// Default `0`. Receivers re-init their decoder pipeline when this
    /// changes — bump only on a real layout change, not gratuitously.
    pmt_version: u8,
}

impl TsMuxer {
    pub fn new() -> Self {
        Self {
            pmt_pid: DEFAULT_PMT_PID,
            video_pid: DEFAULT_VIDEO_PID,
            audio_pid: DEFAULT_AUDIO_PID,
            scte35_pid: DEFAULT_SCTE35_PID,
            pcr_pid_override: None,
            cc_pat: 0,
            cc_pmt: 0,
            cc_video: 0,
            cc_audio: 0,
            cc_scte35: 0,
            has_video: true,
            has_audio: false,
            has_scte35: false,
            video_stream_type: STREAM_TYPE_H264,
            audio_stream_type: STREAM_TYPE_AAC,
            audio_registration: None,
            pat_pmt_counter: 0,
            pat_pmt_sent: false,
            last_pat_pmt_at: None,
            last_audio_pts_90khz: None,
            pmt_version: 0,
        }
    }

    /// Override one or more output PIDs. Any field left `None` keeps the
    /// current value (defaults: PMT 0x1000, video 0x0100, audio 0x0101,
    /// PCR auto-derived). Call before any `mux_*` invocation; call sites
    /// that change PIDs mid-stream are responsible for bumping the PMT
    /// version (`set_pmt_version`) so receivers re-parse.
    pub fn set_pids(
        &mut self,
        pmt: Option<u16>,
        video: Option<u16>,
        audio: Option<u16>,
        pcr: Option<u16>,
    ) {
        if let Some(p) = pmt {
            self.pmt_pid = p;
        }
        if let Some(p) = video {
            self.video_pid = p;
        }
        if let Some(p) = audio {
            self.audio_pid = p;
        }
        self.pcr_pid_override = pcr;
    }

    /// Seed the per-PID continuity counters so the next outgoing TS packet
    /// on each PID picks up immediately after the value supplied. Used by
    /// the file-backed media-player input across playlist transitions and
    /// loop boundaries: the previous file's last CCs are read back via
    /// [`current_cc`], stashed in `SpliceContinuity`, then seeded into the
    /// fresh `TsMuxer` here so receivers see one continuous CC sequence
    /// rather than a reset to 0 every loop.
    ///
    /// Each argument is the **last CC sent on the wire** for that PID;
    /// the next emitted packet on each PID will use `(value + 1) & 0x0F`.
    /// Pass `0xFF` to indicate "no previous value" (the first emitted
    /// packet then uses CC=0).
    pub fn seed_cc(&mut self, pat: u8, pmt: u8, video: u8, audio: u8) {
        // The internal counters store the value to emit *next*. So if the
        // previous wire value was N, the next-to-emit is (N+1) & 0x0F.
        // 0xFF (or any value with bit 7 set) means "no previous" — emit 0.
        let next = |last: u8| -> u8 {
            if last & 0x80 != 0 {
                0
            } else {
                (last + 1) & 0x0F
            }
        };
        self.cc_pat = next(pat);
        self.cc_pmt = next(pmt);
        self.cc_video = next(video);
        self.cc_audio = next(audio);
    }

    /// Read back the last CC value sent on the wire for each managed PID.
    /// Used by the media-player input at file-close to checkpoint state
    /// into `SpliceContinuity` for the next file. Each value is the most
    /// recent CC bit-nibble *that has been written into a packet header*
    /// — so feeding it back into [`seed_cc`] on a fresh muxer continues
    /// the sequence with no gap and no repeat.
    pub fn current_cc(&self) -> (u8, u8, u8, u8) {
        // Internal counters hold the *next* value to emit, so subtract 1 to
        // recover the last value actually sent (mod 16).
        let last = |next: u8| -> u8 { (next.wrapping_sub(1)) & 0x0F };
        (
            last(self.cc_pat),
            last(self.cc_pmt),
            last(self.cc_video),
            last(self.cc_audio),
        )
    }

    /// Set the PMT `version_number` (5 bits, masked to `0..=31`). Bump
    /// only across a real layout change (codec / PID / audio
    /// added-or-removed) — receivers re-parse the PMT and reset decoder
    /// state, which audibly drops audio for a few hundred ms on Apple
    /// decoders. Caller is responsible for bumping mod 32.
    pub fn set_pmt_version(&mut self, version: u8) {
        self.pmt_version = version & 0x1F;
    }

    /// Clamp an incoming audio PTS so the audio elementary stream PES
    /// timestamps are strictly monotonic. Returns the clamped value and
    /// updates internal state. Exposed at `pub(crate)` so it can be
    /// unit-tested without going through the full TS mux/parse cycle.
    ///
    /// Behaviour:
    /// - First call: passes the value through unchanged.
    /// - Subsequent calls: if `pts_90khz <= last`, returns `last + 1`.
    ///   Otherwise returns `pts_90khz` unchanged.
    ///
    /// We bump by exactly 1 tick (≈11.1 µs at 90 kHz) on a regression
    /// rather than a larger value because the regressions seen in
    /// practice are 1–2 ms at most, and a 1-tick advance is enough to
    /// satisfy the strict-greater-than check in every downstream muxer
    /// without distorting the audio clock more than necessary.
    pub(crate) fn clamp_audio_pts(&mut self, pts_90khz: u64) -> u64 {
        let clamped = match self.last_audio_pts_90khz {
            Some(last) if pts_90khz <= last => last + 1,
            _ => pts_90khz,
        };
        self.last_audio_pts_90khz = Some(clamped);
        clamped
    }

    /// Set whether the stream has video (affects PMT).
    pub fn set_has_video(&mut self, has: bool) {
        self.has_video = has;
    }

    /// Set whether the stream has audio (affects PMT).
    pub fn set_has_audio(&mut self, has: bool) {
        self.has_audio = has;
    }

    /// Set the video stream type for PMT (default: H.264 = 0x1B).
    /// Use `0x24` for H.265/HEVC.
    pub fn set_video_stream_type(&mut self, stream_type: u8) {
        self.video_stream_type = stream_type;
    }

    /// Set the audio elementary stream type and optional registration
    /// descriptor format_identifier. For SMPTE 302M:
    ///
    /// ```ignore
    /// muxer.set_audio_stream(0x06, Some(*b"BSSD"));
    /// ```
    ///
    /// Default is AAC (`0x0F`, no registration descriptor).
    pub fn set_audio_stream(&mut self, stream_type: u8, registration: Option<[u8; 4]>) {
        self.audio_stream_type = stream_type;
        self.audio_registration = registration;
    }

    /// Advertise a raw SCTE-35 section stream (`stream_type 0x86`) in the PMT.
    pub fn set_scte35_stream(&mut self, pid: u16) {
        self.scte35_pid = pid;
        self.has_scte35 = true;
    }

    /// Packetize a complete SCTE-35 `splice_info_section` directly (no PES).
    pub fn mux_scte35(&mut self, section: &[u8]) -> Vec<Bytes> {
        if !self.has_scte35 || section.is_empty() {
            return Vec::new();
        }
        let mut packets = self.maybe_emit_pat_pmt(false);
        let mut payload = Vec::with_capacity(section.len() + 1);
        payload.push(0); // pointer_field
        payload.extend_from_slice(section);
        packets.extend(self.packetize(self.scte35_pid, &payload, true, false, None, false));
        packets
    }

    /// Emit PAT and PMT packets if needed.
    ///
    /// PAT/PMT are emitted:
    /// - On the very first frame (audio or video) to ensure players can
    ///   discover the program structure immediately.
    /// - Before every video keyframe (IDR) for fast channel-change.
    /// - Whenever more than [`PAT_PMT_INTERVAL_TICKS`] (≈ 360 ms) of media
    ///   time has elapsed since the previous emission. This time-based
    ///   gate replaces the old "every 40 calls" counter, which violated
    ///   TR-101290 Priority 1 (PAT/PMT every 500 ms) on slow-frame-rate
    ///   inputs — e.g. an RTSP camera sending video at 25 fps + AAC at
    ///   ~16 fps yields ~41 calls/s and ~975 ms PAT/PMT cadence with
    ///   the old counter (see Bug #3 in the 2026-04-09 test report).
    ///
    /// This is called from both `mux_video()` and `mux_audio()` so that
    /// audio-only streams still get valid program tables.
    fn maybe_emit_pat_pmt(&mut self, force: bool) -> Vec<Bytes> {
        self.pat_pmt_counter = self.pat_pmt_counter.saturating_add(1);
        let now = Instant::now();
        let elapsed_due = match self.last_pat_pmt_at {
            Some(last) => now.duration_since(last) >= PAT_PMT_INTERVAL,
            None => true,
        };
        // Belt-and-braces fallback: if `Instant::now()` is somehow frozen
        // (it shouldn't be), the counter still forces a re-emit eventually.
        let counter_due = self.pat_pmt_counter >= 40;
        if force || !self.pat_pmt_sent || elapsed_due || counter_due {
            self.pat_pmt_counter = 0;
            self.pat_pmt_sent = true;
            self.last_pat_pmt_at = Some(now);
            vec![Bytes::from(self.build_pat()), Bytes::from(self.build_pmt())]
        } else {
            Vec::new()
        }
    }

    /// Mux a video access unit (H.264 NALUs in Annex B format) into TS packets.
    ///
    /// `dts_90khz` and `pts_90khz` are in 90kHz clock units.
    /// `is_keyframe` indicates if this is an IDR frame (prepend PAT/PMT).
    ///
    /// PCR is written into the adaptation field of the first TS packet of
    /// **every** video frame (not just keyframes) so the PCR cadence
    /// satisfies TR-101290 Priority 2 (≤ 100 ms). At 25 fps the resulting
    /// cadence is ~40 ms; at 30 fps, ~33 ms. Cameras with long GOPs
    /// (e.g. 2 s default on Reolink) used to emit PCR only every 2 s,
    /// failing P2 — see Bug #3 in the 2026-04-09 test report.
    pub fn mux_video(
        &mut self,
        annex_b_data: &[u8],
        pts_90khz: u64,
        dts_90khz: u64,
        is_keyframe: bool,
    ) -> Vec<Bytes> {
        let mut packets = self.maybe_emit_pat_pmt(is_keyframe);

        // Build PES packet
        let pes = build_pes_packet(0xE0, annex_b_data, pts_90khz, Some(dts_90khz));

        // Split PES into TS packets. Always write PCR (not just on
        // keyframes) for TR-101290 P2 compliance.
        let video_pid = self.video_pid;
        let ts_pkts = self.packetize(video_pid, &pes, true, true, Some(dts_90khz), is_keyframe);
        packets.extend(ts_pkts);

        packets
    }

    /// Mux an AAC frame (raw, without ADTS header) into TS packets.
    ///
    /// Wraps the frame in an ADTS header before PES encapsulation.
    pub fn mux_audio(
        &mut self,
        raw_aac: &[u8],
        pts_90khz: u64,
        sample_rate_idx: u8,
        channels: u8,
    ) -> Vec<Bytes> {
        // Enforce monotonic audio PTS — see `last_audio_pts_90khz` doc
        // for the rationale (RTMP ms-rounded timestamps can dip backward).
        let pts_90khz = self.clamp_audio_pts(pts_90khz);
        let mut packets = self.maybe_emit_pat_pmt(false);

        // Wrap in ADTS header
        let adts_frame = build_adts_frame(raw_aac, sample_rate_idx, channels);
        let pes = build_pes_packet(0xC0, &adts_frame, pts_90khz, None);
        let audio_pid = self.audio_pid;
        packets.extend(self.packetize(audio_pid, &pes, true, false, None, false));

        packets
    }

    /// Mux a pre-ADTS-framed AAC frame into TS packets.
    ///
    /// Use this when the upstream demuxer has already wrapped the AAC
    /// frame in an ADTS header — most notably the `retina` RTSP client
    /// when configured with `FrameFormat::SIMPLE`, which sets
    /// `aac_framing: Adts` and hands us a complete ADTS frame per
    /// audio access unit. Calling [`mux_audio`] with that data would
    /// double-wrap the frame in another ADTS header, producing a
    /// stream that every downstream AAC decoder rejects with
    /// "channel element X.X is not allocated" — see Bug #3 in the
    /// 2026-04-09 test report.
    ///
    /// `adts_frame` must include the 7-byte ADTS header followed by the
    /// raw AAC payload. The header's sample rate / channel config bake
    /// in the codec parameters, so this entry point does not take a
    /// `sample_rate_idx` / `channels` argument.
    pub fn mux_audio_pre_adts(&mut self, adts_frame: &[u8], pts_90khz: u64) -> Vec<Bytes> {
        let pts_90khz = self.clamp_audio_pts(pts_90khz);
        let mut packets = self.maybe_emit_pat_pmt(false);
        let pes = build_pes_packet(0xC0, adts_frame, pts_90khz, None);
        let audio_pid = self.audio_pid;
        packets.extend(self.packetize(audio_pid, &pes, true, false, None, false));
        packets
    }

    /// Mux a raw Opus packet (single access unit, no framing) into TS
    /// packets using the FFmpeg-compatible Opus-in-MPEG-TS carriage.
    ///
    /// Wire format of each Opus AU inside the private PES:
    /// - Control header prefix (11 bits, all 1s) + 1 bit start_trim_flag +
    ///   1 bit end_trim_flag + 1 bit control_extension_flag + 2 reserved
    ///   bits. With all flags clear this packs into the two bytes
    ///   `0xFF 0xE0`.
    /// - `au_size` encoded byte-by-byte: each `0xFF` byte contributes 255
    ///   to the length and signals "continue"; one final non-0xFF byte
    ///   (0x00..=0xFE) supplies the remaining length. For a 150-byte
    ///   packet the encoding is a single `0x96`; for a 400-byte packet
    ///   it is `0xFF 0x91` (255 + 145).
    /// - The Opus packet bytes themselves.
    ///
    /// Callers must have previously configured the muxer via
    /// `set_audio_stream(0x06, Some(*b"Opus"))` so the PMT advertises
    /// stream_type 0x06 with the mandatory "Opus" registration
    /// descriptor. `stream_id = 0xBD` (private_stream_1) per the carriage
    /// spec.
    pub fn mux_audio_opus(&mut self, opus_packet: &[u8], pts_90khz: u64) -> Vec<Bytes> {
        let pts_90khz = self.clamp_audio_pts(pts_90khz);
        let mut packets = self.maybe_emit_pat_pmt(false);

        // Control header: 11 bits '1'*11 + start_trim(1)=0 + end_trim(1)=0 + ctrl_ext(1)=0 + reserved(2)=00
        // Bits:    1111_1111 1110_0000
        // Bytes:       0xFF    0xE0
        let mut au = Vec::with_capacity(4 + opus_packet.len());
        au.push(0xFF);
        au.push(0xE0);

        // au_size: chained 0xFF bytes + final non-0xFF remainder.
        let mut remaining = opus_packet.len();
        while remaining >= 255 {
            au.push(0xFF);
            remaining -= 255;
        }
        au.push(remaining as u8);
        au.extend_from_slice(opus_packet);

        // stream_id 0xBD (private_stream_1) is required for stream_type 0x06.
        let pes = build_pes_packet(0xBD, &au, pts_90khz, None);
        let audio_pid = self.audio_pid;
        packets.extend(self.packetize(audio_pid, &pes, true, false, None, false));
        packets
    }

    /// Split a PES payload into 188-byte TS packets.
    ///
    /// When `is_random_access_point` is true, the first emitted TS packet
    /// gets `random_access_indicator` set in its adaptation-field flags
    /// (bit 0x40) so downstream consumers — notably the replay writer's
    /// IDR index — can find seek points without parsing the elementary
    /// stream. Set this on video keyframes; audio/data must pass `false`.
    fn packetize(
        &mut self,
        pid: u16,
        pes_data: &[u8],
        payload_start: bool,
        write_pcr: bool,
        pcr_90khz: Option<u64>,
        is_random_access_point: bool,
    ) -> Vec<Bytes> {
        let mut packets = Vec::new();
        let mut offset = 0;
        let mut is_first = true;

        while offset < pes_data.len() {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];
            let mut pos = 0;

            let cc = if pid == self.video_pid {
                let c = self.cc_video;
                self.cc_video = (self.cc_video + 1) & 0x0F;
                c
            } else if pid == self.audio_pid {
                let c = self.cc_audio;
                self.cc_audio = (self.cc_audio + 1) & 0x0F;
                c
            } else if pid == self.scte35_pid {
                let c = self.cc_scte35;
                self.cc_scte35 = (self.cc_scte35 + 1) & 0x0F;
                c
            } else {
                0
            };

            let pusi = if is_first && payload_start { 1u8 } else { 0 };
            let need_pcr = is_first && write_pcr && pcr_90khz.is_some();

            // Sync byte
            pkt[pos] = TS_SYNC_BYTE;
            pos += 1;

            // Byte 1-2: TEI=0, PUSI, Priority=0, PID
            pkt[pos] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F);
            pos += 1;
            pkt[pos] = pid as u8;
            pos += 1;

            // Byte 3: placeholder (will fill AFC + CC after we know adaptation)
            let afc_pos = pos;
            pos += 1;

            if need_pcr {
                // Adaptation field with PCR
                let af_start = pos;
                pkt[pos] = 0; // adaptation_field_length (fill later)
                pos += 1;
                let mut af_flags = 0x10u8; // PCR present
                if is_first && is_random_access_point {
                    af_flags |= 0x40; // random_access_indicator
                }
                pkt[pos] = af_flags;
                pos += 1;
                // PCR (6 bytes)
                let pcr = pcr_90khz.unwrap();
                pkt[pos] = (pcr >> 25) as u8;
                pos += 1;
                pkt[pos] = (pcr >> 17) as u8;
                pos += 1;
                pkt[pos] = (pcr >> 9) as u8;
                pos += 1;
                pkt[pos] = (pcr >> 1) as u8;
                pos += 1;
                pkt[pos] = ((pcr & 1) << 7 | 0x7E) as u8;
                pos += 1;
                pkt[pos] = 0x00;
                pos += 1; // extension
                // Set adaptation_field_length = bytes after the length byte
                pkt[af_start] = (pos - af_start - 1) as u8;
                // AFC = 0b11 (adaptation + payload)
                pkt[afc_pos] = (0b11 << 4) | cc;
            } else {
                // No adaptation field
                pkt[afc_pos] = (0b01 << 4) | cc;
            }

            // Fill payload
            let available = TS_PACKET_SIZE - pos;
            let remaining = pes_data.len() - offset;
            let payload_len = remaining.min(available);

            // If this is the last chunk and payload doesn't fill the packet,
            // we need an adaptation field for stuffing
            if payload_len < available && !need_pcr {
                let stuff_needed = available - payload_len;
                if stuff_needed > 0 {
                    // Rewrite: need adaptation field for stuffing
                    // Reset pos to after the 4-byte header
                    pos = 4;
                    pkt[afc_pos] = (0b11 << 4) | cc; // AFC = adaptation + payload

                    if stuff_needed == 1 {
                        // Just the adaptation_field_length byte = 0
                        pkt[pos] = 0;
                        pos += 1;
                    } else {
                        // adaptation_field_length + flags + stuffing
                        pkt[pos] = (stuff_needed - 1) as u8;
                        pos += 1;
                        pkt[pos] = 0x00;
                        pos += 1; // flags = none
                        for _ in 0..stuff_needed.saturating_sub(2) {
                            pkt[pos] = 0xFF;
                            pos += 1;
                        }
                    }
                }
            }

            pkt[pos..pos + payload_len].copy_from_slice(&pes_data[offset..offset + payload_len]);

            packets.push(Bytes::copy_from_slice(&pkt));
            offset += payload_len;
            is_first = false;
        }

        packets
    }

    /// Build a PAT packet.
    fn build_pat(&mut self) -> Vec<u8> {
        let cc = self.cc_pat;
        self.cc_pat = (self.cc_pat + 1) & 0x0F;

        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0x0000 high
        pkt[2] = 0x00; // PID low
        pkt[3] = 0x10 | cc; // AFC=01 (payload only), CC

        // Pointer field
        pkt[4] = 0x00;

        // PAT table
        let pat_start = 5;
        pkt[pat_start] = 0x00; // table_id = 0 (PAT)
        // section_syntax_indicator=1, reserved, section_length
        let section_length: u16 = 13; // 5 fixed + 4 program + 4 CRC
        pkt[pat_start + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[pat_start + 2] = section_length as u8;
        pkt[pat_start + 3] = 0x00; // transport_stream_id high
        pkt[pat_start + 4] = 0x01; // transport_stream_id low
        pkt[pat_start + 5] = 0xC1; // reserved, version=0, current_next=1
        pkt[pat_start + 6] = 0x00; // section_number
        pkt[pat_start + 7] = 0x00; // last_section_number

        // Program 1 -> PMT PID (instance field, default 0x1000)
        pkt[pat_start + 8] = 0x00; // program_number high
        pkt[pat_start + 9] = 0x01; // program_number low
        pkt[pat_start + 10] = 0xE0 | ((self.pmt_pid >> 8) as u8 & 0x1F); // reserved + PID high
        pkt[pat_start + 11] = self.pmt_pid as u8; // PID low

        // CRC32
        let crc = crc32_mpeg2(&pkt[pat_start..pat_start + 12]);
        pkt[pat_start + 12..pat_start + 16].copy_from_slice(&crc.to_be_bytes());

        // Fill rest with 0xFF
        for b in &mut pkt[pat_start + 16..] {
            *b = 0xFF;
        }

        pkt
    }

    /// Build a PMT packet.
    fn build_pmt(&mut self) -> Vec<u8> {
        let cc = self.cc_pmt;
        self.cc_pmt = (self.cc_pmt + 1) & 0x0F;

        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((self.pmt_pid >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = self.pmt_pid as u8;
        pkt[3] = 0x10 | cc;

        // Pointer field
        pkt[4] = 0x00;

        let pmt_start = 5;
        pkt[pmt_start] = 0x02; // table_id = 2 (PMT)

        // Per-ES descriptor sizes (registration descriptor is 6 bytes:
        // tag(1) + len(1) + format_identifier(4)).
        let audio_desc_len = if self.audio_registration.is_some() {
            6
        } else {
            0
        };
        // 5 bytes per stream entry header + per-stream descriptor bytes.
        let streams_len = (if self.has_video { 5 } else { 0 })
            + (if self.has_audio {
                5 + audio_desc_len
            } else {
                0
            })
            + (if self.has_scte35 { 5 } else { 0 });
        // section_length covers everything from program_number through CRC.
        let section_length = 9 + streams_len as u16 + 4;

        pkt[pmt_start + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[pmt_start + 2] = section_length as u8;
        pkt[pmt_start + 3] = 0x00; // program_number high
        pkt[pmt_start + 4] = 0x01; // program_number low
        // reserved(2)='11' | version_number(5) | current_next_indicator(1)='1'
        // → 0xC0 | (version << 1) | 0x01
        pkt[pmt_start + 5] = 0xC1 | ((self.pmt_version & 0x1F) << 1);
        pkt[pmt_start + 6] = 0x00; // section_number
        pkt[pmt_start + 7] = 0x00; // last_section_number
        // PCR PID: explicit override wins; otherwise default to video if
        // present, else audio. The override must point at an ES PID that
        // actually carries PCR-bearing TS packets — the muxer doesn't
        // emit PCR on standalone PIDs.
        let pcr_pid = self.pcr_pid_override.unwrap_or(if self.has_video {
            self.video_pid
        } else {
            self.audio_pid
        });
        pkt[pmt_start + 8] = 0xE0 | ((pcr_pid >> 8) as u8 & 0x1F);
        pkt[pmt_start + 9] = pcr_pid as u8;
        pkt[pmt_start + 10] = 0xF0; // reserved + program_info_length high
        pkt[pmt_start + 11] = 0x00; // program_info_length low = 0

        let mut pos = pmt_start + 12;

        // Video stream entry (H.264 or H.265)
        if self.has_video {
            pkt[pos] = self.video_stream_type;
            pkt[pos + 1] = 0xE0 | ((self.video_pid >> 8) as u8 & 0x1F);
            pkt[pos + 2] = self.video_pid as u8;
            pkt[pos + 3] = 0xF0; // reserved + ES_info_length high
            pkt[pos + 4] = 0x00; // ES_info_length low = 0
            pos += 5;
        }

        // Audio stream entry. The stream type is configurable (default AAC,
        // 0x06 for SMPTE 302M private PES). When `audio_registration` is set,
        // a 6-byte registration descriptor is written into ES_info.
        if self.has_audio {
            pkt[pos] = self.audio_stream_type;
            pkt[pos + 1] = 0xE0 | ((self.audio_pid >> 8) as u8 & 0x1F);
            pkt[pos + 2] = self.audio_pid as u8;
            pkt[pos + 3] = 0xF0 | ((audio_desc_len >> 8) as u8 & 0x0F);
            pkt[pos + 4] = audio_desc_len as u8;
            pos += 5;
            if let Some(reg) = self.audio_registration {
                pkt[pos] = 0x05; // descriptor_tag = registration_descriptor
                pkt[pos + 1] = 0x04; // descriptor_length
                pkt[pos + 2] = reg[0];
                pkt[pos + 3] = reg[1];
                pkt[pos + 4] = reg[2];
                pkt[pos + 5] = reg[3];
                pos += 6;
            }
        }

        if self.has_scte35 {
            pkt[pos] = 0x86;
            pkt[pos + 1] = 0xE0 | ((self.scte35_pid >> 8) as u8 & 0x1F);
            pkt[pos + 2] = self.scte35_pid as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0;
            pos += 5;
        }

        // CRC32
        let crc = crc32_mpeg2(&pkt[pmt_start..pos]);
        pkt[pos..pos + 4].copy_from_slice(&crc.to_be_bytes());
        pos += 4;

        // Fill rest with 0xFF
        for b in &mut pkt[pos..] {
            *b = 0xFF;
        }

        pkt
    }

    /// Mux a private-PES audio payload (e.g., SMPTE 302M LPCM) into TS
    /// packets on the audio PID. Uses PES `stream_id = 0xBD`
    /// (private_stream_1). The caller supplies a complete PES payload — for
    /// 302M, that's a `S302mPacketizer::packetize_f32(...)` result.
    ///
    /// `pts_90khz` is the presentation timestamp in 90 kHz ticks. PCR is
    /// emitted on the audio PID (since this path is normally audio-only —
    /// `has_video` should be `false`).
    pub fn mux_private_audio(&mut self, pes_payload: &[u8], pts_90khz: u64) -> Vec<Bytes> {
        let mut packets = self.maybe_emit_pat_pmt(false);
        let pes = build_pes_packet(0xBD, pes_payload, pts_90khz, None);
        // Audio-only TS: PCR rides the audio PID.
        let audio_pid = self.audio_pid;
        packets.extend(self.packetize(
            audio_pid,
            &pes,
            true,
            !self.has_video,
            Some(pts_90khz),
            false,
        ));
        packets
    }
}

/// Build a PES packet with PTS (and optional DTS) headers.
fn build_pes_packet(
    stream_id: u8,
    payload: &[u8],
    pts_90khz: u64,
    dts_90khz: Option<u64>,
) -> Vec<u8> {
    let has_dts = dts_90khz.is_some() && dts_90khz != Some(pts_90khz);
    let header_data_len = if has_dts { 10 } else { 5 }; // PTS=5, PTS+DTS=10
    let pes_header_len = 3 + header_data_len; // flags(2) + header_data_length(1) + timestamp bytes

    let mut buf = BytesMut::with_capacity(6 + pes_header_len + payload.len());

    // PES start code: 0x000001
    buf.put_u8(0x00);
    buf.put_u8(0x00);
    buf.put_u8(0x01);
    buf.put_u8(stream_id);

    // PES packet length (0 = unbounded for video, but we set it for small packets)
    let pes_packet_len = if payload.len() + pes_header_len > 65535 {
        0u16 // unbounded
    } else {
        (pes_header_len + payload.len()) as u16
    };
    buf.put_u16(pes_packet_len);

    // Flags byte 1: 10xxxxxx (MPEG-2)
    buf.put_u8(0x80);

    // Flags byte 2: PTS/DTS flags
    if has_dts {
        buf.put_u8(0xC0); // PTS + DTS present
    } else {
        buf.put_u8(0x80); // PTS only
    }

    // PES header data length
    buf.put_u8(header_data_len as u8);

    // PTS
    if has_dts {
        write_timestamp(&mut buf, 0x03, pts_90khz); // 0011 xxxx
    } else {
        write_timestamp(&mut buf, 0x02, pts_90khz); // 0010 xxxx
    }

    // DTS
    if has_dts {
        write_timestamp(&mut buf, 0x01, dts_90khz.unwrap()); // 0001 xxxx
    }

    buf.put_slice(payload);
    buf.to_vec()
}

/// Write a 33-bit PTS/DTS timestamp in the 5-byte PES format.
fn write_timestamp(buf: &mut BytesMut, marker_bits: u8, ts: u64) {
    let ts = ts & 0x1_FFFF_FFFF; // 33 bits
    buf.put_u8((marker_bits << 4) | (((ts >> 29) & 0x0E) as u8) | 0x01);
    buf.put_u8(((ts >> 22) & 0xFF) as u8);
    buf.put_u8((((ts >> 14) & 0xFE) as u8) | 0x01);
    buf.put_u8(((ts >> 7) & 0xFF) as u8);
    buf.put_u8((((ts << 1) & 0xFE) as u8) | 0x01);
}

/// Build an ADTS header + raw AAC frame.
fn build_adts_frame(raw_aac: &[u8], freq_idx: u8, channels: u8) -> Vec<u8> {
    let frame_len = raw_aac.len() + 7; // ADTS header is 7 bytes (no CRC)
    let mut buf = BytesMut::with_capacity(frame_len);

    // ADTS fixed header
    buf.put_u8(0xFF); // syncword high
    buf.put_u8(0xF1); // syncword low + ID=0(MPEG-4) + layer=00 + protection_absent=1
    // Profile (AAC-LC=1, stored as profile-1=1) + frequency index + private=0 + channel config high
    let profile = 1u8; // AAC-LC (stored as profile-1 in ADTS)
    buf.put_u8((profile << 6) | (freq_idx << 2) | ((channels >> 2) & 0x01));
    // Channel config low + original/copy=0 + home=0 + copyright=0 + copyright_start=0 + frame_length high
    buf.put_u8(((channels & 0x03) << 6) | ((frame_len >> 11) as u8 & 0x03));
    // Frame length mid
    buf.put_u8((frame_len >> 3) as u8);
    // Frame length low + buffer fullness high
    buf.put_u8(((frame_len & 0x07) as u8) << 5 | 0x1F);
    // Buffer fullness low + number of AAC frames - 1
    buf.put_u8(0xFC); // buffer fullness = 0x7FF (VBR) | 0 frames - 1

    buf.put_slice(raw_aac);
    buf.to_vec()
}

/// CRC-32/MPEG-2 (used in PAT/PMT).
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if crc & 0x8000_0000 != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests still reference the short PID names; alias them to the public
    // defaults so the original test bodies need no churn.
    #[allow(dead_code)]
    const PMT_PID: u16 = DEFAULT_PMT_PID;
    const VIDEO_PID: u16 = DEFAULT_VIDEO_PID;
    const AUDIO_PID: u16 = DEFAULT_AUDIO_PID;

    #[test]
    fn test_pat_packet_size() {
        let mut muxer = TsMuxer::new();
        let pat = muxer.build_pat();
        assert_eq!(pat.len(), TS_PACKET_SIZE);
        assert_eq!(pat[0], TS_SYNC_BYTE);
    }

    #[test]
    fn test_pmt_packet_size() {
        let mut muxer = TsMuxer::new();
        let pmt = muxer.build_pmt();
        assert_eq!(pmt.len(), TS_PACKET_SIZE);
        assert_eq!(pmt[0], TS_SYNC_BYTE);
    }

    #[test]
    fn test_pes_with_pts() {
        let pes = build_pes_packet(0xE0, &[0x00, 0x00, 0x00, 0x01, 0x65], 90000, None);
        // Check PES start code
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xE0); // video stream ID
    }

    #[test]
    fn test_adts_frame() {
        let raw = vec![0xDE, 0xAD]; // dummy AAC data
        let adts = build_adts_frame(&raw, 4, 2); // 44.1kHz, stereo
        assert_eq!(adts.len(), 9); // 7 header + 2 data
        assert_eq!(adts[0], 0xFF);
        assert_eq!(adts[1] & 0xF0, 0xF0);
    }

    #[test]
    fn test_pmt_with_smpte_302m_registration_descriptor() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(false);
        muxer.set_has_audio(true);
        muxer.set_audio_stream(0x06, Some(*b"BSSD"));
        let pmt = muxer.build_pmt();
        assert_eq!(pmt.len(), TS_PACKET_SIZE);
        assert_eq!(pmt[0], TS_SYNC_BYTE);
        // PMT section starts at byte 5 (sync + 3 + pointer field)
        // table_id at pmt_start = byte 5
        assert_eq!(pmt[5], 0x02);
        // Walk to the audio ES entry. With no video, the entry sits right
        // after the 12-byte fixed PMT prologue at pmt_start + 12 = 17.
        let es_start = 5 + 12;
        assert_eq!(
            pmt[es_start], 0x06,
            "stream_type should be 0x06 (private PES)"
        );
        let es_info_len = ((pmt[es_start + 3] & 0x0F) as usize) << 8 | pmt[es_start + 4] as usize;
        assert_eq!(
            es_info_len, 6,
            "ES_info_length should be 6 (registration descriptor)"
        );
        // Registration descriptor: tag 0x05, length 0x04, "BSSD"
        assert_eq!(pmt[es_start + 5], 0x05);
        assert_eq!(pmt[es_start + 6], 0x04);
        assert_eq!(&pmt[es_start + 7..es_start + 11], b"BSSD");
    }

    #[test]
    fn test_pmt_default_audio_aac_no_registration() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(false);
        muxer.set_has_audio(true);
        // Don't override audio_stream — should default to AAC, no registration descriptor.
        let pmt = muxer.build_pmt();
        let es_start = 5 + 12;
        assert_eq!(pmt[es_start], 0x0F);
        let es_info_len = ((pmt[es_start + 3] & 0x0F) as usize) << 8 | pmt[es_start + 4] as usize;
        assert_eq!(es_info_len, 0);
    }

    /// Bug #11 (2026-04-09): RTMP-ingest → RTP transmux emitted
    /// non-monotonic audio DTS because the source publisher fed
    /// millisecond-rounded timestamps that occasionally regressed by
    /// 1 tick when AAC's 21.333 ms frame interval crossed a ms boundary.
    /// This test pumps a deliberately non-monotonic sequence through
    /// `clamp_audio_pts` and asserts every clamped value is strictly
    /// greater than the previous one.
    #[test]
    fn clamp_audio_pts_is_strictly_monotonic_under_regression() {
        let mut muxer = TsMuxer::new();
        // Realistic regressing sequence: AAC at 48 kHz wants 21.333 ms
        // spacing → 1920 ticks @ 90 kHz. RTMP-ms-rounded sources emit
        // values like 0, 1890, 3870, 5760, 7650, ... which are monotonic.
        // But ffmpeg-as-publisher occasionally backs up by 1 ms = 90 ticks.
        // Inject: monotonic chunk, 1-tick equal, 14-tick regression
        // (matches the bug report's "dts 154629 >= 154615" example),
        // and a 1004-µs ≈ 90-tick regression.
        let raw: Vec<u64> = vec![
            0, 1890, 3870, 5760, 7650, 7650, // exact equal
            9540, 9526, // 14-tick regression
            11430, 11340, // 90-tick (≈1 ms) regression
            13320,
        ];
        let mut clamped: Vec<u64> = Vec::new();
        for ts in &raw {
            clamped.push(muxer.clamp_audio_pts(*ts));
        }
        for w in clamped.windows(2) {
            assert!(
                w[1] > w[0],
                "clamped sequence must be strictly monotonic: prev={} cur={} (raw={raw:?} clamped={clamped:?})",
                w[0],
                w[1]
            );
        }
        // First value passes through unchanged.
        assert_eq!(clamped[0], 0);
        // Forward-going values pass through unchanged.
        assert_eq!(clamped[1], 1890);
        assert_eq!(clamped[2], 3870);
        // Equal value gets bumped by exactly 1 tick.
        assert_eq!(clamped[5], 7651);
    }

    /// Verify the same property end-to-end through `mux_audio` by
    /// extracting the PTS field from each emitted PES. This catches the
    /// case where someone bypasses `clamp_audio_pts` and writes the raw
    /// timestamp directly into the PES header.
    #[test]
    fn mux_audio_emits_monotonic_pes_pts_under_regression() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(false);
        muxer.set_has_audio(true);
        let raw_aac = vec![0u8; 32];
        // Raw timestamps with intentional regressions.
        let raw_ts: Vec<u64> = vec![0, 1890, 3870, 3870, 5760, 5746, 7650];
        let mut emitted_pts: Vec<u64> = Vec::new();
        for ts in &raw_ts {
            let packets = muxer.mux_audio(&raw_aac, *ts, 3, 2);
            // Find the audio PES packet (PID 0x0101) with PUSI=1 and
            // extract the 33-bit PTS field per ISO 13818-1 §2.4.3.6.
            for pkt in &packets {
                if pkt.len() != TS_PACKET_SIZE || pkt[0] != TS_SYNC_BYTE {
                    continue;
                }
                let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
                let pusi = (pkt[1] & 0x40) != 0;
                if pid != AUDIO_PID || !pusi {
                    continue;
                }
                let afc = (pkt[3] >> 4) & 0x03;
                // Payload starts at byte 4 (no AF) or 4 + 1 + adaptation_field_length (with AF).
                let payload_start = match afc {
                    0b01 => 4,
                    0b11 => 5 + pkt[4] as usize,
                    _ => continue,
                };
                // PES header: start_code_prefix(3) + stream_id(1) + pkt_len(2) + flags(2) + hdr_data_len(1) = 9 bytes
                // PTS bytes start at payload_start + 9.
                let pts_off = payload_start + 9;
                if pts_off + 5 > pkt.len() {
                    continue;
                }
                let b0 = pkt[pts_off] as u64;
                let b1 = pkt[pts_off + 1] as u64;
                let b2 = pkt[pts_off + 2] as u64;
                let b3 = pkt[pts_off + 3] as u64;
                let b4 = pkt[pts_off + 4] as u64;
                let pts = (((b0 >> 1) & 0x07) << 30)
                    | (b1 << 22)
                    | (((b2 >> 1) & 0x7F) << 15)
                    | (b3 << 7)
                    | ((b4 >> 1) & 0x7F);
                emitted_pts.push(pts);
                break;
            }
        }
        assert_eq!(
            emitted_pts.len(),
            raw_ts.len(),
            "every audio frame must produce a PES with PTS"
        );
        for w in emitted_pts.windows(2) {
            assert!(
                w[1] > w[0],
                "PES PTS sequence must be strictly monotonic: prev={} cur={} (raw={raw_ts:?} emitted={emitted_pts:?})",
                w[0],
                w[1]
            );
        }
    }

    /// `seed_cc` must make the next emitted packet on each PID continue
    /// from `(seed + 1) & 0x0F`, including across the 4-bit wrap.
    #[test]
    fn seed_cc_continues_from_supplied_value_with_wrap() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(true);
        muxer.set_has_audio(false);
        // Seed video CC = 15 → next packet should carry CC = 0.
        muxer.seed_cc(0xFF, 0xFF, 15, 0xFF);
        let pes = build_pes_packet(
            0xE0,
            &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88],
            90_000,
            Some(90_000),
        );
        let pkts = muxer.packetize(VIDEO_PID, &pes, true, true, Some(90_000), true);
        assert!(!pkts.is_empty());
        let first = &pkts[0];
        assert_eq!(first.len(), TS_PACKET_SIZE);
        // CC is the low nibble of byte 3.
        assert_eq!(first[3] & 0x0F, 0, "wrap from 15 → 0");

        // Subsequent emit on the same PID should advance to 1.
        let pes2 = build_pes_packet(0xE0, &[0x00, 0x00, 0x00, 0x01, 0x41], 91_000, Some(91_000));
        let pkts2 = muxer.packetize(VIDEO_PID, &pes2, true, true, Some(91_000), false);
        assert_eq!(pkts2[0][3] & 0x0F, 1);
    }

    /// `current_cc` must round-trip with `seed_cc` so a continuity-aware
    /// caller can checkpoint state across muxer recreation.
    #[test]
    fn current_cc_round_trips_through_seed_cc() {
        let mut a = TsMuxer::new();
        a.set_has_video(true);
        a.set_has_audio(true);
        // Run a few packets through `a` so its CC counters are non-zero.
        let pes = build_pes_packet(0xE0, &[0u8; 16], 0, Some(0));
        let _ = a.packetize(VIDEO_PID, &pes, true, true, Some(0), true);
        let _ = a.packetize(AUDIO_PID, &pes, true, false, None, false);
        let _ = a.packetize(VIDEO_PID, &pes, true, true, Some(3_000), false);
        let (pat, pmt, video, audio) = a.current_cc();

        // Now spin up a fresh muxer, seed it, and verify the next packet
        // on each PID continues the sequence.
        let mut b = TsMuxer::new();
        b.set_has_video(true);
        b.set_has_audio(true);
        b.seed_cc(pat, pmt, video, audio);
        let next_video = b.packetize(VIDEO_PID, &pes, true, true, Some(6_000), false);
        let next_audio = b.packetize(AUDIO_PID, &pes, true, false, None, false);
        assert_eq!(next_video[0][3] & 0x0F, (video + 1) & 0x0F);
        assert_eq!(next_audio[0][3] & 0x0F, (audio + 1) & 0x0F);
    }

    /// `set_pmt_version` must be reflected in the PMT byte that carries
    /// the `version_number` field. Bit layout per ISO/IEC 13818-1 §2.4.4.9:
    /// reserved('11') | version_number(5) | current_next_indicator('1').
    #[test]
    fn set_pmt_version_is_written_into_pmt_section() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(true);
        muxer.set_pmt_version(7);
        let pmt = muxer.build_pmt();
        // PMT section starts at byte 5 (sync + 3 + pointer field). The
        // version_number byte sits 5 bytes later (table_id, section
        // header (2), program_number (2)).
        let v_byte = pmt[5 + 5];
        assert_eq!(v_byte & 0xC1, 0xC1, "reserved + current_next bits intact");
        let recovered = (v_byte >> 1) & 0x1F;
        assert_eq!(recovered, 7);
    }

    /// Version bumps wrap mod 32 (the field is 5 bits).
    #[test]
    fn set_pmt_version_wraps_at_5_bits() {
        let mut muxer = TsMuxer::new();
        muxer.set_pmt_version(0xFF); // out-of-range — should mask to 0x1F
        let pmt = muxer.build_pmt();
        let recovered = (pmt[5 + 5] >> 1) & 0x1F;
        assert_eq!(recovered, 31);
    }

    #[test]
    fn test_mux_private_audio_emits_at_least_one_ts_packet() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(false);
        muxer.set_has_audio(true);
        muxer.set_audio_stream(0x06, Some(*b"BSSD"));
        // 64 bytes of dummy 302M-style PES payload.
        let payload = vec![0u8; 64];
        let ts = muxer.mux_private_audio(&payload, 90_000);
        assert!(!ts.is_empty());
        // First two should be PAT and PMT, then one or more 188-byte TS packets.
        for pkt in &ts {
            assert_eq!(pkt.len(), TS_PACKET_SIZE);
            assert_eq!(pkt[0], TS_SYNC_BYTE);
        }
    }

    /// `set_pids` must propagate through PAT (PMT-PID pointer), the PMT
    /// header (its own PID), and PMT entries (video / audio / PCR PIDs).
    /// This is the contract every transcoded input/output relies on for
    /// switcher use — a downstream decoder pinned to specific PIDs must
    /// see them advertised consistently across muxer instances.
    #[test]
    fn set_pids_overrides_propagate_through_pat_pmt() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(true);
        muxer.set_has_audio(true);
        muxer.set_pids(Some(0x1234), Some(0x0250), Some(0x0260), Some(0x0260));

        // PAT: program 1 → PMT PID 0x1234.
        let pat = muxer.build_pat();
        let pmt_pid_pat = (((pat[5 + 10] & 0x1F) as u16) << 8) | pat[5 + 11] as u16;
        assert_eq!(
            pmt_pid_pat, 0x1234,
            "PAT must point at the override PMT PID"
        );

        // PMT TS header: PID byte 1-2.
        let pmt = muxer.build_pmt();
        let pmt_pid_hdr = (((pmt[1] & 0x1F) as u16) << 8) | pmt[2] as u16;
        assert_eq!(
            pmt_pid_hdr, 0x1234,
            "PMT TS header must use the override PID"
        );

        // PMT body: PCR PID at section offset +8/+9.
        let pcr_pid = (((pmt[5 + 8] & 0x1F) as u16) << 8) | pmt[5 + 9] as u16;
        assert_eq!(pcr_pid, 0x0260, "PCR PID override must appear in PMT");

        // First ES entry (video, since has_video) sits at pmt_start + 12.
        let v_es = (((pmt[5 + 12 + 1] & 0x1F) as u16) << 8) | pmt[5 + 12 + 2] as u16;
        assert_eq!(v_es, 0x0250, "video ES PID override must appear in PMT");

        // Second ES entry (audio) sits 5 bytes after the video entry
        // (no descriptors on default AAC).
        let a_es = (((pmt[5 + 12 + 5 + 1] & 0x1F) as u16) << 8) | pmt[5 + 12 + 5 + 2] as u16;
        assert_eq!(a_es, 0x0260, "audio ES PID override must appear in PMT");
    }

    /// PCR PID auto-falls-back to the video PID when no explicit override
    /// is supplied (preserves legacy behaviour with a custom video PID).
    #[test]
    fn pcr_pid_defaults_to_video_pid_when_unset() {
        let mut muxer = TsMuxer::new();
        muxer.set_has_video(true);
        muxer.set_has_audio(true);
        muxer.set_pids(None, Some(0x0312), Some(0x0413), None);
        let pmt = muxer.build_pmt();
        let pcr_pid = (((pmt[5 + 8] & 0x1F) as u16) << 8) | pmt[5 + 9] as u16;
        assert_eq!(pcr_pid, 0x0312, "PCR must default to the video PID");
    }

    #[test]
    fn mux_scte35_announces_pid_and_increments_continuity() {
        let mut muxer = TsMuxer::new();
        muxer.set_scte35_stream(0x01fc);
        let pmt = muxer.build_pmt();
        assert!(pmt.windows(3).any(|w| w == [0x86, 0xe1, 0xfc]));

        // Mark PSI as already sent so each call returns just its cue packet.
        muxer.pat_pmt_sent = true;
        muxer.last_pat_pmt_at = Some(Instant::now());
        let a = muxer.mux_scte35(&[0xfc, 0x00, 0x00]);
        let b = muxer.mux_scte35(&[0xfc, 0x00, 0x00]);
        assert_eq!((((a[0][1] & 0x1f) as u16) << 8) | a[0][2] as u16, 0x01fc);
        assert_eq!(a[0][3] & 0x0f, 0);
        assert_eq!(b[0][3] & 0x0f, 1);
        let payload = |packet: &Bytes| {
            if packet[3] & 0x20 != 0 {
                5 + packet[4] as usize
            } else {
                4
            }
        };
        let pos = payload(&a[0]);
        assert_eq!(a[0][pos], 0); // pointer_field
        assert_eq!(a[0][pos + 1], 0xfc);
    }
}
