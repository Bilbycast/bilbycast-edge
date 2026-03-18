//! FLV tag builder for H.264 (AVC) video and AAC audio.
//!
//! RTMP transports media as FLV-style tags, where each tag is an RTMP message
//! (audio = type 8, video = type 9). The tag body carries codec-specific headers
//! that describe the codec configuration and the frame type.
//!
//! ## Video tags (H.264 / AVC)
//!
//! ```text
//! Byte 0:   [frame_type:4][codec_id:4]
//!             frame_type: 1 = key frame, 2 = inter frame
//!             codec_id:   7 = AVC
//! Byte 1:   AVC packet type: 0 = sequence header, 1 = NALU
//! Bytes 2-4: Composition time offset (signed 24-bit, big-endian) — usually 0
//! Bytes 5+:  AVC data:
//!            - For sequence header: AVCDecoderConfigurationRecord (SPS + PPS)
//!            - For NALU: one or more NALUs, each prefixed by 4-byte big-endian length
//! ```
//!
//! ## Audio tags (AAC)
//!
//! ```text
//! Byte 0:   [sound_format:4][rate:2][size:1][type:1]
//!            sound_format: 10 = AAC
//!            rate: 3 = 44 kHz (always 3 for AAC, even if actual rate differs)
//!            size: 1 = 16-bit
//!            type: 1 = stereo (or 0 for mono)
//! Byte 1:   AAC packet type: 0 = sequence header (AudioSpecificConfig), 1 = raw
//! Bytes 2+: AAC data
//! ```

use bytes::{BufMut, BytesMut};

// ---------------------------------------------------------------------------
// Video tags — H.264 / AVC
// ---------------------------------------------------------------------------

/// Frame types for the FLV video tag header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFrameType {
    KeyFrame = 1,
    InterFrame = 2,
}

/// AVC packet types.
const AVC_PACKET_SEQUENCE_HEADER: u8 = 0;
const AVC_PACKET_NALU: u8 = 1;

/// Codec ID for H.264 (AVC).
const CODEC_AVC: u8 = 7;

/// Build an AVC sequence header tag body from raw SPS and PPS NAL units.
///
/// This constructs the `AVCDecoderConfigurationRecord` (ISO 14496-15) that
/// RTMP servers need before they can decode video frames.
///
/// `sps` and `pps` should be raw NAL unit bytes (without start codes or length prefixes).
pub fn build_avc_sequence_header(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(32 + sps.len() + pps.len());

    // FLV video tag header byte: key frame (1) + AVC codec (7).
    buf.put_u8((VideoFrameType::KeyFrame as u8) << 4 | CODEC_AVC);
    // AVC packet type: sequence header.
    buf.put_u8(AVC_PACKET_SEQUENCE_HEADER);
    // Composition time offset (3 bytes, 0 for sequence header).
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);

    // AVCDecoderConfigurationRecord:
    //   configurationVersion = 1
    //   AVCProfileIndication = sps[1]
    //   profile_compatibility = sps[2]
    //   AVCLevelIndication = sps[3]
    //   lengthSizeMinusOne = 3 (4-byte NAL length prefix) | reserved 0xFC
    //   numOfSequenceParameterSets = 1 | reserved 0xE0
    //   SPS length (2 bytes) + SPS bytes
    //   numOfPictureParameterSets = 1
    //   PPS length (2 bytes) + PPS bytes
    buf.put_u8(1); // configurationVersion
    if sps.len() >= 4 {
        buf.put_u8(sps[1]); // AVCProfileIndication
        buf.put_u8(sps[2]); // profile_compatibility
        buf.put_u8(sps[3]); // AVCLevelIndication
    } else {
        // Fallback: Baseline profile, level 3.0
        buf.put_u8(0x42);
        buf.put_u8(0x00);
        buf.put_u8(0x1E);
    }
    buf.put_u8(0xFF); // lengthSizeMinusOne=3 | reserved bits

    // SPS
    buf.put_u8(0xE1); // numOfSPS=1 | reserved bits
    buf.put_u16(sps.len() as u16);
    buf.put_slice(sps);

    // PPS
    buf.put_u8(1); // numOfPPS
    buf.put_u16(pps.len() as u16);
    buf.put_slice(pps);

    buf.to_vec()
}

/// Build a video NALU data tag body.
///
/// `nalu_data` should be one or more NALUs, each prefixed by a 4-byte big-endian
/// length. If your input is Annex B (start codes), convert to length-prefixed first.
///
/// `is_keyframe` indicates whether this is an IDR / key frame.
pub fn build_avc_nalu_tag(nalu_data: &[u8], is_keyframe: bool, composition_time_ms: i32) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(5 + nalu_data.len());

    let frame_type = if is_keyframe {
        VideoFrameType::KeyFrame
    } else {
        VideoFrameType::InterFrame
    };

    buf.put_u8((frame_type as u8) << 4 | CODEC_AVC);
    buf.put_u8(AVC_PACKET_NALU);
    // Composition time offset: signed 24-bit big-endian.
    let ct = composition_time_ms as u32;
    buf.put_u8((ct >> 16) as u8);
    buf.put_u8((ct >> 8) as u8);
    buf.put_u8(ct as u8);
    buf.put_slice(nalu_data);

    buf.to_vec()
}

