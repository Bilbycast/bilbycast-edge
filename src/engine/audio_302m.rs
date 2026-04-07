// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE 302M LPCM audio packetizer / depacketizer.
//!
//! SMPTE 302M-2007 carries uncompressed AES3 LPCM audio inside MPEG-2
//! transport stream private PES packets, identified in the PMT by:
//!
//! - `stream_type = 0x06` (private PES)
//! - A registration descriptor with `format_identifier = "BSSD"` (`0x42535344`)
//!
//! It's the broadcast industry standard for carrying lossless audio over
//! MPEG-TS contribution links: ffmpeg supports it natively (`-c:a s302m`),
//! `srt-live-transmit` passes it untouched, and most professional hardware
//! decoders accept it.
//!
//! ## Constraints
//!
//! - Sample rate: **48 kHz only** (per the spec)
//! - Bit depths: 16, 20, 24
//! - Channels: 2, 4, 6, 8 (must be even)
//!
//! Bilbycast pairs this module with [`crate::engine::audio_transcode`] so
//! arbitrary input sample rates / channel counts are upsampled to 48 kHz
//! and an even channel count before 302M packetization.
//!
//! ## PES payload layout
//!
//! ```text
//!  ┌──────────────────────────────────────────┐
//!  │  audio_packet_size  (16 bits)            │  4-byte header
//!  ├──────────────────────────────────────────┤  (AES3_HEADER_LEN)
//!  │  number_channels    ( 2 bits)            │
//!  │  channel_id         ( 8 bits) = 0x00     │
//!  │  bits_per_sample    ( 2 bits)            │
//!  │  alignment_bits     ( 4 bits) = 0        │
//!  ├──────────────────────────────────────────┤
//!  │  audio data:                             │
//!  │   pairs of channels, bit-packed and      │
//!  │   bit-reversed per spec                  │
//!  └──────────────────────────────────────────┘
//! ```
//!
//! Per stereo pair, the encoded byte count is:
//!
//! | bit_depth | bytes/pair | bits/pair |
//! |-----------|------------|-----------|
//! |     16    |     5      |     40    |
//! |     20    |     6      |     48    |
//! |     24    |     7      |     56    |
//!
//! ## Bit reversal
//!
//! Each output byte is bit-reversed (LSB↔MSB) to follow the AES3
//! transmission order. We pre-compute a 256-entry `BIT_REVERSE` LUT.
//!
//! ## Reference
//!
//! Bit packing follows ffmpeg's `libavcodec/s302menc.c` and `s302mdec.c`
//! verbatim — both are GPL but the bit layout is normative per SMPTE
//! 302M-2007. We re-implement in pure Rust without linking against
//! ffmpeg.

#![allow(dead_code)]

use bytes::{Bytes, BytesMut};

/// Length of the SMPTE 302M PES payload header in bytes.
pub const AES3_HEADER_LEN: usize = 4;

/// MPEG-TS PMT `stream_type` value used for SMPTE 302M private PES.
pub const STREAM_TYPE_S302M: u8 = 0x06;

/// Registration descriptor `format_identifier` for SMPTE 302M (`"BSSD"`).
pub const BSSD_FORMAT_ID: [u8; 4] = *b"BSSD";

/// Pre-computed bit-reversal lookup table (LSB ↔ MSB within each byte).
const BIT_REVERSE: [u8; 256] = build_bit_reverse_lut();

const fn build_bit_reverse_lut() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut x = i as u8;
        let mut r = 0u8;
        let mut k = 0;
        while k < 8 {
            r = (r << 1) | (x & 1);
            x >>= 1;
            k += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
}

#[inline(always)]
fn br(x: u8) -> u8 {
    BIT_REVERSE[x as usize]
}

/// SMPTE 302M packetizer.
///
/// Stateful only in the `framing_index` (the AES3 192-frame channel-status
/// block counter, used to set the validity/user/channel-status/frame bits
/// in `vucf`). Channels and bit depth are fixed at construction.
#[derive(Debug)]
pub struct S302mPacketizer {
    channels: u8,
    bit_depth: u8,
    framing_index: u32,
}

