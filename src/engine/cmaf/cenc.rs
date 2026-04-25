// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CMAF ClearKey CENC (Common Encryption, ISO/IEC 23001-7).
//!
//! Implements two schemes:
//! - `cenc` (AES-128 **CTR**): full-bytes encryption of the non-clear
//!   portion of each subsample. Per-sample 16-byte IV.
//! - `cbcs` (AES-128 **CBC** with 1:9 block pattern): every 10 AES blocks
//!   (160 bytes) in the encrypted span, the first is encrypted and
//!   the next nine pass through clear. Used by Apple FairPlay.
//!
//! Subsample splitting for H.264 / HEVC: each NAL unit is split into a
//! clear (length prefix + NAL header) and encrypted (payload) pair.
//! Non-VCL NALUs (SPS/PPS/VPS/SEI/AUD) are fully clear.
//!
//! ISO/IEC 23001-7 box writers (`tenc`, `senc`, `saio`, `saiz`, `pssh`)
//! live in [`super::cenc_boxes`]. This module owns the cipher itself +
//! subsample splitter + per-sample IV generation.

use aes::Aes128;
use aes::cipher::KeyInit;
use ctr::cipher::StreamCipher;

use super::nalu::{H264_NAL_SPS, H264_NAL_PPS, H264_NAL_AUD, H264_NAL_SEI};
use super::nalu::{h264_nal_type, h265_nal_type, H265_NAL_VPS, H265_NAL_SPS, H265_NAL_PPS, H265_NAL_AUD};

/// Encryption scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// AES-128 CTR, per-sample IV, encrypt entire payload.
    Cenc,
    /// AES-128 CBC with 1:9 block pattern (9 clear / 1 encrypted each 10-block cycle).
    Cbcs,
}

impl Scheme {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cenc" => Some(Self::Cenc),
            "cbcs" => Some(Self::Cbcs),
            _ => None,
        }
    }

    pub fn fourcc(self) -> [u8; 4] {
        match self {
            Scheme::Cenc => *b"cenc",
            Scheme::Cbcs => *b"cbcs",
        }
    }
}

/// One clear/encrypted pair inside a sample.
#[derive(Debug, Clone, Copy)]
pub struct Subsample {
    /// Clear bytes at the start of this pair.
    pub bytes_of_clear_data: u16,
    /// Encrypted bytes immediately following the clear prefix.
    pub bytes_of_protected_data: u32,
}

/// Per-sample encryption information used to populate `senc` / `saio`
/// / `saiz`.
#[derive(Debug, Clone)]
pub struct SampleCencInfo {
    /// 16-byte IV used for this sample.
    pub iv: [u8; 16],
    /// Subsample layout. May be empty for audio (whole-sample
    /// encryption with no subsample split).
    pub subsamples: Vec<Subsample>,
}

/// Streaming per-track encryptor. Owns the 16-byte content key and
/// the per-sample IV generator.
pub struct CencEncryptor {
    scheme: Scheme,
    key: [u8; 16],
    /// Monotonically incremented 64-bit nonce low word, concatenated
    /// with zero high bytes to form the 16-byte IV. Matches Widevine /
    /// shaka-packager default behaviour for ClearKey.
    iv_counter: u64,
}

impl CencEncryptor {
    pub fn new(scheme: Scheme, key: [u8; 16]) -> Self {
        Self {
            scheme,
            key,
            iv_counter: 1,
        }
    }

    /// Next per-sample IV (cenc) or the track-constant IV (cbcs).
    /// For `cbcs` ISO/IEC 23001-7 §10.4.2.2 allows both per-sample and
    /// constant-IV; we use a constant (all-zero) IV for compatibility
    /// with FairPlay Streaming which mandates it.
    fn next_iv(&mut self) -> [u8; 16] {
        match self.scheme {
            Scheme::Cenc => {
                let mut iv = [0u8; 16];
                iv[8..16].copy_from_slice(&self.iv_counter.to_be_bytes());
                self.iv_counter = self.iv_counter.wrapping_add(1);
                iv
            }
            Scheme::Cbcs => [0u8; 16],
        }
    }

