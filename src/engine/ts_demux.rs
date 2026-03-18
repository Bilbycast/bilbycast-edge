//! MPEG-2 Transport Stream demuxer.
//!
//! Provides zero-copy TS packet parsing, PAT/PMT discovery, PES reassembly,
//! and elementary stream extraction. Used by:
//! - RTMP output: extracts H.264 NALUs and AAC frames for FLV muxing
//! - WebRTC output: extracts H.264 NALUs for RFC 6184 repacketization
//! - TR-07 awareness: detects JPEG XS stream type (0x61) in PMT
//!
//! Each output that needs demuxing creates its own `TsDemuxer` instance — there
//! is no shared mutable state between outputs.

use bytes::Bytes;

// ── Constants ──────────────────────────────────────────────────────────────

pub const TS_PACKET_SIZE: usize = 188;
pub const TS_SYNC_BYTE: u8 = 0x47;
pub const PAT_PID: u16 = 0x0000;
pub const NULL_PID: u16 = 0x1FFF;

/// Well-known MPEG-2 stream types
pub const STREAM_TYPE_H264: u8 = 0x1B;
pub const STREAM_TYPE_H265: u8 = 0x24;
pub const STREAM_TYPE_AAC: u8 = 0x0F;
pub const STREAM_TYPE_AAC_LATM: u8 = 0x11;
pub const STREAM_TYPE_OPUS: u8 = 0x06; // Private data, identified by descriptor
pub const STREAM_TYPE_JPEG_XS: u8 = 0x61;

// ── Inline TS Packet Parsing ───────────────────────────────────────────────

/// Extract PID from TS packet header bytes 1-2.
#[inline(always)]
pub fn ts_pid(pkt: &[u8]) -> u16 {
    ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16
}

/// Check Transport Error Indicator bit.
#[inline(always)]
pub fn ts_tei(pkt: &[u8]) -> bool {
    (pkt[1] & 0x80) != 0
}

/// Check Payload Unit Start Indicator bit.
#[inline(always)]
pub fn ts_pusi(pkt: &[u8]) -> bool {
    (pkt[1] & 0x40) != 0
}

/// Extract continuity counter (4 bits).
#[inline(always)]
pub fn ts_cc(pkt: &[u8]) -> u8 {
    pkt[3] & 0x0F
}

/// Extract adaptation field control bits.
#[inline(always)]
pub fn ts_adaptation_field_control(pkt: &[u8]) -> u8 {
    (pkt[3] >> 4) & 0x03
}

/// Check if TS packet carries payload.
#[inline(always)]
pub fn ts_has_payload(pkt: &[u8]) -> bool {
    ts_adaptation_field_control(pkt) & 0x01 != 0
}

/// Check if TS packet has adaptation field.
#[inline(always)]
pub fn ts_has_adaptation(pkt: &[u8]) -> bool {
    ts_adaptation_field_control(pkt) & 0x02 != 0
}

/// Get the byte offset where payload data begins in a TS packet.
pub fn ts_payload_offset(pkt: &[u8]) -> usize {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    offset
}

// ── Access Unit ────────────────────────────────────────────────────────────

/// A single elementary stream access unit extracted from PES reassembly.
///
/// For H.264, this is typically one or more NALUs constituting a complete
/// access unit (frame). For AAC, this is one audio frame.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// PID this access unit was extracted from.
    pub pid: u16,
    /// MPEG-2 stream type from the PMT (0x1B = H.264, 0x0F = AAC, etc.).
    pub stream_type: u8,
    /// Raw access unit data (NALUs for video, frame for audio).
    pub data: Bytes,
    /// Presentation timestamp in 90kHz ticks (from PES header), if present.
    pub pts: Option<u64>,
    /// Decode timestamp in 90kHz ticks (from PES header), if present.
    pub dts: Option<u64>,
}

// ── Elementary Stream Info ─────────────────────────────────────────────────