impl S302mPacketizer {
    /// Construct. Returns `Err` for unsupported channel count or bit depth.
    pub fn new(channels: u8, bit_depth: u8) -> Result<Self, String> {
        if !matches!(channels, 2 | 4 | 6 | 8) {
            return Err(format!(
                "S302mPacketizer: channels must be 2, 4, 6, or 8 (SMPTE 302M-2007), got {channels}"
            ));
        }
        if !matches!(bit_depth, 16 | 20 | 24) {
            return Err(format!(
                "S302mPacketizer: bit_depth must be 16, 20, or 24, got {bit_depth}"
            ));
        }
        Ok(Self {
            channels,
            bit_depth,
            framing_index: 0,
        })
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// Number of payload bytes that one stereo pair occupies.
    fn bytes_per_pair(&self) -> usize {
        match self.bit_depth {
            16 => 5,
            20 => 6,
            24 => 7,
            _ => unreachable!(),
        }
    }

    /// Convert a planar `[chan][frame]` block of f32 PCM samples in
    /// `[-1.0, 1.0]` into one SMPTE 302M PES payload (header + bit-packed
    /// samples). The result is a `Bytes` ready to feed to a TS muxer's
    /// private PES path.
    ///
    /// `samples_per_channel = planar[0].len()` (all channels must agree).
    pub fn packetize_f32(&mut self, planar: &[Vec<f32>]) -> Result<Bytes, String> {
        if planar.len() != self.channels as usize {
            return Err(format!(
                "packetize_f32: planar has {} channels, expected {}",
                planar.len(),
                self.channels
            ));
        }
        let n_frames = planar[0].len();
        for ch in planar.iter() {
            if ch.len() != n_frames {
                return Err("packetize_f32: channels have differing frame counts".into());
            }
        }
        // Convert f32 → i32 sample slots in the layout the inner encoder
        // expects (left-shifted into the high bits of the slot).
        let mut interleaved: Vec<i32> = Vec::with_capacity(n_frames * self.channels as usize);
        let shift = match self.bit_depth {
            16 => 16, // 16-bit values used as i16 by the 16-bit encode path
            20 => 12,
            24 => 8,
            _ => unreachable!(),
        };
        let scale = match self.bit_depth {
            16 => i16::MAX as f32 + 1.0,
            20 => (1 << 19) as f32,
            24 => (1 << 23) as f32,
            _ => unreachable!(),
        };
        for frame in 0..n_frames {
            for ch in 0..self.channels as usize {
                let v = planar[ch][frame].clamp(-1.0, 1.0);
                let q = (v * scale).round() as i32;
                // Clamp to the natural sample range to avoid overflow on
                // the +1.0 boundary.
                let max = match self.bit_depth {
                    16 => i16::MAX as i32,
                    20 => (1 << 19) - 1,
                    24 => (1 << 23) - 1,
                    _ => unreachable!(),
                };
                let min = match self.bit_depth {
                    16 => i16::MIN as i32,
                    20 => -(1 << 19),
                    24 => -(1 << 23),
                    _ => unreachable!(),
                };
                let q = q.clamp(min, max);
                let slot = (q as i32) << shift;
                interleaved.push(slot);
            }
        }
        Ok(self.encode_to_pes_payload(&interleaved, n_frames))
    }

    /// Lower-level entry point for callers that already have i32 sample
    /// slots in the ffmpeg-style left-shifted layout. `interleaved` length
    /// must equal `n_frames * channels`.
    pub fn packetize_i32_slots(&mut self, interleaved: &[i32], n_frames: usize) -> Bytes {
        self.encode_to_pes_payload(interleaved, n_frames)
    }

    fn encode_to_pes_payload(&mut self, interleaved: &[i32], n_frames: usize) -> Bytes {
        debug_assert_eq!(interleaved.len(), n_frames * self.channels as usize);
        let pairs_per_frame = self.channels as usize / 2;
        let bytes_per_pair = self.bytes_per_pair();
        let audio_bytes = n_frames * pairs_per_frame * bytes_per_pair;
        let total = AES3_HEADER_LEN + audio_bytes;
        let mut out = BytesMut::with_capacity(total);

        // ── 4-byte header ──
        // 16 bits audio_packet_size = total - AES3_HEADER_LEN
        let aps = audio_bytes as u16;
        out.extend_from_slice(&aps.to_be_bytes());
        // 2 bits number_channels: 0=2, 1=4, 2=6, 3=8
        let nc_field = ((self.channels - 2) >> 1) as u8 & 0x03;
        // 8 bits channel_identification: 0
        // 2 bits bits_per_sample: 0=16, 1=20, 2=24
        let bps_field = ((self.bit_depth - 16) / 4) as u8 & 0x03;
        // 4 bits alignment_bits: 0
        //
        // Layout across the next two header bytes (big-endian bitstream):
        //   byte2 = nc(2) | id(6 high)
        //   byte3 = id(2 low) | bps(2) | align(4)
        let byte2 = (nc_field << 6) | 0x00;
        let byte3 = (0x00 << 6) | (bps_field << 4) | 0x00;
        out.extend_from_slice(&[byte2, byte3]);

        // ── Audio body ──
        let mut buf = vec![0u8; audio_bytes];
        let mut o_off = 0;
        for frame in 0..n_frames {
            let frame_off = frame * self.channels as usize;
            for pair in 0..pairs_per_frame {
                let s0 = interleaved[frame_off + pair * 2];
                let s1 = interleaved[frame_off + pair * 2 + 1];
                let vucf = self.vucf_for_current();
                match self.bit_depth {
                    16 => {
                        // samples are i16 logically — encoded as if i16 but stored
                        // in i32 slots without left shift (shift was 16). The
                        // ffmpeg encoder uses int16_t* directly, so the shift
                        // we applied is 16 to recover the original i16 value
                        // when masking with 0xFF / 0xFF00.
                        let s0 = (s0 >> 16) & 0xFFFF; // recover the i16 value
                        let s1 = (s1 >> 16) & 0xFFFF;
                        buf[o_off]     = br((s0 & 0xFF) as u8);
                        buf[o_off + 1] = br(((s0 & 0xFF00) >> 8) as u8);
                        buf[o_off + 2] = br(((s1 & 0x000F) << 4) as u8) | vucf;
                        buf[o_off + 3] = br(((s1 & 0x0FF0) >> 4) as u8);
                        buf[o_off + 4] = br(((s1 & 0xF000) >> 12) as u8);
                        o_off += 5;
                    }
                    20 => {
                        let s0u = s0 as u32;
                        let s1u = s1 as u32;
                        buf[o_off]     = br(((s0u & 0x000F_F000) >> 12) as u8);
                        buf[o_off + 1] = br(((s0u & 0x0FF0_0000) >> 20) as u8);
                        buf[o_off + 2] = br((((s0u & 0xF000_0000) >> 28) as u8) | vucf);
                        buf[o_off + 3] = br(((s1u & 0x000F_F000) >> 12) as u8);
                        buf[o_off + 4] = br(((s1u & 0x0FF0_0000) >> 20) as u8);
                        buf[o_off + 5] = br(((s1u & 0xF000_0000) >> 28) as u8);
                        o_off += 6;
                    }
                    24 => {
                        let s0u = s0 as u32;
                        let s1u = s1 as u32;
                        buf[o_off]     = br(((s0u & 0x0000_FF00) >> 8) as u8);
                        buf[o_off + 1] = br(((s0u & 0x00FF_0000) >> 16) as u8);
                        buf[o_off + 2] = br(((s0u & 0xFF00_0000) >> 24) as u8);
                        buf[o_off + 3] = br(((s1u & 0x0000_0F00) >> 4) as u8) | vucf;
                        buf[o_off + 4] = br(((s1u & 0x000F_F000) >> 12) as u8);
                        buf[o_off + 5] = br(((s1u & 0x0FF0_0000) >> 20) as u8);
                        buf[o_off + 6] = br(((s1u & 0xF000_0000) >> 28) as u8);
                        o_off += 7;
                    }
                    _ => unreachable!(),
                }
            }
            self.framing_index = (self.framing_index + 1) % 192;
        }
        out.extend_from_slice(&buf);
        out.freeze()
    }

    fn vucf_for_current(&self) -> u8 {
        // For 16-bit / 24-bit: 0x10 at framing_index 0 (block boundary), else 0
        // For 20-bit:           0x80 at framing_index 0, else 0
        if self.framing_index != 0 {
            return 0;
        }
        match self.bit_depth {
            20 => 0x80,
            _ => 0x10,
        }
    }
}

/// SMPTE 302M depacketizer.
#[derive(Debug, Default)]
pub struct S302mDepacketizer;

impl S302mDepacketizer {
    pub fn new() -> Self {
        Self
    }