    /// Encrypt one AVCC/HVCC sample in place. Returns the per-sample
    /// CENC info (IV + subsample map) for the `senc` box.
    ///
    /// `sample_bytes` is the packed sample payload — a sequence of
    /// `length_prefix (4 bytes) | NAL unit bytes`. The clear prefix
    /// for each NAL unit is:
    ///   - 4 bytes length prefix
    ///   - 1 byte NAL header (H.264) or 2 bytes (HEVC)
    ///   - slice_header bytes (approximate — we conservatively leave
    ///     32 bytes clear for VCL NALs to cover typical slice
    ///     headers; this is compatible with Widevine/PlayReady and
    ///     avoids fragile bit-level slice header parsing).
    /// Non-VCL NALs (SPS/PPS/VPS/SEI/AUD) are fully clear.
    pub fn encrypt_video_sample(
        &mut self,
        sample_bytes: &mut [u8],
        codec: VideoCodec,
    ) -> SampleCencInfo {
        let iv = self.next_iv();
        let mut subsamples: Vec<Subsample> = Vec::new();
        let mut offset = 0usize;
        let mut cipher_state = CipherState::new(self.scheme, &self.key, &iv);

        while offset + 4 <= sample_bytes.len() {
            let n = u32::from_be_bytes([
                sample_bytes[offset],
                sample_bytes[offset + 1],
                sample_bytes[offset + 2],
                sample_bytes[offset + 3],
            ]) as usize;
            if offset + 4 + n > sample_bytes.len() {
                break;
            }
            let nal_start = offset + 4;
            let nal_end = nal_start + n;
            let nal_type = match codec {
                VideoCodec::H264 => h264_nal_type(&sample_bytes[nal_start..nal_end]),
                VideoCodec::H265 => h265_nal_type(&sample_bytes[nal_start..nal_end]),
            };

            // Decide how much of this NAL is clear. Non-VCL NALs are
            // fully clear; VCL NALs (non-IDR slice, IDR slice, coded
            // trailing picture, etc.) are partially encrypted.
            let fully_clear = match codec {
                VideoCodec::H264 => matches!(nal_type, H264_NAL_SPS | H264_NAL_PPS | H264_NAL_AUD | H264_NAL_SEI),
                VideoCodec::H265 => matches!(nal_type, H265_NAL_VPS | H265_NAL_SPS | H265_NAL_PPS | H265_NAL_AUD),
            };

            // Clear bytes span: 4-byte length prefix + NAL header + (slice header estimate).
            let nal_header_len = match codec {
                VideoCodec::H264 => 1,
                VideoCodec::H265 => 2,
            };
            // Conservative slice header estimate: 32 bytes. Real
            // implementations can parse the slice_header bitstream
            // for byte-perfect alignment; 32 bytes is safe and
            // accepted by Shaka, ExoPlayer, and MSE CENC clients for
            // mainstream H.264 / HEVC streams.
            let slice_header_estimate = 32usize.min(n.saturating_sub(nal_header_len));
            let clear_prefix = if fully_clear {
                4 + n
            } else {
                4 + nal_header_len + slice_header_estimate
            };
            let clear_prefix = clear_prefix.min(4 + n);
            let encrypted_len = (4 + n) - clear_prefix;

            // For cbcs we must round the encrypted length down to a
            // multiple of 16 bytes (one AES block). Any trailing
            // bytes under 16 stay clear.
            let encrypted_len = match self.scheme {
                Scheme::Cenc => encrypted_len,
                Scheme::Cbcs => encrypted_len - (encrypted_len % 16),
            };

            if encrypted_len > 0 {
                let enc_start = offset + clear_prefix;
                let enc_end = enc_start + encrypted_len;
                cipher_state.apply(&mut sample_bytes[enc_start..enc_end]);
            }

            subsamples.push(Subsample {
                bytes_of_clear_data: clear_prefix as u16,
                bytes_of_protected_data: encrypted_len as u32,
            });

            offset = offset + 4 + n;
        }

        SampleCencInfo { iv, subsamples }
    }