/// Convert an Annex B byte stream (with 0x00000001 or 0x000001 start codes)
/// into length-prefixed NALU format (4-byte big-endian lengths).
///
/// Returns the converted buffer. Each NALU is prefixed with its length.
pub fn annex_b_to_length_prefixed(data: &[u8]) -> Vec<u8> {
    let mut out = BytesMut::with_capacity(data.len());
    let nalus = split_annex_b_nalus(data);
    for nalu in &nalus {
        out.put_u32(nalu.len() as u32);
        out.put_slice(nalu);
    }
    out.to_vec()
}

/// Split an Annex B byte stream into individual NAL units (without start codes).
pub fn split_annex_b_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut i = 0;

    // Find first start code.
    while i < data.len() {
        if let Some(sc_len) = start_code_len(data, i) {
            i += sc_len;
            break;
        }
        i += 1;
    }

    let mut nalu_start = i;

    while i < data.len() {
        if let Some(sc_len) = start_code_len(data, i) {
            if i > nalu_start {
                nalus.push(&data[nalu_start..i]);
            }
            i += sc_len;
            nalu_start = i;
        } else {
            i += 1;
        }
    }

    // Last NALU.
    if nalu_start < data.len() {
        nalus.push(&data[nalu_start..]);
    }

    nalus
}

/// Check if there is an Annex B start code at position `pos`.
/// Returns `Some(length)` of the start code (3 or 4), or `None`.
fn start_code_len(data: &[u8], pos: usize) -> Option<usize> {
    if pos + 4 <= data.len() && data[pos] == 0 && data[pos + 1] == 0 && data[pos + 2] == 0 && data[pos + 3] == 1 {
        return Some(4);
    }
    if pos + 3 <= data.len() && data[pos] == 0 && data[pos + 1] == 0 && data[pos + 2] == 1 {
        return Some(3);
    }
    None
}

// ---------------------------------------------------------------------------
// Audio tags — AAC
// ---------------------------------------------------------------------------

/// AAC packet types.
const AAC_PACKET_SEQUENCE_HEADER: u8 = 0;
const AAC_PACKET_RAW: u8 = 1;

/// Sound format code for AAC.
const SOUND_FORMAT_AAC: u8 = 10;

/// Build the FLV audio tag header byte.
///
/// For AAC, the rate/size/type fields are fixed regardless of actual sample rate:
/// - rate = 3 (44 kHz marker)
/// - size = 1 (16-bit)
/// - type = `channels` (0 = mono, 1 = stereo)
fn aac_audio_header_byte(stereo: bool) -> u8 {
    let format = SOUND_FORMAT_AAC << 4;
    let rate = 3 << 2; // 44 kHz (convention for AAC in FLV)
    let size = 1 << 1; // 16-bit
    let ch_type = if stereo { 1 } else { 0 };
    format | rate | size | ch_type
}

/// Build an AAC sequence header tag body.
///
/// `audio_specific_config` is the raw AudioSpecificConfig from the MPEG-4
/// descriptor (typically 2 bytes for LC-AAC: `[profile:5][freq_idx:4][channels:4][...remaining]`).
pub fn build_aac_sequence_header(audio_specific_config: &[u8], stereo: bool) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(2 + audio_specific_config.len());
    buf.put_u8(aac_audio_header_byte(stereo));
    buf.put_u8(AAC_PACKET_SEQUENCE_HEADER);
    buf.put_slice(audio_specific_config);
    buf.to_vec()
}

/// Build an AAC raw frame tag body.
///
/// `raw_aac` is one AAC frame (without ADTS header).
pub fn build_aac_raw_tag(raw_aac: &[u8], stereo: bool) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(2 + raw_aac.len());
    buf.put_u8(aac_audio_header_byte(stereo));
    buf.put_u8(AAC_PACKET_RAW);
    buf.put_slice(raw_aac);
    buf.to_vec()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc_sequence_header_structure() {
        // Minimal SPS and PPS.
        let sps = vec![0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8];
        let pps = vec![0x68, 0xCE, 0x38, 0x80];

        let tag = build_avc_sequence_header(&sps, &pps);

        // Byte 0: key frame (1 << 4) | AVC (7) = 0x17
        assert_eq!(tag[0], 0x17);
        // Byte 1: sequence header
        assert_eq!(tag[1], 0x00);
        // Bytes 2-4: composition time = 0
        assert_eq!(&tag[2..5], &[0, 0, 0]);
        // Byte 5: configurationVersion = 1
        assert_eq!(tag[5], 1);
        // Byte 6: AVCProfileIndication = sps[1] = 0x42
        assert_eq!(tag[6], 0x42);
    }

    #[test]
    fn annex_b_split() {
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // SPS start code + data
            0x00, 0x00, 0x01, 0x68, 0xCE,        // PPS with 3-byte start code
        ];
        let nalus = split_annex_b_nalus(&data);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0], &[0x67, 0x42]);
        assert_eq!(nalus[1], &[0x68, 0xCE]);
    }

    #[test]
    fn aac_header_byte_stereo() {
        let b = aac_audio_header_byte(true);
        // AAC(10) << 4 | rate(3) << 2 | size(1) << 1 | stereo(1) = 0xAF
        assert_eq!(b, 0xAF);
    }

    #[test]
    fn aac_header_byte_mono() {
        let b = aac_audio_header_byte(false);
        // AAC(10) << 4 | rate(3) << 2 | size(1) << 1 | mono(0) = 0xAE
        assert_eq!(b, 0xAE);
    }
}