    /// Parse one SMPTE 302M PES payload (header + audio body) into a planar
    /// f32 sample frame at 48 kHz. Channel count and bit depth are read from
    /// the header — the caller doesn't need to know them in advance.
    pub fn parse(&self, payload: &[u8]) -> Result<S302mFrame, String> {
        if payload.len() < AES3_HEADER_LEN {
            return Err("S302m payload too short for header".into());
        }
        let aps = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        if AES3_HEADER_LEN + aps > payload.len() {
            return Err(format!(
                "S302m payload truncated: header says {} body bytes but only {} available",
                aps,
                payload.len() - AES3_HEADER_LEN
            ));
        }
        let nc_field = (payload[2] >> 6) & 0x03;
        let bps_field = (payload[3] >> 4) & 0x03;
        let channels = ((nc_field as u8) + 1) * 2; // 0→2, 1→4, 2→6, 3→8
        let bit_depth = 16u8 + bps_field * 4; // 0→16, 1→20, 2→24
        if !matches!(channels, 2 | 4 | 6 | 8) {
            return Err(format!("S302m: bad channel field {nc_field}"));
        }
        if !matches!(bit_depth, 16 | 20 | 24) {
            return Err(format!("S302m: bad bit_depth field {bps_field}"));
        }
        let bytes_per_pair = match bit_depth {
            16 => 5,
            20 => 6,
            24 => 7,
            _ => unreachable!(),
        };
        let pairs_per_frame = channels as usize / 2;
        let bytes_per_frame = pairs_per_frame * bytes_per_pair;
        if aps % bytes_per_frame != 0 {
            return Err(format!(
                "S302m: audio_packet_size {aps} not aligned to frame size {bytes_per_frame}"
            ));
        }
        let n_frames = aps / bytes_per_frame;
        let audio = &payload[AES3_HEADER_LEN..AES3_HEADER_LEN + aps];

        let mut planar: Vec<Vec<f32>> = (0..channels).map(|_| Vec::with_capacity(n_frames)).collect();
        let mut off = 0;
        let scale_inv = match bit_depth {
            16 => 1.0_f32 / (i16::MAX as f32 + 1.0),
            20 => 1.0_f32 / ((1 << 19) as f32),
            24 => 1.0_f32 / ((1 << 23) as f32),
            _ => unreachable!(),
        };
        for _ in 0..n_frames {
            for pair in 0..pairs_per_frame {
                let (s0, s1) = match bit_depth {
                    16 => {
                        // Encoder:
                        //   o[0] = br( s0       & 0xFF        )  → low byte of s0
                        //   o[1] = br((s0 >> 8) & 0xFF        )  → high byte of s0
                        //   o[2] = br((s1 & 0x0F) << 4) | vucf   → low nibble of s1 in
                        //                                          o[2]'s bits 0..3,
                        //                                          vucf in bits 4..7
                        //   o[3] = br((s1 >> 4) & 0xFF        )  → mid byte of s1 (bits 4..11)
                        //   o[4] = br((s1 >> 12) & 0x0F)         → high nibble of s1
                        //                                          ends up in br()'s
                        //                                          bits 4..7
                        let b0 = br(audio[off]) as u16;
                        let b1 = br(audio[off + 1]) as u16;
                        let b2_stripped = br(audio[off + 2] & 0x0F) as u16; // strip vucf (bits 4..7)
                        let b3 = br(audio[off + 3]) as u16;
                        let b4 = br(audio[off + 4]) as u16;
                        let s0 = b0 | (b1 << 8);
                        // After stripping vucf and bit-reversing, the 4 sample
                        // bits sit at positions 4..7 of `b2_stripped`. Shift
                        // them down to 0..3 then OR into samples[1] bits 0..3.
                        let s1_low4 = (b2_stripped >> 4) & 0x0F;
                        // br(o[3]) recovers the 8 bits at samples[1] positions 4..11.
                        let s1_mid8 = b3;
                        // br(o[4]) recovers the 4 high bits at samples[1] positions
                        // 12..15. Encoder did `(s1 & 0xF000) >> 12` (4 bits at byte
                        // positions 0..3) and then br → bits at positions 4..7 of
                        // o[4]. After br again here, bits land back at 0..3.
                        let s1_high4 = b4 & 0x0F;
                        let s1 = s1_low4 | (s1_mid8 << 4) | (s1_high4 << 12);
                        (s0 as i16 as i32, s1 as i16 as i32)
                    }
                    20 => {
                        // Encoder:
                        //   o[0] = br((s0 & 0x000F_F000) >> 12)  → bits 12..19 of slot
                        //   o[1] = br((s0 & 0x0FF0_0000) >> 20)  → bits 20..27
                        //   o[2] = br(((s0 & 0xF000_0000) >> 28) | vucf)
                        //          → 4 sample bits at byte positions 0..3
                        //            ORed with vucf (0x80 = bit 7), the
                        //            *whole byte* then bit-reversed.
                        //   o[3..6] same for s1.
                        //
                        // br is its own inverse, so br(o[k]) recovers the
                        // pre-reverse byte: for k=2, that byte has the 4 sample
                        // bits at positions 0..3 and vucf at bit 7. Mask & 0x0F
                        // to drop vucf and keep just the sample bits.
                        let b0 = br(audio[off]) as u32;
                        let b1 = br(audio[off + 1]) as u32;
                        let b2 = br(audio[off + 2]) as u32;
                        let b3 = br(audio[off + 3]) as u32;
                        let b4 = br(audio[off + 4]) as u32;
                        let b5 = br(audio[off + 5]) as u32;
                        let s0_top = b2 & 0x0F;
                        let s1_top = b5 & 0x0F;
                        let s0 = (b0 << 12) | (b1 << 20) | (s0_top << 28);
                        let s1 = (b3 << 12) | (b4 << 20) | (s1_top << 28);
                        // Sign-extend the 20-bit sample placed in bits 12..31.
                        let s0 = ((s0 as i32) >> 12) as i32;
                        let s1 = ((s1 as i32) >> 12) as i32;
                        (s0, s1)
                    }
                    24 => {
                        // Encoder:
                        //   o[0] = br((s0 & 0x0000_FF00) >> 8 )  → bits 8..15 of slot
                        //   o[1] = br((s0 & 0x00FF_0000) >> 16)  → bits 16..23
                        //   o[2] = br((s0 & 0xFF00_0000) >> 24)  → bits 24..31
                        //   o[3] = br((s1 & 0x0000_0F00) >> 4 ) | vucf
                        //          → 4 sample bits (originally at slot 8..11)
                        //            placed at byte 4..7, br'd to byte 0..3,
                        //            then OR'd with vucf at byte bit 4
                        //   o[4..6] = remaining bytes of s1.
                        let b0 = br(audio[off]) as u32;
                        let b1 = br(audio[off + 1]) as u32;
                        let b2 = br(audio[off + 2]) as u32;
                        // Strip vucf (bits 4..7) before bit-reversing o[3].
                        let b3_stripped = br(audio[off + 3] & 0x0F) as u32;
                        let b4 = br(audio[off + 4]) as u32;
                        let b5 = br(audio[off + 5]) as u32;
                        let b6 = br(audio[off + 6]) as u32;
                        let s0 = (b0 << 8) | (b1 << 16) | (b2 << 24);
                        // b3_stripped has the 4 sample bits at byte positions 4..7,
                        // which directly map to slot positions 8..11 (shift left by 4).
                        let s1 = (b3_stripped << 4)
                            | (b4 << 12)
                            | (b5 << 20)
                            | (b6 << 28);
                        // Sign-extend the 24-bit sample placed in bits 8..31.
                        let s0 = ((s0 as i32) >> 8) as i32;
                        let s1 = ((s1 as i32) >> 8) as i32;
                        (s0, s1)
                    }
                    _ => unreachable!(),
                };
                planar[pair * 2].push(s0 as f32 * scale_inv);
                planar[pair * 2 + 1].push(s1 as f32 * scale_inv);
                off += bytes_per_pair;
            }
        }

        Ok(S302mFrame {
            sample_rate: 48_000,
            channels,
            bit_depth,
            samples: planar,
        })
    }
}

/// One depacketized SMPTE 302M PES payload's worth of audio samples.
#[derive(Debug, Clone)]
pub struct S302mFrame {
    pub sample_rate: u32, // always 48000 per the spec
    pub channels: u8,
    pub bit_depth: u8,
    pub samples: Vec<Vec<f32>>, // planar [chan][frame]
}

// ── End-to-end output pipeline ──────────────────────────────────────────────

use crate::engine::audio_transcode::{
    BitDepth, ChannelMatrix, InputFormat, MatrixSource, SrcQuality, TranscodeConfig,
    TranscodeStage, TranscodeStats,
};
use crate::engine::packet::RtpPacket;
use crate::engine::rtmp::ts_mux::TsMuxer;
use std::sync::Arc;

/// 1316 bytes (7 × 188-byte TS packets) — the standard MPEG-TS-over-SRT
/// datagram size for live mode SRT.
pub const SRT_TS_DATAGRAM_BYTES: usize = 7 * 188;

/// RFC 2250 / RFC 3551 static RTP payload type for MPEG-2 Transport Stream.
pub const RTP_MP2T_PAYLOAD_TYPE: u8 = 33;

/// Wrap a 1316-byte (7 × 188) MPEG-TS chunk in an RTP/MP2T header per
/// RFC 2250 §2. Used by the rtp_audio output in `audio_302m` mode.
///
/// `seq` is the next RTP sequence number (caller advances after each call);
/// `ts_90khz` is the RTP timestamp in 90 kHz ticks; `ssrc` is the session
/// SSRC. The RTP marker bit is left clear (RFC 2250 reserves it for
/// experimental signalling).
pub fn rtp_mp2t_packet(
    ts_chunk: &[u8],
    seq: u16,
    ts_90khz: u32,
    ssrc: u32,
) -> bytes::Bytes {
    let mut buf = bytes::BytesMut::with_capacity(12 + ts_chunk.len());
    buf.extend_from_slice(&[0x80, RTP_MP2T_PAYLOAD_TYPE & 0x7F]);
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&ts_90khz.to_be_bytes());
    buf.extend_from_slice(&ssrc.to_be_bytes());
    buf.extend_from_slice(ts_chunk);
    buf.freeze()
}

