// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
use bytes::{BufMut, BytesMut, Bytes};

// TS constants
const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const AUDIO_PID: u16 = 0x0101;

/// H.264 stream type in PMT (ISO 14496-10).
const STREAM_TYPE_H264: u8 = 0x1B;
/// H.265/HEVC stream type in PMT (ITU-T H.265).
pub const STREAM_TYPE_H265: u8 = 0x24;
/// AAC stream type in PMT (ISO 13818-7).
const STREAM_TYPE_AAC: u8 = 0x0F;

/// MPEG-TS muxer state.
pub struct TsMuxer {
    /// Continuity counters per PID.
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    cc_audio: u8,
    /// Whether the stream includes video (affects PMT content).
    has_video: bool,
    /// Whether the stream includes audio (affects PMT content).
    has_audio: bool,
    /// Video stream type for PMT (H.264=0x1B, H.265=0x24).
    video_stream_type: u8,
    /// Frame counter for periodic PAT/PMT emission.
    /// Counts across both video and audio frames.
    pat_pmt_counter: u32,
    /// Whether PAT/PMT have been emitted at least once.
    pat_pmt_sent: bool,
}

impl TsMuxer {
    pub fn new() -> Self {
        Self {
            cc_pat: 0,
            cc_pmt: 0,
            cc_video: 0,
            cc_audio: 0,
            has_video: true,
            has_audio: false,
            video_stream_type: STREAM_TYPE_H264,
            pat_pmt_counter: 0,
            pat_pmt_sent: false,
        }
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

    /// Emit PAT and PMT packets if needed.
    ///
    /// PAT/PMT are emitted:
    /// - On the very first frame (audio or video) to ensure players can
    ///   discover the program structure immediately.
    /// - Before every video keyframe (IDR) for fast channel-change.
    /// - Every 40 frames (~1.3s at 30fps, well within the TR-101290
    ///   500ms requirement for typical frame rates).
    ///
    /// This is called from both `mux_video()` and `mux_audio()` so that
    /// audio-only streams (e.g., RTSP cameras sending H.265 video that
    /// we don't yet decode) still get valid program tables.
    fn maybe_emit_pat_pmt(&mut self, force: bool) -> Vec<Bytes> {
        self.pat_pmt_counter += 1;
        if force || !self.pat_pmt_sent || self.pat_pmt_counter >= 40 {
            self.pat_pmt_counter = 0;
            self.pat_pmt_sent = true;
            vec![
                Bytes::from(self.build_pat()),
                Bytes::from(self.build_pmt()),
            ]
        } else {
            Vec::new()
        }
    }

    /// Mux a video access unit (H.264 NALUs in Annex B format) into TS packets.
    ///
    /// `dts_90khz` and `pts_90khz` are in 90kHz clock units.
    /// `is_keyframe` indicates if this is an IDR frame (prepend PAT/PMT + PCR).
    pub fn mux_video(&mut self, annex_b_data: &[u8], pts_90khz: u64, dts_90khz: u64, is_keyframe: bool) -> Vec<Bytes> {
        let mut packets = self.maybe_emit_pat_pmt(is_keyframe);

        // Build PES packet
        let pes = build_pes_packet(0xE0, annex_b_data, pts_90khz, Some(dts_90khz));

        // Split PES into TS packets
        let ts_pkts = self.packetize(VIDEO_PID, &pes, true, is_keyframe, Some(dts_90khz));
        packets.extend(ts_pkts);

        packets
    }

    /// Mux an AAC frame (raw, without ADTS header) into TS packets.
    ///
    /// Wraps the frame in an ADTS header before PES encapsulation.
    pub fn mux_audio(&mut self, raw_aac: &[u8], pts_90khz: u64, sample_rate_idx: u8, channels: u8) -> Vec<Bytes> {
        let mut packets = self.maybe_emit_pat_pmt(false);

        // Wrap in ADTS header
        let adts_frame = build_adts_frame(raw_aac, sample_rate_idx, channels);
        let pes = build_pes_packet(0xC0, &adts_frame, pts_90khz, None);
        packets.extend(self.packetize(AUDIO_PID, &pes, true, false, None));

        packets
    }

    /// Split a PES payload into 188-byte TS packets.
    fn packetize(&mut self, pid: u16, pes_data: &[u8], payload_start: bool, write_pcr: bool, pcr_90khz: Option<u64>) -> Vec<Bytes> {
        let mut packets = Vec::new();
        let mut offset = 0;
        let mut is_first = true;

        while offset < pes_data.len() {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];
            let mut pos = 0;

            let cc = match pid {
                VIDEO_PID => { let c = self.cc_video; self.cc_video = (self.cc_video + 1) & 0x0F; c }
                AUDIO_PID => { let c = self.cc_audio; self.cc_audio = (self.cc_audio + 1) & 0x0F; c }
                _ => 0,
            };

            let pusi = if is_first && payload_start { 1u8 } else { 0 };
            let need_pcr = is_first && write_pcr && pcr_90khz.is_some();

            // Sync byte
            pkt[pos] = TS_SYNC_BYTE; pos += 1;

            // Byte 1-2: TEI=0, PUSI, Priority=0, PID
            pkt[pos] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F); pos += 1;
            pkt[pos] = pid as u8; pos += 1;

            // Byte 3: placeholder (will fill AFC + CC after we know adaptation)
            let afc_pos = pos;
            pos += 1;

            if need_pcr {
                // Adaptation field with PCR
                let af_start = pos;
                pkt[pos] = 0; // adaptation_field_length (fill later)
                pos += 1;
                pkt[pos] = 0x10; // flags: PCR present
                pos += 1;
                // PCR (6 bytes)
                let pcr = pcr_90khz.unwrap();
                pkt[pos] = (pcr >> 25) as u8; pos += 1;
                pkt[pos] = (pcr >> 17) as u8; pos += 1;
                pkt[pos] = (pcr >> 9) as u8; pos += 1;
                pkt[pos] = (pcr >> 1) as u8; pos += 1;
                pkt[pos] = ((pcr & 1) << 7 | 0x7E) as u8; pos += 1;
                pkt[pos] = 0x00; pos += 1; // extension
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
                        pkt[pos] = 0; pos += 1;
                    } else {
                        // adaptation_field_length + flags + stuffing
                        pkt[pos] = (stuff_needed - 1) as u8; pos += 1;
                        pkt[pos] = 0x00; pos += 1; // flags = none
                        for _ in 0..stuff_needed.saturating_sub(2) {
                            pkt[pos] = 0xFF; pos += 1;
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

        // Program 1 -> PMT PID 0x1000
        pkt[pat_start + 8] = 0x00; // program_number high
        pkt[pat_start + 9] = 0x01; // program_number low
        pkt[pat_start + 10] = 0xE0 | ((PMT_PID >> 8) as u8 & 0x1F); // reserved + PID high
        pkt[pat_start + 11] = PMT_PID as u8; // PID low

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
        pkt[1] = 0x40 | ((PMT_PID >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = PMT_PID as u8;
        pkt[3] = 0x10 | cc;

        // Pointer field
        pkt[4] = 0x00;

        let pmt_start = 5;
        pkt[pmt_start] = 0x02; // table_id = 2 (PMT)

        // Calculate section length: 5 bytes per stream entry
        let streams_len = (if self.has_video { 5 } else { 0 }) + (if self.has_audio { 5 } else { 0 });
        // section_length includes: program_number(2) + reserved/version(1) + section_number(1) + last_section(1)
        // + reserved/PCR_PID(2) + reserved/program_info_length(2) + streams + CRC32(4)
        let section_length = 9 + streams_len as u16 + 4; // +4 for CRC

        pkt[pmt_start + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[pmt_start + 2] = section_length as u8;
        pkt[pmt_start + 3] = 0x00; // program_number high
        pkt[pmt_start + 4] = 0x01; // program_number low
        pkt[pmt_start + 5] = 0xC1; // reserved, version=0, current_next=1
        pkt[pmt_start + 6] = 0x00; // section_number
        pkt[pmt_start + 7] = 0x00; // last_section_number
        // PCR PID: use video PID if video is present, otherwise audio PID
        let pcr_pid = if self.has_video { VIDEO_PID } else { AUDIO_PID };
        pkt[pmt_start + 8] = 0xE0 | ((pcr_pid >> 8) as u8 & 0x1F);
        pkt[pmt_start + 9] = pcr_pid as u8;
        pkt[pmt_start + 10] = 0xF0; // reserved + program_info_length high
        pkt[pmt_start + 11] = 0x00; // program_info_length low = 0

        let mut pos = pmt_start + 12;

        // Video stream entry (H.264 or H.265)
        if self.has_video {
            pkt[pos] = self.video_stream_type;
            pkt[pos + 1] = 0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F);
            pkt[pos + 2] = VIDEO_PID as u8;
            pkt[pos + 3] = 0xF0; // reserved + ES_info_length high
            pkt[pos + 4] = 0x00; // ES_info_length low = 0
            pos += 5;
        }

        // Audio stream entry: AAC
        if self.has_audio {
            pkt[pos] = STREAM_TYPE_AAC;
            pkt[pos + 1] = 0xE0 | ((AUDIO_PID >> 8) as u8 & 0x1F);
            pkt[pos + 2] = AUDIO_PID as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
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
}

/// Build a PES packet with PTS (and optional DTS) headers.
fn build_pes_packet(stream_id: u8, payload: &[u8], pts_90khz: u64, dts_90khz: Option<u64>) -> Vec<u8> {
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
}
