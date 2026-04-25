// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Minimal bit reader + Exp-Golomb decoder for H.264 / H.265 RBSP parsing.
//!
//! Used by the content-analysis SPS / VUI / SEI parsers. Kept deliberately
//! small — no allocations, no `Result` churn; out-of-buffer returns `None`.

/// A forward bit reader with Exp-Golomb (`ue(v)` / `se(v)`) support.
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Total remaining bits.
    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }

    pub fn read_bit(&mut self) -> Option<u8> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.data.len() {
            return None;
        }
        let bit = (self.data[byte_idx] >> (7 - (self.bit_pos % 8))) & 1;
        self.bit_pos += 1;
        Some(bit)
    }

    pub fn read_bits(&mut self, n: u8) -> Option<u32> {
        if n > 32 {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | (self.read_bit()? as u32);
        }
        Some(v)
    }

    /// Exp-Golomb unsigned `ue(v)` — H.264 §9.1.
    pub fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u8;
        loop {
            let b = self.read_bit()?;
            if b == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Exp-Golomb signed `se(v)` — H.264 §9.1.1.
    pub fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        if ue & 1 != 0 {
            Some((ue as i32 + 1) / 2)
        } else {
            Some(-((ue as i32) / 2))
        }
    }
}

/// Strip emulation-prevention bytes (`0x00 0x00 0x03` → `0x00 0x00`) from
/// an H.264 / H.265 RBSP byte stream. Per ITU-T H.264 §7.4.1.1 emulation
/// prevention is only inserted after two zero bytes, so a straight three-
/// byte scan is both correct and cheap.
///
/// Returns a freshly allocated `Vec<u8>` because most callers want to then
/// hand the payload to [`BitReader`] which needs a contiguous slice. The
/// allocation happens once per SPS / SEI observation (≤ a few per second),
/// well off the data path.
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0x03 {
            out.push(0);
            out.push(0);
            i += 3;
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ue_zero() {
        let mut r = BitReader::new(&[0b1000_0000]);
        assert_eq!(r.read_ue(), Some(0));
    }

    #[test]
    fn reads_ue_one() {
        // 010 -> ue=1
        let mut r = BitReader::new(&[0b0100_0000]);
        assert_eq!(r.read_ue(), Some(1));
    }

    #[test]
    fn reads_ue_seven() {
        // 0001000 -> ue=7
        let mut r = BitReader::new(&[0b0001_0000, 0x00]);
        assert_eq!(r.read_ue(), Some(7));
    }

    #[test]
    fn reads_se_mapping() {
        // ue=0 -> se=0, ue=1 -> se=1, ue=2 -> se=-1, ue=3 -> se=2, ue=4 -> se=-2
        // Encode [0, 1, 2, 3, 4] then decode as se.
        // 1, 010, 011, 00100, 00101
        let bytes = [0b1_010_011_0, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_se(), Some(0));
        assert_eq!(r.read_se(), Some(1));
        assert_eq!(r.read_se(), Some(-1));
        assert_eq!(r.read_se(), Some(2));
        assert_eq!(r.read_se(), Some(-2));
    }

    #[test]
    fn unescape_strips_03_sequences() {
        let input = [0, 0, 3, 1, 0, 0, 3, 0, 2];
        let out = unescape_rbsp(&input);
        assert_eq!(out, vec![0, 0, 1, 0, 0, 0, 2]);
    }
}
