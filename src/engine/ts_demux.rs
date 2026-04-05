// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! MPEG-TS demuxer for WebRTC output.
//!
//! Extracts H.264 NAL units and Opus audio frames from an MPEG-TS stream
//! carried in `RtpPacket` broadcast channel messages. Uses PAT/PMT parsing
//! from `ts_parse.rs` to discover elementary stream PIDs.

use std::collections::HashMap;

use crate::engine::ts_parse::*;

/// Stream type constants from ISO/IEC 13818-1.
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_H265: u8 = 0x24;
const STREAM_TYPE_PRIVATE: u8 = 0x06;
const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;

/// Extracted media frame from the TS demuxer.
pub enum DemuxedFrame {
    /// Complete H.264 access unit (one or more NALUs in Annex B format).
    H264 {
        /// NAL units with 0x00000001 start codes stripped.
        /// Each entry is a single NALU (header byte + body).
        nalus: Vec<Vec<u8>>,
        /// Presentation timestamp in 90 kHz clock ticks.
        pts: u64,
        /// Whether this is a keyframe (contains IDR NALU).
        is_keyframe: bool,
    },
    /// Opus audio frame (not yet supported for output — placeholder variant).
    Opus,
    /// AAC audio frame (raw, ADTS header stripped).
    Aac {
        /// Raw AAC frame data (without ADTS header).
        data: Vec<u8>,
        /// Presentation timestamp in 90 kHz clock ticks.
        pts: u64,
    },
}

/// Per-PID PES reassembly state.
struct PesAssembler {
    /// Accumulated PES data.
    buffer: Vec<u8>,
    /// Whether we've seen the start of a PES packet.
    started: bool,
    /// Stream type from PMT.
    stream_type: u8,
}

