// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

// Phase 1 step 4: PCM audio packetizer/depacketizer for SMPTE ST 2110-30.
// AES3 (ST 2110-31) reuses the same wire framing — only the payload bytes
// are interpreted differently by upstream consumers — and is implemented as
// a thin wrapper that lives next to this module.
#![allow(dead_code)]

//! SMPTE ST 2110-30 PCM audio packetizer and depacketizer.
//!
//! ST 2110-30 carries linear PCM audio over RTP per RFC 3190. Two profiles
//! are in common use:
//!
//! - **L16**: 16-bit big-endian signed integer samples per channel.
//! - **L24**: 24-bit big-endian signed integer samples per channel
//!   (3 bytes per sample, no padding).
//!
//! Channels are interleaved within each RTP packet at the per-sample (frame)
//! granularity. The number of samples per packet is determined by the
//! configured packet time and sample rate:
//!
//! ```text
//! samples_per_packet = packet_time_us × sample_rate / 1_000_000
//! ```
//!
//! ST 2110-30 PM (Standard) profile uses `packet_time_us = 1000` (1 ms),
//! which gives 48 samples/channel at 48 kHz. AM (high frame rate) profile
//! uses `packet_time_us = 125` (0.125 ms / 6 samples per channel at 48 kHz).
//!
//! ## Pure Rust
//!
//! No new dependencies. RTP header is hand-written (12 bytes). Sample
//! interleaving is done at the byte level so we don't have to special-case
//! the bit layout.

use bytes::{Bytes, BytesMut};

/// Linear PCM sample format for ST 2110-30.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmFormat {
    /// 16-bit big-endian signed PCM (L16, RFC 3551 §4.5.10).
    L16,
    /// 24-bit big-endian signed PCM (L24, RFC 3190 §4).
    L24,
}

impl PcmFormat {
    /// Bytes per sample per channel.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PcmFormat::L16 => 2,
            PcmFormat::L24 => 3,
        }
    }

    /// SDP `rtpmap` encoding name.
    pub fn rtpmap_encoding(self) -> &'static str {
        match self {
            PcmFormat::L16 => "L16",
            PcmFormat::L24 => "L24",
        }
    }
}

/// Errors returned by the depacketizer.
#[derive(Debug, PartialEq, Eq)]
pub enum PcmDepacketizeError {
    /// Buffer is shorter than the minimal RTP header.
    TooShort,
    /// RTP version field is not 2.
    WrongVersion,
    /// Payload type does not match the expected value.
    WrongPayloadType { expected: u8, got: u8 },
    /// Payload length is not a multiple of (channels × bytes_per_sample),
    /// so it does not contain a whole number of audio frames.
    NotFrameAligned { payload_len: usize, frame_size: usize },
}

impl std::fmt::Display for PcmDepacketizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PcmDepacketizeError::TooShort => f.write_str("RTP packet too short"),
            PcmDepacketizeError::WrongVersion => f.write_str("RTP version is not 2"),
            PcmDepacketizeError::WrongPayloadType { expected, got } => {
                write!(f, "RTP payload type mismatch: expected {expected}, got {got}")
            }
            PcmDepacketizeError::NotFrameAligned { payload_len, frame_size } => write!(
                f,
                "PCM payload length {payload_len} is not a multiple of frame size {frame_size}"
            ),
        }
    }
}

impl std::error::Error for PcmDepacketizeError {}

/// A depacketized PCM RTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmDepacketizedPacket {
    /// RTP sequence number.
    pub seq: u16,
    /// RTP timestamp (audio sample-rate ticks).
    pub timestamp: u32,
    /// SSRC of the sender.
    pub ssrc: u32,
    /// Marker bit. ST 2110-30 senders typically leave this clear; receivers
    /// must be tolerant of either value.
    pub marker: bool,
    /// PCM payload bytes (interleaved samples in the configured format).
    pub payload: Bytes,
}

impl PcmDepacketizedPacket {
    /// Number of complete audio frames (samples per channel) carried by
    /// this packet, given a known frame size in bytes.
    pub fn frame_count(&self, frame_size: usize) -> usize {
        debug_assert!(frame_size > 0);
        self.payload.len() / frame_size
    }
}

/// Stateful packetizer that converts a stream of interleaved PCM samples
/// into one or more RTP packets, advancing the RTP sequence number and
/// timestamp.
///
/// The packetizer is push-style: feed it raw sample bytes via [`packetize`]
/// and collect the emitted packets. Bytes that do not fill a complete packet
/// are returned as a leftover slice and the caller is expected to prepend
/// them to the next chunk.
///
/// [`packetize`]: PcmPacketizer::packetize
pub struct PcmPacketizer {
    format: PcmFormat,
    sample_rate: u32,
    channels: u8,
    packet_time_us: u32,
    payload_type: u8,
    ssrc: u32,
    seq: u16,
    timestamp: u32,
}