/// End-to-end "PCM in → SMPTE 302M-in-MPEG-TS bytes out" pipeline used by
/// the SRT, RTP, and UDP outputs in `audio_302m` mode.
///
/// Composition:
///
/// 1. [`TranscodeStage`] resamples the upstream input audio to 48 kHz,
///    promotes the channel count to one of {2,4,6,8}, and quantizes to
///    16 / 20 / 24-bit. (When the input is already 48 kHz with an even
///    channel count, no SRC happens — only bit-depth quantization.)
/// 2. The transcoder emits one or more RTP/PCM packets per call. We
///    immediately depacketize each one back to planar f32 samples (this
///    avoids re-running the SRC and re-quantization that the transcoder
///    has already done; the RTP wrapper is just a structural delivery
///    mechanism inside the transcoder).
/// 3. Each f32 block is fed to [`S302mPacketizer::packetize_f32`] which
///    bit-packs it as a SMPTE 302M PES payload.
/// 4. [`TsMuxer::mux_private_audio`] wraps the PES payload in MPEG-TS
///    packets on the audio PID and prepends PAT/PMT periodically.
/// 5. The TS packets are bundled into 7-packet (1316-byte) chunks ready
///    to ship over the SRT live-mode socket.
///
/// The pipeline is **stateful**: TS continuity counters, the muxer's
/// PAT/PMT cadence, the 302M framing index, and the PES PTS counter all
/// advance across calls.
pub struct S302mOutputPipeline {
    transcode: TranscodeStage,
    packetizer: S302mPacketizer,
    ts_mux: TsMuxer,
    /// 90 kHz PTS counter, advances by `out_packet_time_us * 90 / 1000`
    /// per emitted PES packet.
    pts_90khz: u64,
    /// Output channel count (2/4/6/8) — used to verify the transcoder
    /// produced the right shape on each call.
    out_channels: u8,
    /// Output bit depth (16/20/24).
    out_bit_depth: u8,
    /// Output samples per emitted block (matches the transcoder's
    /// `out_packet_time_us` × 48000 / 1e6).
    out_samples_per_block: usize,
    /// Pending TS bytes accumulated across calls — drained in
    /// `SRT_TS_DATAGRAM_BYTES` chunks via `take_ready_datagrams`.
    pending_ts: Vec<u8>,
}