/// Information about a discovered elementary stream from PMT parsing.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// Elementary stream PID.
    pub pid: u16,
    /// Stream type code from the PMT.
    pub stream_type: u8,
}

// ── PES Reassembly Buffer ──────────────────────────────────────────────────

/// Per-PID PES packet reassembly state.
struct PesBuffer {
    /// Accumulated PES data (header + payload).
    data: Vec<u8>,
    /// Stream type from PMT for this PID.
    stream_type: u8,
    /// Whether we have seen the first PUSI for this PID.
    started: bool,
}

// ── TS Demuxer ─────────────────────────────────────────────────────────────

/// Synchronous MPEG-2 Transport Stream demuxer.
///
/// Maintains per-PID state for PAT/PMT parsing and PES reassembly.
/// Each call to [`feed`] accepts raw TS packets (typically stripped from
/// RTP payload) and returns any completed access units.
///
/// # Usage
///
/// ```ignore
/// let mut demuxer = TsDemuxer::new();
/// // Strip RTP header, feed TS packets
/// let access_units = demuxer.feed(&ts_payload);
/// for au in access_units {
///     match au.stream_type {
///         STREAM_TYPE_H264 => { /* process H.264 NALUs */ }
///         STREAM_TYPE_AAC => { /* process AAC frame */ }
///         _ => {}
///     }
/// }
/// ```
pub struct TsDemuxer {
    /// PMT PIDs discovered from PAT (program_number → PMT PID).
    pmt_pids: Vec<u16>,
    /// Elementary streams discovered from PMT (PID → StreamInfo).
    streams: Vec<StreamInfo>,
    /// Per-PID PES reassembly buffers.
    pes_buffers: Vec<(u16, PesBuffer)>,
    /// Whether JPEG XS (stream type 0x61) was detected in any PMT.
    pub jpeg_xs_detected: bool,
    /// PID of JPEG XS stream, if detected.
    pub jpeg_xs_pid: Option<u16>,
    /// Whether H.264 was detected.
    pub h264_detected: bool,
    /// Whether H.265 was detected.
    pub h265_detected: bool,
    /// Whether AAC audio was detected.
    pub aac_detected: bool,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self {
            pmt_pids: Vec::new(),
            streams: Vec::new(),
            pes_buffers: Vec::new(),
            jpeg_xs_detected: false,
            jpeg_xs_pid: None,
            h264_detected: false,
            h265_detected: false,
            aac_detected: false,
        }
    }

    /// Returns all discovered elementary streams from the latest PMT.
    pub fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// Feed raw bytes containing concatenated 188-byte TS packets.
    /// Returns any completed access units.
    pub fn feed(&mut self, data: &[u8]) -> Vec<AccessUnit> {
        let mut access_units = Vec::new();
        let mut offset = 0;

        while offset + TS_PACKET_SIZE <= data.len() {
            let pkt = &data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            // Skip packets without sync byte
            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }

            // Skip packets with transport error
            if ts_tei(pkt) {
                continue;
            }

            let pid = ts_pid(pkt);

            // Skip null packets
            if pid == NULL_PID {
                continue;
            }

            // PAT handling
            if pid == PAT_PID {
                self.process_pat(pkt);
                continue;
            }

            // PMT handling
            if self.pmt_pids.contains(&pid) {
                self.process_pmt(pkt);
                continue;
            }

            // PES data for known elementary streams
            if let Some(stream_type) = self.stream_type_for_pid(pid) {
                if let Some(au) = self.process_pes_packet(pid, stream_type, pkt) {
                    access_units.push(au);
                }
            }
        }

        access_units
    }

    /// Look up stream type for a PID.
    fn stream_type_for_pid(&self, pid: u16) -> Option<u8> {
        self.streams.iter().find(|s| s.pid == pid).map(|s| s.stream_type)
    }

    /// Parse PAT to discover PMT PIDs.
    fn process_pat(&mut self, pkt: &[u8]) {
        if !ts_pusi(pkt) {
            return;
        }

        let mut offset = ts_payload_offset(pkt);
        if offset >= TS_PACKET_SIZE {
            return;
        }

        // pointer_field
        let pointer = pkt[offset] as usize;
        offset += 1 + pointer;

        // PAT section header: table_id(1) + flags+length(2) + ts_id(2) +
        // version/cni(1) + section_number(1) + last_section(1) = 8 bytes
        if offset + 8 > TS_PACKET_SIZE {
            return;
        }
        let table_id = pkt[offset];
        if table_id != 0x00 {
            return;
        }
        let section_length =
            (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
        let data_start = offset + 8;
        let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

        let mut new_pmt_pids = Vec::new();
        let mut pos = data_start;
        while pos + 4 <= data_end {
            let program_number = ((pkt[pos] as u16) << 8) | pkt[pos + 1] as u16;
            let pid = ((pkt[pos + 2] as u16 & 0x1F) << 8) | pkt[pos + 3] as u16;
            if program_number != 0 {
                new_pmt_pids.push(pid);
            }
            pos += 4;
        }

        if !new_pmt_pids.is_empty() {
            self.pmt_pids = new_pmt_pids;
        }
    }

    /// Parse PMT to discover elementary stream PIDs and types.
    fn process_pmt(&mut self, pkt: &[u8]) {
        if !ts_pusi(pkt) {
            return;
        }

        let mut offset = ts_payload_offset(pkt);
        if offset >= TS_PACKET_SIZE {
            return;
        }

        // pointer_field
        let pointer = pkt[offset] as usize;
        offset += 1 + pointer;

        // PMT section header
        if offset + 12 > TS_PACKET_SIZE {
            return;
        }
        let table_id = pkt[offset];
        if table_id != 0x02 {
            return; // Not a PMT
        }
        let section_length =
            (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);

        // Skip to program_info_length (offset + 10)
        let program_info_length =
            (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

        let data_start = offset + 12 + program_info_length;
        let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

        let mut new_streams = Vec::new();
        let mut pos = data_start;

        while pos + 5 <= data_end {
            let stream_type = pkt[pos];
            let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
            let es_info_length =
                (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

            new_streams.push(StreamInfo {
                pid: es_pid,
                stream_type,
            });

            // Detect codec types
            match stream_type {
                STREAM_TYPE_H264 => self.h264_detected = true,
                STREAM_TYPE_H265 => self.h265_detected = true,
                STREAM_TYPE_AAC | STREAM_TYPE_AAC_LATM => self.aac_detected = true,
                STREAM_TYPE_JPEG_XS => {
                    self.jpeg_xs_detected = true;
                    self.jpeg_xs_pid = Some(es_pid);
                }
                _ => {}
            }

            pos += 5 + es_info_length;
        }

        if !new_streams.is_empty() {
            self.streams = new_streams;
        }
    }

    /// Process a TS packet belonging to an elementary stream PES.
    /// Returns a completed access unit when a new PES packet starts
    /// (flushing the previous one).
    fn process_pes_packet(&mut self, pid: u16, stream_type: u8, pkt: &[u8]) -> Option<AccessUnit> {
        if !ts_has_payload(pkt) {
            return None;
        }

        let payload_offset = ts_payload_offset(pkt);
        if payload_offset >= TS_PACKET_SIZE {
            return None;
        }
        let payload = &pkt[payload_offset..];

        let is_pusi = ts_pusi(pkt);

        // Find or create PES buffer for this PID
        let buf_idx = self.pes_buffers.iter().position(|(p, _)| *p == pid);

        if is_pusi {
            // New PES packet starting — flush any existing buffer
            let completed = if let Some(idx) = buf_idx {
                let stream_type_val = self.pes_buffers[idx].1.stream_type;
                let started = self.pes_buffers[idx].1.started;
                let is_empty = self.pes_buffers[idx].1.data.is_empty();
                if started && !is_empty {
                    // Clone the data to avoid borrow conflict with self
                    let data_clone = self.pes_buffers[idx].1.data.clone();
                    let au = self.extract_access_unit(pid, stream_type_val, &data_clone);
                    self.pes_buffers[idx].1.data.clear();
                    au
                } else {
                    self.pes_buffers[idx].1.data.clear();
                    None
                }
            } else {
                None
            };

            // Start new PES buffer
            if let Some(idx) = buf_idx {
                let (_, ref mut buf) = self.pes_buffers[idx];
                buf.data.extend_from_slice(payload);
                buf.started = true;
                buf.stream_type = stream_type;
            } else {
                let mut buf = PesBuffer {
                    data: Vec::with_capacity(65536),
                    stream_type,
                    started: true,
                };
                buf.data.extend_from_slice(payload);
                self.pes_buffers.push((pid, buf));
            }

            return completed;
        }

        // Continuation packet
        if let Some(idx) = buf_idx {
            let (_, ref mut buf) = self.pes_buffers[idx];
            if buf.started {
                buf.data.extend_from_slice(payload);
            }
        }

        None
    }

    /// Extract PTS/DTS from PES header and return an AccessUnit with
    /// the raw elementary stream data (after the PES header).
    fn extract_access_unit(&self, pid: u16, stream_type: u8, pes_data: &[u8]) -> Option<AccessUnit> {
        // Minimum PES header: start code (3) + stream_id (1) + length (2) + flags (3)
        if pes_data.len() < 9 {
            return None;
        }

        // Verify PES start code: 0x00 0x00 0x01
        if pes_data[0] != 0x00 || pes_data[1] != 0x00 || pes_data[2] != 0x01 {
            return None;
        }

        let pes_header_data_length = pes_data[8] as usize;
        let header_end = 9 + pes_header_data_length;
        if header_end > pes_data.len() {
            return None;
        }

        // Parse PTS/DTS from PES header flags
        let pts_dts_flags = (pes_data[7] >> 6) & 0x03;
        let pts = if pts_dts_flags >= 2 && pes_data.len() >= 14 {
            Some(parse_timestamp(&pes_data[9..14]))
        } else {
            None
        };
        let dts = if pts_dts_flags == 3 && pes_data.len() >= 19 {
            Some(parse_timestamp(&pes_data[14..19]))
        } else {
            pts // If no DTS, use PTS
        };

        let es_data = &pes_data[header_end..];
        if es_data.is_empty() {
            return None;
        }

        Some(AccessUnit {
            pid,
            stream_type,
            data: Bytes::copy_from_slice(es_data),
            pts,
            dts,
        })
    }
}

/// Parse a 5-byte PTS or DTS timestamp from PES header.
fn parse_timestamp(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;

    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) << 15)
        | (b3 << 7)
        | (b4 >> 1)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ts_packet(pid: u16, cc: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        if pusi {
            pkt[1] |= 0x40;
        }
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F); // payload only, no adaptation
        let copy_len = payload.len().min(TS_PACKET_SIZE - 4);
        pkt[4..4 + copy_len].copy_from_slice(&payload[..copy_len]);
        pkt
    }

    /// Build a synthetic PAT packet.
    fn make_pat(program_number: u16, pmt_pid: u16) -> Vec<u8> {
        // PAT section: table_id=0x00, section_syntax=1, section_length, ts_id, version, section_num, last_section, entries, CRC
        let mut section = vec![
            0x00, // table_id
            0xB0, 0x0D, // section_syntax_indicator=1, section_length=13
            0x00, 0x01, // transport_stream_id
            0xC1, // version=0, current_next=1
            0x00, // section_number
            0x00, // last_section_number
        ];
        // Program entry
        section.push((program_number >> 8) as u8);
        section.push(program_number as u8);
        section.push(0xE0 | ((pmt_pid >> 8) & 0x1F) as u8);
        section.push(pmt_pid as u8);
        // CRC placeholder (4 bytes)
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut payload = vec![0x00]; // pointer_field
        payload.extend_from_slice(&section);

        make_ts_packet(PAT_PID, 0, true, &payload)
    }

    /// Build a synthetic PMT packet with given streams.
    fn make_pmt(pmt_pid: u16, pcr_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
        let es_info_len: usize = streams.len() * 5;
        let section_length = 9 + es_info_len + 4; // header(9) + es_entries + CRC(4)

        let mut section = vec![
            0x02, // table_id
            0xB0, section_length as u8, // section_syntax_indicator=1
            0x00, 0x01, // program_number
            0xC1, // version=0, current_next=1
            0x00, // section_number
            0x00, // last_section_number
            0xE0 | ((pcr_pid >> 8) & 0x1F) as u8,
            pcr_pid as u8,
            0xF0, 0x00, // program_info_length=0
        ];

        for &(stream_type, es_pid) in streams {
            section.push(stream_type);
            section.push(0xE0 | ((es_pid >> 8) & 0x1F) as u8);
            section.push(es_pid as u8);
            section.push(0xF0); // ES_info_length high
            section.push(0x00); // ES_info_length low = 0
        }

        // CRC placeholder
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut payload = vec![0x00]; // pointer_field
        payload.extend_from_slice(&section);

        make_ts_packet(pmt_pid, 0, true, &payload)
    }

    #[test]
    fn test_ts_parsing_helpers() {
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x41; // PUSI set, PID high = 0x01
        pkt[2] = 0x00; // PID low = 0x00 → PID = 0x100
        pkt[3] = 0x15; // AFC=01 (payload only), CC=5

        assert_eq!(ts_pid(&pkt), 0x100);
        assert!(ts_pusi(&pkt));
        assert_eq!(ts_cc(&pkt), 5);
        assert!(ts_has_payload(&pkt));
        assert!(!ts_has_adaptation(&pkt));
        assert!(!ts_tei(&pkt));
    }

    #[test]
    fn test_pat_parsing() {
        let mut demuxer = TsDemuxer::new();
        let pat = make_pat(1, 0x100);
        demuxer.feed(&pat);
        assert_eq!(demuxer.pmt_pids, vec![0x100]);
    }

    #[test]
    fn test_pmt_h264_detection() {
        let mut demuxer = TsDemuxer::new();
        let pat = make_pat(1, 0x100);
        let pmt = make_pmt(0x100, 0x200, &[(STREAM_TYPE_H264, 0x200), (STREAM_TYPE_AAC, 0x201)]);
        demuxer.feed(&pat);
        demuxer.feed(&pmt);

        assert!(demuxer.h264_detected);
        assert!(demuxer.aac_detected);
        assert!(!demuxer.jpeg_xs_detected);
        assert_eq!(demuxer.streams.len(), 2);
    }

    #[test]
    fn test_pmt_jpeg_xs_detection() {
        let mut demuxer = TsDemuxer::new();
        let pat = make_pat(1, 0x100);
        let pmt = make_pmt(0x100, 0x200, &[(STREAM_TYPE_JPEG_XS, 0x200)]);
        demuxer.feed(&pat);
        demuxer.feed(&pmt);

        assert!(demuxer.jpeg_xs_detected);
        assert_eq!(demuxer.jpeg_xs_pid, Some(0x200));
    }

    #[test]
    fn test_timestamp_parsing() {
        // PTS = 0 encoded as 5 bytes: 0010 xxx1 ...
        let ts_bytes = [0x21, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(parse_timestamp(&ts_bytes), 0);
    }

    #[test]
    fn test_null_pid_skipped() {
        let mut demuxer = TsDemuxer::new();
        let pkt = make_ts_packet(NULL_PID, 0, false, &[0u8; 184]);
        let aus = demuxer.feed(&pkt);
        assert!(aus.is_empty());
    }
}