    /// Encrypt one AAC audio sample in place. AAC uses whole-sample
    /// encryption with no subsample split; CBC mode still requires
    /// 16-byte alignment (bytes beyond the last full block pass
    /// through clear), which is the spec-defined behaviour.
    ///
    /// Reserved for the Phase 5 audio-track CENC wiring — currently
    /// only the video track is encrypted (see `encrypt_video_sample`).
    #[allow(dead_code)]
    pub fn encrypt_audio_sample(&mut self, sample_bytes: &mut [u8]) -> SampleCencInfo {
        let iv = self.next_iv();
        let n = sample_bytes.len();
        let enc_len = match self.scheme {
            Scheme::Cenc => n,
            Scheme::Cbcs => n - (n % 16),
        };
        if enc_len > 0 {
            let mut cs = CipherState::new(self.scheme, &self.key, &iv);
            cs.apply(&mut sample_bytes[..enc_len]);
        }
        SampleCencInfo {
            iv,
            subsamples: Vec::new(),
        }
    }
}

pub use super::fmp4::VideoCodec;

/// Shared cipher state across the multiple blocks of one sample.
enum CipherState {
    Ctr(ctr::Ctr128BE<Aes128>),
    Cbcs {
        cipher: Aes128,
        iv: [u8; 16],
    },
}

impl CipherState {
    fn new(scheme: Scheme, key: &[u8; 16], iv: &[u8; 16]) -> Self {
        match scheme {
            Scheme::Cenc => {
                use ctr::cipher::KeyIvInit;
                let c = ctr::Ctr128BE::<Aes128>::new(key.into(), iv.into());
                CipherState::Ctr(c)
            }
            Scheme::Cbcs => CipherState::Cbcs {
                cipher: Aes128::new(key.into()),
                iv: *iv,
            },
        }
    }