impl S302mOutputPipeline {
    /// Build the pipeline. The output sample rate is always 48 kHz; the
    /// caller chooses bit depth (16/20/24) and channel count (2/4/6/8).
    /// Packet time is in microseconds — typical values are 4000 (4 ms,
    /// 192 frames) for ordinary contribution and 1000 (1 ms) for low-latency
    /// talkback.
    pub fn new(
        input: InputFormat,
        out_channels: u8,
        out_bit_depth: u8,
        out_packet_time_us: u32,
    ) -> Result<Self, String> {
        if !matches!(out_channels, 2 | 4 | 6 | 8) {
            return Err(format!(
                "S302mOutputPipeline: out_channels must be 2/4/6/8, got {out_channels}"
            ));
        }
        if !matches!(out_bit_depth, 16 | 20 | 24) {
            return Err(format!(
                "S302mOutputPipeline: out_bit_depth must be 16/20/24, got {out_bit_depth}"
            ));
        }
        // Synthesize a TranscodeConfig with a default identity / promote
        // matrix matching the input → output channel mapping.
        let channel_matrix = if input.channels == out_channels {
            ChannelMatrix::identity(out_channels)
        } else if input.channels == 1 && out_channels == 2 {
            // mono → stereo dup
            ChannelMatrix(vec![vec![(0, 1.0)], vec![(0, 1.0)]])
        } else if input.channels >= out_channels {
            // Take the first N input channels.
            ChannelMatrix(
                (0..out_channels)
                    .map(|i| vec![(i, 1.0_f32)])
                    .collect(),
            )
        } else {
            return Err(format!(
                "S302mOutputPipeline: cannot route {} input channels to {} output channels — \
                 specify an explicit channel_map in the transcode config",
                input.channels, out_channels
            ));
        };
        let cfg = TranscodeConfig {
            out_sample_rate: 48_000,
            out_bit_depth: BitDepth::from_u8(out_bit_depth)
                .ok_or_else(|| format!("invalid bit depth {out_bit_depth}"))?,
            out_channels,
            channel_matrix,
            out_packet_time_us,
            out_payload_type: 97,
            src_quality: SrcQuality::High,
            dither: crate::engine::audio_transcode::Dither::Tpdf,
        };
        let stats = Arc::new(TranscodeStats::new());
        let matrix = MatrixSource::static_(cfg.channel_matrix.clone());
        let transcode = TranscodeStage::new(input, cfg, matrix, stats, 0xCAFE_BABE, 0, 0);
        let packetizer = S302mPacketizer::new(out_channels, out_bit_depth)?;
        let mut ts_mux = TsMuxer::new();
        ts_mux.set_has_video(false);
        ts_mux.set_has_audio(true);
        ts_mux.set_audio_stream(STREAM_TYPE_S302M, Some(BSSD_FORMAT_ID));
        let out_samples_per_block =
            ((out_packet_time_us as u64 * 48_000) / 1_000_000) as usize;
        Ok(Self {
            transcode,
            packetizer,
            ts_mux,
            pts_90khz: 0,
            out_channels,
            out_bit_depth,
            out_samples_per_block,
            pending_ts: Vec::with_capacity(SRT_TS_DATAGRAM_BYTES * 4),
        })
    }