/// MPEG-TS demuxer that extracts elementary stream frames.
pub struct TsDemuxer {
    /// PMT PIDs discovered from PAT.
    pmt_pids: HashMap<u16, ()>,
    /// Video PID (H.264 or H.265) discovered from PMT.
    video_pid: Option<u16>,
    /// Video stream type.
    video_stream_type: u8,
    /// Audio PID (Opus) discovered from PMT.
    audio_pid: Option<u16>,
    /// Per-PID PES reassembly.
    pes_assemblers: HashMap<u16, PesAssembler>,
    /// Cached SPS NALU for late joiners.
    cached_sps: Option<Vec<u8>>,
    /// Cached PPS NALU for late joiners.
    cached_pps: Option<Vec<u8>>,
    /// Cached AAC config: (profile, sample_rate_index, channel_config) from first ADTS header.
    cached_aac_config: Option<(u8, u8, u8)>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self {
            pmt_pids: HashMap::new(),
            video_pid: None,
            video_stream_type: 0,
            audio_pid: None,
            pes_assemblers: HashMap::new(),
            cached_sps: None,
            cached_pps: None,
            cached_aac_config: None,
        }
    }

    /// Get the cached SPS NALU (for SDP or late-joiner keyframe injection).
    pub fn cached_sps(&self) -> Option<&[u8]> {
        self.cached_sps.as_deref()
    }

    /// Get the cached PPS NALU.
    pub fn cached_pps(&self) -> Option<&[u8]> {
        self.cached_pps.as_deref()
    }

    /// Get the cached AAC config: (profile, sample_rate_index, channel_config).
    /// Parsed from the first ADTS header encountered.
    pub fn cached_aac_config(&self) -> Option<(u8, u8, u8)> {
        self.cached_aac_config
    }

    /// Process TS payload bytes (from an RtpPacket, after RTP header stripping).
    /// Returns any completed frames.
    pub fn demux(&mut self, ts_data: &[u8]) -> Vec<DemuxedFrame> {
        let mut frames = Vec::new();
        let mut offset = 0;

        while offset + TS_PACKET_SIZE <= ts_data.len() {
            let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                let mut new_frames = self.process_ts_packet(pkt);
                frames.append(&mut new_frames);
            }
            offset += TS_PACKET_SIZE;
        }

        frames
    }

    fn process_ts_packet(&mut self, pkt: &[u8]) -> Vec<DemuxedFrame> {
        let pid = ts_pid(pkt);

        // PAT
        if pid == PAT_PID && ts_pusi(pkt) {
            let pids = parse_pat_pmt_pids(pkt);
            self.pmt_pids.clear();
            for p in pids {
                self.pmt_pids.insert(p, ());
            }
            return Vec::new();
        }

        // PMT
        if self.pmt_pids.contains_key(&pid) && ts_pusi(pkt) {
            self.parse_pmt(pkt);
            return Vec::new();
        }

        // Video PID
        if Some(pid) == self.video_pid {
            return self.process_es_packet(pkt, pid);
        }

        // Audio PID
        if Some(pid) == self.audio_pid {
            return self.process_es_packet(pkt, pid);
        }

        Vec::new()
    }

    /// Parse PMT to discover video and audio PIDs.
    fn parse_pmt(&mut self, pkt: &[u8]) {
        let mut offset = 4;
        if ts_has_adaptation(pkt) {
            let af_len = pkt[4] as usize;
            offset = 5 + af_len;
        }
        if offset >= TS_PACKET_SIZE {
            return;
        }

        let pointer = pkt[offset] as usize;
        offset += 1 + pointer;

        if offset + 12 > TS_PACKET_SIZE {
            return;
        }
        if pkt[offset] != 0x02 {
            return; // Not PMT
        }

        let section_length =
            (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
        let program_info_length =
            (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

        let data_start = offset + 12 + program_info_length;
        let data_end = (offset + 3 + section_length)
            .min(TS_PACKET_SIZE)
            .saturating_sub(4);

        let mut pos = data_start;
        while pos + 5 <= data_end {
            let stream_type = pkt[pos];
            let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
            let es_info_length =
                (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

            // Check descriptors for Opus registration
            let desc_start = pos + 5;
            let desc_end = (desc_start + es_info_length).min(data_end);
            let is_opus = self.check_opus_descriptor(&pkt[desc_start..desc_end]);

            match stream_type {
                STREAM_TYPE_H264 | STREAM_TYPE_H265 => {
                    if self.video_pid != Some(es_pid) {
                        tracing::info!(
                            "TS demux: video PID 0x{:04X} (stream_type=0x{:02X})",
                            es_pid,
                            stream_type
                        );
                        self.video_pid = Some(es_pid);
                        self.video_stream_type = stream_type;
                        self.pes_assemblers.insert(
                            es_pid,
                            PesAssembler {
                                buffer: Vec::with_capacity(256 * 1024),
                                started: false,
                                stream_type,
                            },
                        );
                    }
                }
                STREAM_TYPE_PRIVATE if is_opus => {
                    if self.audio_pid != Some(es_pid) {
                        tracing::info!("TS demux: Opus audio PID 0x{:04X}", es_pid);
                        self.audio_pid = Some(es_pid);
                        self.pes_assemblers.insert(
                            es_pid,
                            PesAssembler {
                                buffer: Vec::with_capacity(16 * 1024),
                                started: false,
                                stream_type,
                            },
                        );
                    }
                }
                STREAM_TYPE_AAC_ADTS => {
                    if self.audio_pid != Some(es_pid) {
                        tracing::info!("TS demux: AAC audio PID 0x{:04X}", es_pid);
                        self.audio_pid = Some(es_pid);
                        self.pes_assemblers.insert(
                            es_pid,
                            PesAssembler {
                                buffer: Vec::with_capacity(16 * 1024),
                                started: false,
                                stream_type,
                            },
                        );
                    }
                }
                _ => {}
            }

            pos += 5 + es_info_length;
        }
    }

    /// Check ES descriptors for Opus registration descriptor.
    fn check_opus_descriptor(&self, descriptors: &[u8]) -> bool {
        let mut pos = 0;
        while pos + 2 <= descriptors.len() {
            let tag = descriptors[pos];
            let len = descriptors[pos + 1] as usize;
            if pos + 2 + len > descriptors.len() {
                break;
            }
            // Registration descriptor (tag 0x05) with "Opus" identifier
            if tag == 0x05 && len >= 4 && &descriptors[pos + 2..pos + 6] == b"Opus" {
                return true;
            }
            pos += 2 + len;
        }
        false
    }

    /// Process a TS packet belonging to a known ES PID (video or audio).
    fn process_es_packet(&mut self, pkt: &[u8], pid: u16) -> Vec<DemuxedFrame> {
        if !ts_has_payload(pkt) {
            return Vec::new();
        }

        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            return Vec::new();
        }
        let payload = &pkt[payload_start..];

        // Extract the PES data to parse before mutably borrowing assembler
        let completed_pes = if pusi {
            if let Some(assembler) = self.pes_assemblers.get(&pid) {
                if assembler.started && !assembler.buffer.is_empty() {
                    let pes_data = assembler.buffer.clone();
                    let stream_type = assembler.stream_type;
                    Some((pes_data, stream_type))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Parse any completed PES (before we modify the assembler)
        let result = completed_pes
            .map(|(pes_data, stream_type)| self.parse_pes(&pes_data, pid, stream_type))
            .unwrap_or_default();

        // Now update the assembler
        if let Some(assembler) = self.pes_assemblers.get_mut(&pid) {
            if pusi {
                assembler.buffer.clear();
                assembler.buffer.extend_from_slice(payload);
                assembler.started = true;
            } else if assembler.started {
                assembler.buffer.extend_from_slice(payload);
            }
        }

        result
    }

    /// Parse a complete PES packet and extract the elementary stream frame(s).
    fn parse_pes(&mut self, pes: &[u8], pid: u16, stream_type: u8) -> Vec<DemuxedFrame> {
        // PES header: 0x000001 + stream_id(1) + length(2) + flags(2) + header_data_len(1)
        if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
            return Vec::new();
        }

        let header_data_len = pes[8] as usize;
        let es_start = 9 + header_data_len;
        if es_start >= pes.len() {
            return Vec::new();
        }

        // Parse PTS if present
        let pts_dts_flags = (pes[7] >> 6) & 0x03;
        let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
            Some(parse_pts(&pes[9..14]))
        } else {
            None
        };

        let es_data = &pes[es_start..];

        match stream_type {
            STREAM_TYPE_H264 => {
                let nalus = self.extract_h264_nalus(es_data);
                if nalus.is_empty() {
                    return Vec::new();
                }
                let is_keyframe = nalus.iter().any(|n| !n.is_empty() && (n[0] & 0x1F) == 5);
                vec![DemuxedFrame::H264 {
                    nalus,
                    pts: pts.unwrap_or(0),
                    is_keyframe,
                }]
            }
            STREAM_TYPE_PRIVATE if Some(pid) == self.audio_pid => {
                vec![DemuxedFrame::Opus]
            }
            STREAM_TYPE_AAC_ADTS if Some(pid) == self.audio_pid => {
                // A single PES may contain multiple ADTS frames concatenated.
                self.extract_aac_frames(es_data, pts.unwrap_or(0))
            }
            _ => Vec::new(),
        }
    }

    /// Extract individual H.264 NALUs from Annex B byte stream.
    /// Splits on 0x00000001 or 0x000001 start codes.
    fn extract_h264_nalus(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut nalus = Vec::new();
        let mut i = 0;

        // Find start codes and extract NALUs
        while i < data.len() {
            // Look for start code: 0x000001 or 0x00000001
            let (start_code_len, found) = if i + 3 < data.len()
                && data[i] == 0x00
                && data[i + 1] == 0x00
                && data[i + 2] == 0x00
                && data[i + 3] == 0x01
            {
                (4, true)
            } else if i + 2 < data.len()
                && data[i] == 0x00
                && data[i + 1] == 0x00
                && data[i + 2] == 0x01
            {
                (3, true)
            } else {
                (0, false)
            };

            if !found {
                i += 1;
                continue;
            }

            let nalu_start = i + start_code_len;

            // Find end of this NALU (next start code or end of data)
            let mut nalu_end = data.len();
            let mut j = nalu_start + 1;
            while j + 2 < data.len() {
                if data[j] == 0x00
                    && data[j + 1] == 0x00
                    && (data[j + 2] == 0x01
                        || (j + 3 < data.len() && data[j + 2] == 0x00 && data[j + 3] == 0x01))
                {
                    nalu_end = j;
                    break;
                }
                j += 1;
            }

            if nalu_start < nalu_end {
                let nalu = &data[nalu_start..nalu_end];
                if !nalu.is_empty() {
                    let nalu_type = nalu[0] & 0x1F;
                    // Cache SPS/PPS for late joiners
                    if nalu_type == 7 {
                        self.cached_sps = Some(nalu.to_vec());
                    } else if nalu_type == 8 {
                        self.cached_pps = Some(nalu.to_vec());
                    }
                    nalus.push(nalu.to_vec());
                }
            }

            i = nalu_end;
        }

        nalus
    }

    /// Extract all AAC frames from ADTS-wrapped data.
    /// Strips ADTS headers and caches the audio config on first call.
    /// A single PES may contain multiple concatenated ADTS frames.
    fn extract_aac_frames(&mut self, data: &[u8], base_pts: u64) -> Vec<DemuxedFrame> {
        let mut frames = Vec::new();
        let mut offset = 0;
        let mut frame_index = 0u32;

        while offset + 7 <= data.len() {
            // Check ADTS sync word: 0xFFF
            if data[offset] != 0xFF || (data[offset + 1] & 0xF0) != 0xF0 {
                break;
            }

            let protection_absent = (data[offset + 1] & 0x01) != 0;
            let header_len = if protection_absent { 7 } else { 9 };

            if offset + header_len > data.len() {
                break;
            }

            // Cache AAC config from first ADTS header
            if self.cached_aac_config.is_none() {
                let profile = (data[offset + 2] >> 6) & 0x03;
                let sample_rate_idx = (data[offset + 2] >> 2) & 0x0F;
                let channel_config = ((data[offset + 2] & 0x01) << 2) | ((data[offset + 3] >> 6) & 0x03);
                self.cached_aac_config = Some((profile, sample_rate_idx, channel_config));
                tracing::debug!(
                    "AAC config: profile={}, sample_rate_idx={}, channels={}",
                    profile + 1, sample_rate_idx, channel_config,
                );
            }

            // ADTS frame length (13 bits): includes header + raw frame
            let frame_length = (((data[offset + 3] & 0x03) as usize) << 11)
                | ((data[offset + 4] as usize) << 3)
                | ((data[offset + 5] >> 5) as usize);

            if frame_length < header_len || offset + frame_length > data.len() {
                break;
            }

            let raw_start = offset + header_len;
            let raw_end = offset + frame_length;

            if raw_start < raw_end {
                // Estimate PTS offset for subsequent frames (~21.3ms per 1024 samples at 48kHz)
                let pts = base_pts + (frame_index as u64) * 1920; // 1920 = 1024*90000/48000
                frames.push(DemuxedFrame::Aac {
                    data: data[raw_start..raw_end].to_vec(),
                    pts,
                });
            }

            offset += frame_length;
            frame_index += 1;
        }

        frames
    }
}

/// Parse a 5-byte PTS field from PES header.
fn parse_pts(data: &[u8]) -> u64 {
    let pts = ((data[0] as u64 & 0x0E) << 29)
        | ((data[1] as u64) << 22)
        | ((data[2] as u64 & 0xFE) << 14)
        | ((data[3] as u64) << 7)
        | ((data[4] as u64) >> 1);
    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annex_b_nalu_extraction() {
        let mut demux = TsDemuxer::new();

        // Annex B stream with two NALUs: SPS and PPS
        let data = [
            0x00, 0x00, 0x00, 0x01, // start code
            0x67, 0x42, 0x00, 0x1E, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, // start code
            0x68, 0xCE, 0x38, 0x80, // PPS (type 8)
        ];

        let nalus = demux.extract_h264_nalus(&data);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0][0] & 0x1F, 7); // SPS
        assert_eq!(nalus[1][0] & 0x1F, 8); // PPS

        // SPS/PPS should be cached
        assert!(demux.cached_sps.is_some());
        assert!(demux.cached_pps.is_some());
    }

    #[test]
    fn test_three_byte_start_code() {
        let mut demux = TsDemuxer::new();

        let data = [
            0x00, 0x00, 0x01, // 3-byte start code
            0x65, 0x01, 0x02, // IDR (type 5)
        ];

        let nalus = demux.extract_h264_nalus(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0][0] & 0x1F, 5); // IDR
    }

    #[test]
    fn test_pts_parsing() {
        // PTS = 0 encoded as: 0010_xxx1 xxxx_xxxx xxxx_xxx1 xxxx_xxxx xxxx_xxx1
        let data = [0x21, 0x00, 0x01, 0x00, 0x01];
        let pts = parse_pts(&data);
        assert_eq!(pts, 0);

        // PTS = 90000 (1 second at 90kHz)
        // 90000 = 0x15F90
        // Spread into 5 bytes with marker bits
        let pts_val: u64 = 90000;
        let b0 = (0x20 | ((pts_val >> 29) & 0x0E) as u8) | 0x01;
        let b1 = ((pts_val >> 22) & 0xFF) as u8;
        let b2 = (((pts_val >> 14) & 0xFE) as u8) | 0x01;
        let b3 = ((pts_val >> 7) & 0xFF) as u8;
        let b4 = (((pts_val & 0x7F) << 1) as u8) | 0x01;
        let encoded = [b0, b1, b2, b3, b4];
        let decoded = parse_pts(&encoded);
        assert_eq!(decoded, 90000);
    }
}