    fn apply(&mut self, bytes: &mut [u8]) {
        match self {
            CipherState::Ctr(c) => {
                c.apply_keystream(bytes);
            }
            CipherState::Cbcs { cipher, iv } => {
                // 1:9 pattern: encrypt block 0, pass through blocks 1..=9.
                use aes::cipher::BlockCipherEncrypt;
                let mut i = 0usize;
                let mut cbc_iv = *iv;
                while i + 16 <= bytes.len() {
                    // Every 10th position (0, 10, 20, ...) is encrypted.
                    if (i / 16) % 10 == 0 {
                        // One AES-CBC block: XOR IV, encrypt, result becomes next IV.
                        let block: &mut [u8; 16] = (&mut bytes[i..i + 16])
                            .try_into()
                            .expect("16-byte slice");
                        for (b, k) in block.iter_mut().zip(cbc_iv.iter()) {
                            *b ^= *k;
                        }
                        cipher.encrypt_block(block.into());
                        cbc_iv.copy_from_slice(block);
                    }
                    i += 16;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_schemes() {
        assert_eq!(Scheme::parse("cenc"), Some(Scheme::Cenc));
        assert_eq!(Scheme::parse("cbcs"), Some(Scheme::Cbcs));
        assert_eq!(Scheme::parse("xyz"), None);
    }

    #[test]
    fn iv_counter_increments_for_cenc() {
        let mut e = CencEncryptor::new(Scheme::Cenc, [0u8; 16]);
        let iv1 = e.next_iv();
        let iv2 = e.next_iv();
        assert_eq!(&iv1[0..8], &[0u8; 8]);
        assert_eq!(&iv2[0..8], &[0u8; 8]);
        assert_ne!(iv1[8..16], iv2[8..16]);
    }

    #[test]
    fn cbcs_uses_constant_iv() {
        let mut e = CencEncryptor::new(Scheme::Cbcs, [0u8; 16]);
        let iv1 = e.next_iv();
        let iv2 = e.next_iv();
        assert_eq!(iv1, iv2);
        assert_eq!(iv1, [0u8; 16]);
    }

    #[test]
    fn cenc_ctr_roundtrip() {
        let key = [0xABu8; 16];
        let iv = [0x11u8; 16];
        let plain = b"hello cenc world this is a test 12345678901234567890";
        let mut buf = plain.to_vec();
        let mut c1 = CipherState::new(Scheme::Cenc, &key, &iv);
        c1.apply(&mut buf);
        assert_ne!(&buf[..], &plain[..]);
        // Decrypt by applying again (CTR is symmetric).
        let mut c2 = CipherState::new(Scheme::Cenc, &key, &iv);
        c2.apply(&mut buf);
        assert_eq!(&buf[..], &plain[..]);
    }

    #[test]
    fn cenc_video_subsamples_populated() {
        let key = [0xABu8; 16];
        let mut enc = CencEncryptor::new(Scheme::Cenc, key);
        // Two synthetic NALs: an SPS (fully clear) + an IDR (encrypted).
        let sps = vec![0x67, 0x42, 0x00, 0x1E]; // H.264 SPS
        let idr = vec![0x65; 100]; // H.264 IDR slice, 100 bytes
        let mut sample = Vec::new();
        sample.extend_from_slice(&(sps.len() as u32).to_be_bytes());
        sample.extend_from_slice(&sps);
        sample.extend_from_slice(&(idr.len() as u32).to_be_bytes());
        sample.extend_from_slice(&idr);
        let info = enc.encrypt_video_sample(&mut sample, VideoCodec::H264);
        assert_eq!(info.subsamples.len(), 2);
        // SPS subsample is fully clear.
        assert_eq!(
            info.subsamples[0].bytes_of_clear_data as usize,
            4 + sps.len()
        );
        assert_eq!(info.subsamples[0].bytes_of_protected_data, 0);
        // IDR has 4 (length) + 1 (header) + 32 (slice hdr) = 37 bytes
        // clear; remainder (100 - 33 = 67) encrypted.
        assert_eq!(info.subsamples[1].bytes_of_clear_data, 4 + 1 + 32);
        assert_eq!(info.subsamples[1].bytes_of_protected_data, 100 - 1 - 32);
    }

    #[test]
    fn cbcs_rounds_down_to_16() {
        let key = [0xABu8; 16];
        let mut enc = CencEncryptor::new(Scheme::Cbcs, key);
        let sample = vec![0x65; 100]; // One 100-byte IDR
        let mut wire = Vec::new();
        wire.extend_from_slice(&(sample.len() as u32).to_be_bytes());
        wire.extend_from_slice(&sample);
        let info = enc.encrypt_video_sample(&mut wire, VideoCodec::H264);
        assert_eq!(info.subsamples.len(), 1);
        // Clear: 4 (length) + 1 (header) + 32 (slice) = 37
        // Encrypted (before round-down): 100 - 33 = 67
        // After round-down to 16: 64
        assert_eq!(info.subsamples[0].bytes_of_clear_data, 37);
        assert_eq!(info.subsamples[0].bytes_of_protected_data, 64);
    }

    #[test]
    fn aac_whole_sample_encrypted() {
        let key = [0xABu8; 16];
        let mut enc = CencEncryptor::new(Scheme::Cenc, key);
        let original = vec![0xFFu8; 128];
        let mut buf = original.clone();
        let info = enc.encrypt_audio_sample(&mut buf);
        assert!(info.subsamples.is_empty());
        assert_ne!(&buf[..], &original[..]);
    }
}