    /// Feed one input RTP audio packet through the pipeline. After this
    /// call, drain ready 1316-byte datagrams via [`take_ready_datagrams`].
    pub fn process(&mut self, packet: &RtpPacket) {
        let rtp_packets = self.transcode.process(packet);
        // The transcoder emits one or more PcmPacketizer-formatted RTP
        // audio packets at 48 kHz / out_channels / out_bit_depth. For each
        // one, decode the payload back to planar f32 (the conversion is
        // lossless since the encode → decode of i16/i24 PCM is exact), pack
        // as 302M, and mux into TS.
        let bps_wire = match self.out_bit_depth {
            16 => 2usize,
            20 => 3usize,
            24 => 3usize,
            _ => return,
        };
        let frame_size = self.out_channels as usize * bps_wire;
        let bit_depth_enum = match self.out_bit_depth {
            16 => BitDepth::L16,
            20 => BitDepth::L20,
            24 => BitDepth::L24,
            _ => return,
        };
        for rtp in rtp_packets {
            // Strip the 12-byte RTP header to get the PCM payload.
            if rtp.len() < 12 {
                continue;
            }
            let payload = &rtp[12..];
            if payload.len() % frame_size != 0 {
                continue;
            }
            let n_frames = payload.len() / frame_size;
            let mut planar: Vec<Vec<f32>> =
                (0..self.out_channels).map(|_| Vec::with_capacity(n_frames)).collect();
            // Use the existing decode_pcm_be helper to recover f32 samples.
            if crate::engine::audio_transcode::decode_pcm_be(
                payload,
                bit_depth_enum,
                self.out_channels,
                &mut planar,
            )
            .is_err()
            {
                continue;
            }
            let pes = match self.packetizer.packetize_f32(&planar) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts_packets = self.ts_mux.mux_private_audio(&pes, self.pts_90khz);
            for ts_pkt in ts_packets {
                self.pending_ts.extend_from_slice(&ts_pkt);
            }
            // Advance PTS by one packet time at 90 kHz.
            // packet_time_us * 90 / 1000 = ((us * 9) / 100)
            // For 4 ms (4000 us) → 360 ticks. For 1 ms → 90 ticks.
            let pts_advance = (n_frames as u64 * 90_000) / 48_000;
            self.pts_90khz = self.pts_90khz.wrapping_add(pts_advance);
        }
    }