impl PcmPacketizer {
    /// Build a new packetizer.
    ///
    /// `initial_seq` and `initial_timestamp` are typically randomized at
    /// session start per RFC 3550 §5.1.
    pub fn new(
        format: PcmFormat,
        sample_rate: u32,
        channels: u8,
        packet_time_us: u32,
        payload_type: u8,
        ssrc: u32,
        initial_seq: u16,
        initial_timestamp: u32,
    ) -> Self {
        assert!(channels > 0, "channels must be > 0");
        assert!(payload_type < 128, "payload_type must be < 128");
        Self {
            format,
            sample_rate,
            channels,
            packet_time_us,
            payload_type,
            ssrc,
            seq: initial_seq,
            timestamp: initial_timestamp,
        }
    }

    /// Number of audio samples per channel in each output packet.
    pub fn samples_per_packet(&self) -> u32 {
        ((self.packet_time_us as u64 * self.sample_rate as u64) / 1_000_000) as u32
    }

    /// Bytes per audio frame (one sample across all channels).
    pub fn frame_size(&self) -> usize {
        self.channels as usize * self.format.bytes_per_sample()
    }

    /// Bytes of PCM payload per output packet.
    pub fn payload_bytes_per_packet(&self) -> usize {
        self.samples_per_packet() as usize * self.frame_size()
    }

    /// Current RTP sequence number (next packet will use this value).
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// Current RTP timestamp (next packet will use this value).
    pub fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    /// Packetize as many full packets as possible from `samples` (a buffer
    /// of interleaved PCM samples in the configured format), pushing each
    /// emitted packet onto `out`. Returns the leftover slice that did not
    /// fill a complete packet — the caller should prepend it to the next
    /// call's input.
    pub fn packetize<'a>(
        &mut self,
        samples: &'a [u8],
        out: &mut Vec<Bytes>,
    ) -> &'a [u8] {
        let bytes_per_packet = self.payload_bytes_per_packet();
        if bytes_per_packet == 0 {
            return samples;
        }
        let mut offset = 0;
        while offset + bytes_per_packet <= samples.len() {
            let payload = &samples[offset..offset + bytes_per_packet];
            out.push(self.build_packet(payload));
            offset += bytes_per_packet;
        }
        &samples[offset..]
    }

    fn build_packet(&mut self, payload: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(12 + payload.len());
        // RTP header (12 bytes, no extensions, no CSRCs)
        buf.extend_from_slice(&[
            0x80, // V=2, P=0, X=0, CC=0
            self.payload_type & 0x7F, // M=0
        ]);
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
        buf.extend_from_slice(payload);

        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self
            .timestamp
            .wrapping_add(self.samples_per_packet());
        buf.freeze()
    }
}

/// Stateless depacketizer for PCM RTP packets.
///
/// Holds the configured payload type, format, and channel count so it can
/// reject mismatched packets and validate frame alignment. Sequence-number
/// gap detection and PLC are responsibilities of the input task, not this
/// depacketizer.
pub struct PcmDepacketizer {
    format: PcmFormat,
    expected_pt: u8,
    expected_channels: u8,
}

impl PcmDepacketizer {
    pub fn new(format: PcmFormat, expected_pt: u8, expected_channels: u8) -> Self {
        assert!(expected_pt < 128);
        assert!(expected_channels > 0);
        Self {
            format,
            expected_pt,
            expected_channels,
        }
    }

    /// Bytes per audio frame this depacketizer expects.
    pub fn frame_size(&self) -> usize {
        self.expected_channels as usize * self.format.bytes_per_sample()
    }

    /// Parse a single RTP packet, validating headers and payload alignment.
    pub fn depacketize(
        &self,
        packet: &[u8],
    ) -> Result<PcmDepacketizedPacket, PcmDepacketizeError> {
        if packet.len() < 12 {
            return Err(PcmDepacketizeError::TooShort);
        }
        if (packet[0] >> 6) != 2 {
            return Err(PcmDepacketizeError::WrongVersion);
        }
        let pt = packet[1] & 0x7F;
        if pt != self.expected_pt {
            return Err(PcmDepacketizeError::WrongPayloadType {
                expected: self.expected_pt,
                got: pt,
            });
        }
        let cc = (packet[0] & 0x0F) as usize;
        let header_len = 12 + cc * 4;
        if packet.len() < header_len {
            return Err(PcmDepacketizeError::TooShort);
        }
        let marker = (packet[1] & 0x80) != 0;
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        let ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());
        let payload_bytes = &packet[header_len..];