    /// Feed pre-decoded planar f32 PCM through the pipeline. Mirrors
    /// [`process`](Self::process) but skips the upstream RTP-PCM decode
    /// stage — used by the compressed-audio bridge in
    /// [`crate::engine::output_rtp_audio`] when the upstream input is AAC
    /// in MPEG-TS and has already been decoded by
    /// [`crate::engine::audio_decode::AacDecoder`].
    ///
    /// `recv_time_us` is the upstream packet receive timestamp; passed
    /// through to the transcode stage solely for latency stats.
    ///
    /// After this call, drain ready 1316-byte datagrams via
    /// [`take_ready_datagrams`](Self::take_ready_datagrams).
    pub fn process_planar(&mut self, planar: &[Vec<f32>], recv_time_us: u64) {
        let rtp_packets = self.transcode.process_planar(planar, recv_time_us);
        let bps_wire = match self.out_bit_depth {
            16 => 2usize,
            20 => 3usize,
            24 => 3usize,
            _ => return,
        };
        let frame_size = self.out_channels as usize * bps_wire;
        let bit_depth_enum = match self.out_bit_depth {
            16 => BitDepth::L16,
            20 => BitDepth::L20,
            24 => BitDepth::L24,
            _ => return,
        };
        for rtp in rtp_packets {
            if rtp.len() < 12 {
                continue;
            }
            let payload = &rtp[12..];
            if payload.len() % frame_size != 0 {
                continue;
            }
            let n_frames = payload.len() / frame_size;
            let mut planar_out: Vec<Vec<f32>> =
                (0..self.out_channels).map(|_| Vec::with_capacity(n_frames)).collect();
            if crate::engine::audio_transcode::decode_pcm_be(
                payload,
                bit_depth_enum,
                self.out_channels,
                &mut planar_out,
            )
            .is_err()
            {
                continue;
            }
            let pes = match self.packetizer.packetize_f32(&planar_out) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts_packets = self.ts_mux.mux_private_audio(&pes, self.pts_90khz);
            for ts_pkt in ts_packets {
                self.pending_ts.extend_from_slice(&ts_pkt);
            }
            let pts_advance = (n_frames as u64 * 90_000) / 48_000;
            self.pts_90khz = self.pts_90khz.wrapping_add(pts_advance);
        }
    }

    /// Drain ready 1316-byte SRT datagrams. Returns whatever is currently
    /// buffered, leaving any partial datagram for the next call.
    pub fn take_ready_datagrams(&mut self) -> Vec<bytes::Bytes> {
        let mut out = Vec::new();
        let mut taken = 0;
        while self.pending_ts.len() - taken >= SRT_TS_DATAGRAM_BYTES {
            out.push(bytes::Bytes::copy_from_slice(
                &self.pending_ts[taken..taken + SRT_TS_DATAGRAM_BYTES],
            ));
            taken += SRT_TS_DATAGRAM_BYTES;
        }
        if taken > 0 {
            self.pending_ts.drain(..taken);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_planar(channels: u8, n_frames: usize, maker: impl Fn(usize, usize) -> f32) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|ch| (0..n_frames).map(|f| maker(ch as usize, f)).collect())
            .collect()
    }