        let frame_size = self.frame_size();
        if payload_bytes.len() % frame_size != 0 {
            return Err(PcmDepacketizeError::NotFrameAligned {
                payload_len: payload_bytes.len(),
                frame_size,
            });
        }

        Ok(PcmDepacketizedPacket {
            seq,
            timestamp,
            ssrc,
            marker,
            payload: Bytes::copy_from_slice(payload_bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic ramp of `n` samples per channel for `channels`
    /// channels in the given `format`. Sample 0 of channel 0 starts at 0;
    /// each subsequent sample increments by 1 (per-channel within a frame
    /// the channel index is used as an offset). Useful for round-trip checks.
    fn build_ramp(format: PcmFormat, channels: u8, samples_per_channel: u32) -> Vec<u8> {
        let bps = format.bytes_per_sample();
        let mut buf = Vec::with_capacity(samples_per_channel as usize * channels as usize * bps);
        for s in 0..samples_per_channel {
            for ch in 0..channels {
                let value = (s as i32) * 100 + ch as i32;
                match format {
                    PcmFormat::L16 => {
                        let v16 = value as i16;
                        buf.extend_from_slice(&v16.to_be_bytes());
                    }
                    PcmFormat::L24 => {
                        // 24-bit big-endian signed: take the low 24 bits of i32
                        // and write the three high-order bytes (network order).
                        let mut v24 = value & 0xFFFFFF;
                        if value < 0 {
                            v24 |= 0x800000;
                        }
                        buf.push(((v24 >> 16) & 0xFF) as u8);
                        buf.push(((v24 >> 8) & 0xFF) as u8);
                        buf.push((v24 & 0xFF) as u8);
                    }
                }
            }
        }
        buf
    }

    #[test]
    fn test_pcm_format_bytes_per_sample() {
        assert_eq!(PcmFormat::L16.bytes_per_sample(), 2);
        assert_eq!(PcmFormat::L24.bytes_per_sample(), 3);
    }

    #[test]
    fn test_samples_per_packet_pm_profile() {
        let p = PcmPacketizer::new(PcmFormat::L24, 48_000, 2, 1_000, 97, 0xdeadbeef, 0, 0);
        assert_eq!(p.samples_per_packet(), 48);
        assert_eq!(p.payload_bytes_per_packet(), 48 * 2 * 3);
    }

    #[test]
    fn test_samples_per_packet_am_profile() {
        let p = PcmPacketizer::new(PcmFormat::L24, 48_000, 8, 125, 97, 0, 0, 0);
        // 125us × 48000 / 1_000_000 = 6 samples/channel
        assert_eq!(p.samples_per_packet(), 6);
        assert_eq!(p.payload_bytes_per_packet(), 6 * 8 * 3);
    }

    #[test]
    fn test_samples_per_packet_96khz() {
        let p = PcmPacketizer::new(PcmFormat::L24, 96_000, 2, 1_000, 97, 0, 0, 0);
        assert_eq!(p.samples_per_packet(), 96);
    }

    #[test]
    fn test_packetize_emits_complete_packets_l24_stereo_1ms_48k() {
        // 3 ms of stereo L24 → 3 packets of 48 frames each.
        let mut p = PcmPacketizer::new(PcmFormat::L24, 48_000, 2, 1_000, 97, 0xcafebabe, 100, 1_000);
        let samples = build_ramp(PcmFormat::L24, 2, 48 * 3);
        let mut packets = Vec::new();
        let leftover = p.packetize(&samples, &mut packets);
        assert_eq!(packets.len(), 3);
        assert!(leftover.is_empty());
        // Sequence numbers advance.
        let seqs: Vec<u16> = packets.iter().map(|pkt| u16::from_be_bytes([pkt[2], pkt[3]])).collect();
        assert_eq!(seqs, vec![100, 101, 102]);
        // Timestamps advance by samples_per_packet (48).
        let tses: Vec<u32> = packets.iter().map(|pkt| u32::from_be_bytes(pkt[4..8].try_into().unwrap())).collect();
        assert_eq!(tses, vec![1_000, 1_048, 1_096]);
        // PT in low 7 bits of byte 1.
        for pkt in &packets {
            assert_eq!(pkt[1] & 0x7F, 97);
            assert_eq!(pkt[0] >> 6, 2); // RTP version
            // SSRC matches.
            assert_eq!(u32::from_be_bytes(pkt[8..12].try_into().unwrap()), 0xcafebabe);
            // Payload length matches one packet's worth.
            assert_eq!(pkt.len() - 12, 48 * 2 * 3);
        }
    }

    #[test]
    fn test_packetize_partial_returns_leftover() {
        let mut p = PcmPacketizer::new(PcmFormat::L24, 48_000, 2, 1_000, 97, 0, 0, 0);
        // 50 frames = 1 full packet (48 frames) + 2 leftover frames.
        let samples = build_ramp(PcmFormat::L24, 2, 50);
        let mut packets = Vec::new();
        let leftover = p.packetize(&samples, &mut packets);
        assert_eq!(packets.len(), 1);
        assert_eq!(leftover.len(), 2 * 2 * 3);
    }

    #[test]
    fn test_packetize_empty_input_emits_nothing() {
        let mut p = PcmPacketizer::new(PcmFormat::L24, 48_000, 2, 1_000, 97, 0, 0, 0);
        let mut packets = Vec::new();
        let leftover = p.packetize(&[], &mut packets);
        assert!(packets.is_empty());
        assert!(leftover.is_empty());
    }

    #[test]
    fn test_packetize_l16_mono() {
        let mut p = PcmPacketizer::new(PcmFormat::L16, 48_000, 1, 1_000, 97, 0, 0, 0);
        let samples = build_ramp(PcmFormat::L16, 1, 48);
        let mut packets = Vec::new();
        let leftover = p.packetize(&samples, &mut packets);
        assert!(leftover.is_empty());
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len() - 12, 48 * 2);
    }

    #[test]
    fn test_packetize_seq_wraps_at_u16_max() {
        let mut p = PcmPacketizer::new(
            PcmFormat::L24, 48_000, 2, 1_000, 97, 0, u16::MAX, 0,
        );
        let samples = build_ramp(PcmFormat::L24, 2, 48 * 2);
        let mut packets = Vec::new();
        p.packetize(&samples, &mut packets);
        let seq0 = u16::from_be_bytes([packets[0][2], packets[0][3]]);
        let seq1 = u16::from_be_bytes([packets[1][2], packets[1][3]]);
        assert_eq!(seq0, u16::MAX);
        assert_eq!(seq1, 0);
    }

    #[test]
    fn test_packetize_timestamp_wraps_at_u32_max() {
        let mut p = PcmPacketizer::new(
            PcmFormat::L24, 48_000, 2, 1_000, 97, 0, 0, u32::MAX - 24,
        );
        let samples = build_ramp(PcmFormat::L24, 2, 48 * 2);
        let mut packets = Vec::new();
        p.packetize(&samples, &mut packets);
        let ts0 = u32::from_be_bytes(packets[0][4..8].try_into().unwrap());
        let ts1 = u32::from_be_bytes(packets[1][4..8].try_into().unwrap());
        assert_eq!(ts0, u32::MAX - 24);
        // ts0 + 48 wraps past u32::MAX
        assert_eq!(ts1, ts0.wrapping_add(48));
    }

    #[test]
    fn test_round_trip_l24_stereo_48k_1ms() {
        let mut packetizer =
            PcmPacketizer::new(PcmFormat::L24, 48_000, 2, 1_000, 97, 0xdeadbeef, 0, 0);
        let depacketizer = PcmDepacketizer::new(PcmFormat::L24, 97, 2);

        let original = build_ramp(PcmFormat::L24, 2, 48 * 5);
        let mut packets = Vec::new();
        let leftover = packetizer.packetize(&original, &mut packets);
        assert!(leftover.is_empty());
        assert_eq!(packets.len(), 5);

        let mut reconstructed = Vec::with_capacity(original.len());
        for pkt in &packets {
            let dp = depacketizer.depacketize(pkt).expect("depacketize");
            assert_eq!(dp.ssrc, 0xdeadbeef);
            reconstructed.extend_from_slice(&dp.payload);
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn test_round_trip_l16_mono_48k_1ms() {
        let mut packetizer =
            PcmPacketizer::new(PcmFormat::L16, 48_000, 1, 1_000, 97, 0x12345678, 1000, 5000);
        let depacketizer = PcmDepacketizer::new(PcmFormat::L16, 97, 1);

        let original = build_ramp(PcmFormat::L16, 1, 48 * 4);
        let mut packets = Vec::new();
        packetizer.packetize(&original, &mut packets);
        let mut reconstructed = Vec::new();
        for pkt in &packets {
            reconstructed.extend_from_slice(&depacketizer.depacketize(pkt).unwrap().payload);
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn test_round_trip_l24_8ch_125us_96k() {
        let mut packetizer =
            PcmPacketizer::new(PcmFormat::L24, 96_000, 8, 125, 97, 0, 0, 0);
        let depacketizer = PcmDepacketizer::new(PcmFormat::L24, 97, 8);
        // 125 µs × 96 kHz = 12 samples/channel per packet. Send 4 packets'
        // worth of samples (48 samples/channel total) to exercise multiple
        // packets.
        let original = build_ramp(PcmFormat::L24, 8, 12 * 4);
        let mut packets = Vec::new();
        let leftover = packetizer.packetize(&original, &mut packets);
        assert!(leftover.is_empty());
        assert_eq!(packets.len(), 4);
        let mut reconstructed = Vec::new();
        for pkt in &packets {
            reconstructed.extend_from_slice(&depacketizer.depacketize(pkt).unwrap().payload);
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn test_depacketize_rejects_short() {
        let d = PcmDepacketizer::new(PcmFormat::L24, 97, 2);
        assert_eq!(d.depacketize(&[]), Err(PcmDepacketizeError::TooShort));
        assert_eq!(d.depacketize(&[0u8; 11]), Err(PcmDepacketizeError::TooShort));
    }

    #[test]
    fn test_depacketize_rejects_wrong_version() {
        let d = PcmDepacketizer::new(PcmFormat::L24, 97, 2);
        let mut buf = vec![0x40, 0x61]; // V=1
        buf.extend_from_slice(&[0u8; 16]);
        assert_eq!(d.depacketize(&buf), Err(PcmDepacketizeError::WrongVersion));
    }

    #[test]
    fn test_depacketize_rejects_wrong_pt() {
        let d = PcmDepacketizer::new(PcmFormat::L24, 97, 2);
        let mut buf = vec![0x80, 96]; // PT=96, expected 97
        buf.extend_from_slice(&[0u8; 10]);
        assert_eq!(
            d.depacketize(&buf),
            Err(PcmDepacketizeError::WrongPayloadType { expected: 97, got: 96 })
        );
    }

    #[test]
    fn test_depacketize_rejects_misaligned_payload() {
        let d = PcmDepacketizer::new(PcmFormat::L24, 97, 2);
        // 12-byte header + 5 bytes payload (frame size is 6) → not aligned.
        let mut buf = vec![0x80, 97, 0, 1, 0, 0, 0, 0, 0xCA, 0xFE, 0xBA, 0xBE];
        buf.extend_from_slice(&[0u8; 5]);
        assert!(matches!(
            d.depacketize(&buf),
            Err(PcmDepacketizeError::NotFrameAligned { payload_len: 5, frame_size: 6 })
        ));
    }

    #[test]
    fn test_depacketize_handles_csrcs() {
        // CC=2 → 4 + 4 = 8 bytes of CSRCs after the fixed 12-byte header.
        let d = PcmDepacketizer::new(PcmFormat::L16, 97, 1);
        let mut buf = vec![0x82, 97]; // V=2, CC=2
        buf.extend_from_slice(&7u16.to_be_bytes()); // seq
        buf.extend_from_slice(&100u32.to_be_bytes()); // ts
        buf.extend_from_slice(&0xdeadbeefu32.to_be_bytes()); // ssrc
        buf.extend_from_slice(&[0; 8]); // CSRC[0..2]
        buf.extend_from_slice(&[0x12, 0x34]); // 1 sample of L16 mono
        let dp = d.depacketize(&buf).unwrap();
        assert_eq!(dp.seq, 7);
        assert_eq!(dp.timestamp, 100);
        assert_eq!(dp.ssrc, 0xdeadbeef);
        assert_eq!(dp.payload.as_ref(), &[0x12, 0x34]);
    }

    #[test]
    fn test_marker_bit_round_trips_for_arbitrary_packet() {
        // The packetizer never sets M; this test only checks that the
        // depacketizer correctly reads the bit when present.
        let d = PcmDepacketizer::new(PcmFormat::L16, 97, 1);
        let mut buf = vec![0x80, 97 | 0x80]; // M=1
        buf.extend_from_slice(&[0u8; 10]);
        buf.extend_from_slice(&[0x00, 0x01]);
        let dp = d.depacketize(&buf).unwrap();
        assert!(dp.marker);
    }

    #[test]
    fn test_frame_count_helper() {
        let d = PcmDepacketizer::new(PcmFormat::L24, 97, 2);
        let dp = PcmDepacketizedPacket {
            seq: 0,
            timestamp: 0,
            ssrc: 0,
            marker: false,
            payload: Bytes::from(vec![0u8; 48 * 6]),
        };
        assert_eq!(dp.frame_count(d.frame_size()), 48);
    }
}