    #[test]
    fn header_round_trip_2ch_16bit() {
        let mut p = S302mPacketizer::new(2, 16).unwrap();
        let in_planar = make_planar(2, 32, |ch, f| {
            ((f as f32 * 0.01) - 0.16) * (1.0 - ch as f32 * 0.5)
        });
        let pes = p.packetize_f32(&in_planar).unwrap();
        let frame = S302mDepacketizer::new().parse(&pes).unwrap();
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.bit_depth, 16);
        assert_eq!(frame.samples[0].len(), 32);
        // ±1 LSB in 16-bit is ~3e-5 in f32 normalized form.
        for ch in 0..2 {
            for f in 0..32 {
                let diff = (in_planar[ch][f] - frame.samples[ch][f]).abs();
                assert!(diff < 1e-4, "16-bit round-trip diff {diff}");
            }
        }
    }

    #[test]
    fn header_round_trip_2ch_24bit() {
        let mut p = S302mPacketizer::new(2, 24).unwrap();
        let in_planar = make_planar(2, 64, |ch, f| {
            ((f as f32 * 0.001) - 0.032) * if ch == 0 { 1.0 } else { -0.5 }
        });
        let pes = p.packetize_f32(&in_planar).unwrap();
        let frame = S302mDepacketizer::new().parse(&pes).unwrap();
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.bit_depth, 24);
        for ch in 0..2 {
            for f in 0..64 {
                let diff = (in_planar[ch][f] - frame.samples[ch][f]).abs();
                assert!(diff < 1e-6, "24-bit round-trip diff {diff}");
            }
        }
    }

    #[test]
    fn header_round_trip_2ch_20bit() {
        let mut p = S302mPacketizer::new(2, 20).unwrap();
        let in_planar = make_planar(2, 32, |ch, f| {
            ((f as f32 * 0.005) - 0.08) * if ch == 0 { 0.7 } else { 0.3 }
        });
        let pes = p.packetize_f32(&in_planar).unwrap();
        let frame = S302mDepacketizer::new().parse(&pes).unwrap();
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.bit_depth, 20);
        for ch in 0..2 {
            for f in 0..32 {
                let diff = (in_planar[ch][f] - frame.samples[ch][f]).abs();
                assert!(diff < 2e-6, "20-bit round-trip diff {diff}");
            }
        }
    }

    #[test]
    fn round_trip_8ch_24bit() {
        let mut p = S302mPacketizer::new(8, 24).unwrap();
        let in_planar = make_planar(8, 16, |ch, f| {
            (f as f32 * 0.01).sin() * (0.1 + ch as f32 * 0.05)
        });
        let pes = p.packetize_f32(&in_planar).unwrap();
        let frame = S302mDepacketizer::new().parse(&pes).unwrap();
        assert_eq!(frame.channels, 8);
        assert_eq!(frame.bit_depth, 24);
        for ch in 0..8 {
            for f in 0..16 {
                let diff = (in_planar[ch][f] - frame.samples[ch][f]).abs();
                assert!(diff < 1e-6, "8ch/24bit diff {diff} at ch={ch} f={f}");
            }
        }
    }

    #[test]
    fn rejects_invalid_channels() {
        assert!(S302mPacketizer::new(1, 24).is_err());
        assert!(S302mPacketizer::new(3, 24).is_err());
        assert!(S302mPacketizer::new(0, 24).is_err());
        assert!(S302mPacketizer::new(10, 24).is_err());
    }

    #[test]
    fn rejects_invalid_bit_depth() {
        assert!(S302mPacketizer::new(2, 8).is_err());
        assert!(S302mPacketizer::new(2, 32).is_err());
    }

    #[test]
    fn header_advertises_correct_fields_2ch_24bit() {
        let mut p = S302mPacketizer::new(2, 24).unwrap();
        let in_planar = make_planar(2, 4, |_, _| 0.0);
        let pes = p.packetize_f32(&in_planar).unwrap();
        // 4-byte header
        let aps = u16::from_be_bytes([pes[0], pes[1]]) as usize;
        // 4 frames × 1 pair × 7 bytes = 28
        assert_eq!(aps, 28);
        let nc_field = (pes[2] >> 6) & 0x03;
        let bps_field = (pes[3] >> 4) & 0x03;
        assert_eq!(nc_field, 0); // 2ch
        assert_eq!(bps_field, 2); // 24-bit
        assert_eq!(pes.len(), AES3_HEADER_LEN + aps);
    }

    #[test]
    fn header_advertises_correct_fields_8ch_16bit() {
        let mut p = S302mPacketizer::new(8, 16).unwrap();
        let in_planar = make_planar(8, 4, |_, _| 0.0);
        let pes = p.packetize_f32(&in_planar).unwrap();
        let aps = u16::from_be_bytes([pes[0], pes[1]]) as usize;
        // 4 frames × 4 pairs × 5 bytes = 80
        assert_eq!(aps, 80);
        let nc_field = (pes[2] >> 6) & 0x03;
        let bps_field = (pes[3] >> 4) & 0x03;
        assert_eq!(nc_field, 3); // 8ch
        assert_eq!(bps_field, 0); // 16-bit
    }

    #[test]
    fn parser_rejects_short_payload() {
        let d = S302mDepacketizer::new();
        assert!(d.parse(&[0u8; 2]).is_err());
    }

    #[test]
    fn parser_rejects_truncated_body() {
        // Header says 100 audio bytes but only 4 in payload
        let mut p = vec![0u8; 4];
        p[0] = 0x00;
        p[1] = 0x64; // 100
        p[2] = 0x00; // 2ch
        p[3] = 0x20; // 24-bit (bps_field=2 → byte = 0x20)
        let d = S302mDepacketizer::new();
        assert!(d.parse(&p).is_err());
    }

    #[test]
    fn s302m_output_pipeline_emits_ts_datagrams() {
        use crate::engine::audio_transcode::{BitDepth, InputFormat};
        use crate::engine::packet::RtpPacket;
        use bytes::Bytes;

        // Build pipeline: 48k/24/2 in → 48k/24/2 out, 4ms packets.
        let input = InputFormat {
            sample_rate: 48_000,
            bit_depth: BitDepth::L24,
            channels: 2,
        };
        let mut pipeline = S302mOutputPipeline::new(input, 2, 24, 4_000).unwrap();

        // Build a 1ms (48-frame) stereo L24 RTP packet using a known ramp.
        fn build_rtp(seq: u16, ts: u32) -> RtpPacket {
            let mut data = Vec::new();
            data.push(0x80);
            data.push(97);
            data.extend_from_slice(&seq.to_be_bytes());
            data.extend_from_slice(&ts.to_be_bytes());
            data.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
            for i in 0..(48 * 2 * 3) {
                data.push(i as u8);
            }
            RtpPacket {
                data: Bytes::from(data),
                sequence_number: seq,
                rtp_timestamp: ts,
                recv_time_us: 0,
                is_raw_ts: false,
            }
        }

        // Push enough packets to fill at least one 1316-byte SRT datagram.
        // Each 4ms PES carries 192 frames × 2ch × 7 bytes/pair = 2688 bytes
        // of audio + 4 byte header = 2692 PES bytes. PES expansion to TS
        // adds PES header (~14 bytes) and TS header (4 bytes per 188-byte
        // packet) overhead. Several input packets should comfortably yield
        // a full 1316-byte datagram.
        for i in 0..20 {
            let pkt = build_rtp(i, (i as u32) * 48);
            pipeline.process(&pkt);
        }

        let datagrams = pipeline.take_ready_datagrams();
        assert!(
            !datagrams.is_empty(),
            "expected at least one 1316-byte SRT datagram"
        );
        for d in &datagrams {
            assert_eq!(d.len(), SRT_TS_DATAGRAM_BYTES);
            // Each datagram should be 7 valid TS packets (sync byte at 0, 188, 376...).
            for i in 0..7 {
                assert_eq!(
                    d[i * 188],
                    0x47,
                    "datagram TS packet {i} missing sync byte"
                );
            }
        }
    }

    #[test]
    fn bit_reverse_lut_is_self_inverse() {
        for x in 0u8..=255 {
            assert_eq!(BIT_REVERSE[BIT_REVERSE[x as usize] as usize], x);
        }
    }
}
