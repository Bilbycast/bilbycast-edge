// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The arithmetic the H.264 SPS parsers share: the picture's size, and the
//! scaling lists between it and the profile.
//!
//! Each parser reads the bits with a reader of its own and keeps what it
//! needs: CMAF and the MP4 export (`cmaf::codecs`), media analysis, content
//! analysis signalling, the media library probe, the PES splice sentinel and
//! the WHEP input. What an Exp-Golomb code says goes up to 2^32 − 2 and comes
//! off the wire, so none of it is added or multiplied unchecked. Until
//! 2026-11 (edge#213) it was: a source sending such an SPS had its size wrap
//! in a release build (0 px wide in a CMAF init segment) and panicked a build
//! with overflow checks.

/// The largest picture width or height, in luma samples and before cropping,
/// that is taken from an SPS — the HEVC parser's bound (`cmaf::fmp4`).
pub const MAX_DIMENSION: u32 = 16_384;

/// The picture's width and height once cropped (ITU-T H.264 §7.4.2.1.1).
/// `crop` is the four `frame_crop_*_offset` in the SPS's own order (left,
/// right, top, bottom), and `crop_unit` the luma samples one of them is,
/// across and down. `None` for a size over [`MAX_DIMENSION`] and for a crop
/// that leaves no picture.
pub fn picture_size(
    pic_width_in_mbs_minus1: u32,
    pic_height_in_map_units_minus1: u32,
    frame_mbs_only: bool,
    crop: (u32, u32, u32, u32),
    crop_unit: (u32, u32),
) -> Option<(u32, u32)> {
    // A map unit is a macroblock, or a pair of them where fields are coded.
    let map_unit = if frame_mbs_only { 16 } else { 32 };
    let width = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let height = pic_height_in_map_units_minus1.checked_add(1)?.checked_mul(map_unit)?;
    Some((
        cropped(width, (crop.0, crop.1), crop_unit.0)?,
        cropped(height, (crop.2, crop.3), crop_unit.1)?,
    ))
}

/// One dimension, here and in `engine::hevc_sps`: `coded` samples, less the
/// crop.
pub(super) fn cropped(coded: u32, crop: (u32, u32), crop_unit: u32) -> Option<u32> {
    if coded > MAX_DIMENSION {
        return None;
    }
    let crop = crop.0.checked_add(crop.1)?.checked_mul(crop_unit)?;
    coded.checked_sub(crop).filter(|size| *size > 0)
}

/// Skip a scaling list of `size` entries (§7.3.2.1.1.1), `se` reading the
/// next se(v). `None` where that does, and for a `delta_scale` outside the
/// −128..=127 of §7.4.2.1.1.1.
pub fn skip_scaling_list(size: u32, mut se: impl FnMut() -> Option<i32>) -> Option<()> {
    let (mut last, mut next) = (8, 8);
    for _ in 0..size {
        if next != 0 {
            let delta_scale = se().filter(|delta| (-128..=127).contains(delta))?;
            next = (last + delta_scale + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::cmaf::param_sets::tests::{Bits, escaped, hx};

    /// An H.264 SPS, as far as its VUI flag.
    #[derive(Clone, Copy)]
    pub(crate) struct Sps {
        pub profile: u8,
        pub chroma: u32,
        /// `(bit_depth_luma_minus8, bit_depth_chroma_minus8)`.
        pub bit_depths: (u32, u32),
        /// The deltas of the first scaling list; the others are absent.
        pub scaling_list: Option<&'static [i32]>,
        /// Every scaling list, all its deltas 0, where `scaling_list` is
        /// `None`.
        pub flat_lists: bool,
        pub width_in_mbs_minus1: u32,
        pub height_in_map_units_minus1: u32,
        pub frame_mbs_only: bool,
        /// Left, right, top, bottom.
        pub crop: Option<[u32; 4]>,
    }

    /// Baseline, 1920x1088, not cropped.
    impl Default for Sps {
        fn default() -> Self {
            Self {
                profile: 66,
                chroma: 1,
                bit_depths: (0, 0),
                scaling_list: None,
                flat_lists: false,
                width_in_mbs_minus1: 119,
                height_in_map_units_minus1: 67,
                frame_mbs_only: true,
                crop: None,
            }
        }
    }

    impl Sps {
        /// Its RBSP: what is behind the NAL header, with no
        /// emulation-prevention bytes.
        pub(crate) fn rbsp(&self) -> Vec<u8> {
            let mut b = Bits::new(&[self.profile, 0, 40]);
            b.ue(0);
            if self.profile >= 100 || self.profile == 44 {
                b.ue(self.chroma);
                if self.chroma == 3 {
                    b.put(0, 1); // separate_colour_plane_flag
                }
                b.ue(self.bit_depths.0).ue(self.bit_depths.1).put(0, 1);
                b.put(u64::from(self.scaling_list.is_some() || self.flat_lists), 1);
                let lists = if self.chroma == 3 { 12 } else { 8 };
                if let Some(deltas) = self.scaling_list {
                    b.put(1, 1);
                    for &delta in deltas {
                        let delta = i64::from(delta);
                        b.ue(if delta > 0 { 2 * delta - 1 } else { -2 * delta } as u32);
                    }
                    b.put(0, lists - 1);
                } else if self.flat_lists {
                    for list in 0..lists {
                        b.put(1, 1);
                        for _ in 0..if list < 6 { 16 } else { 64 } {
                            b.ue(0);
                        }
                    }
                }
            }
            b.ue(2).ue(0).ue(0); // frame_num of 6 bits, POC type 0 of 4
            b.ue(1).put(0, 1); // one reference picture, no gaps
            b.ue(self.width_in_mbs_minus1).ue(self.height_in_map_units_minus1);
            b.put(u64::from(self.frame_mbs_only), 1);
            if !self.frame_mbs_only {
                b.put(0, 1); // mb_adaptive_frame_field_flag
            }
            b.put(1, 1); // direct_8x8_inference_flag
            b.put(u64::from(self.crop.is_some()), 1);
            for offset in self.crop.into_iter().flatten() {
                b.ue(offset);
            }
            b.put(0, 1); // vui_parameters_present_flag
            b.done()
        }

        /// The NAL unit, as a stream carries it.
        pub(crate) fn nal(&self) -> Vec<u8> {
            escaped([vec![0x67], self.rbsp()].concat(), 1)
        }

        /// [`Self::nal`] behind a start code, a PPS behind it.
        pub(crate) fn annex_b(&self) -> Vec<u8> {
            [&[0, 0, 0, 1], &self.nal()[..], &[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]].concat()
        }
    }

    /// The four SPS of edge#213. The first two state a size that does not fit
    /// 32 bits; the third a crop, and the fourth a `delta_scale`, that does
    /// not.
    pub(crate) fn edge_213() -> [Sps; 4] {
        [
            Sps { width_in_mbs_minus1: 0x0FFF_FFFF, ..Sps::default() },
            Sps { height_in_map_units_minus1: 0x07FF_FFFF, frame_mbs_only: false, ..Sps::default() },
            Sps { crop: Some([0x8000_0000, 0x8000_0000, 0, 0]), ..Sps::default() },
            Sps { profile: 100, scaling_list: Some(&[i32::MAX]), ..Sps::default() },
        ]
    }

    /// Sizes at their bounds, as `(SPS, the size of it where it is cropped by
    /// 2x2 units)`; every one is 4:2:0 progressive or not cropped, so that is
    /// every parser's answer.
    pub(crate) fn sizes() -> [(Sps, Option<(u32, u32)>); 15] {
        let d = Sps::default;
        [
            (d(), Some((1920, 1088))),
            (Sps { crop: Some([0, 0, 0, 4]), ..d() }, Some((1920, 1080))),
            (Sps { height_in_map_units_minus1: 33, frame_mbs_only: false, ..d() }, Some((1920, 1088))),
            (Sps { width_in_mbs_minus1: 0, height_in_map_units_minus1: 0, ..d() }, Some((16, 16))),
            (Sps { width_in_mbs_minus1: 1023, height_in_map_units_minus1: 1023, ..d() }, Some((16384, 16384))),
            (Sps { height_in_map_units_minus1: 511, frame_mbs_only: false, ..d() }, Some((1920, 16384))),
            (Sps { width_in_mbs_minus1: 1024, ..d() }, None),
            (Sps { height_in_map_units_minus1: 1024, ..d() }, None),
            (Sps { height_in_map_units_minus1: 512, frame_mbs_only: false, ..d() }, None),
            // 80016 px, which fit neither a `u16` nor a 16.16 `tkhd`; the last
            // code there is; and a size the crop would bring back under.
            (Sps { width_in_mbs_minus1: 5000, ..d() }, None),
            (Sps { width_in_mbs_minus1: u32::MAX - 1, ..d() }, None),
            (Sps { width_in_mbs_minus1: 1024, crop: Some([0, 8, 0, 0]), ..d() }, None),
            (Sps { height_in_map_units_minus1: u32::MAX - 1, frame_mbs_only: false, ..d() }, None),
            // These wrap to 1920 and to 1088.
            (Sps { width_in_mbs_minus1: 0x1000_0077, ..d() }, None),
            (Sps { height_in_map_units_minus1: 0x0800_0021, frame_mbs_only: false, ..d() }, None),
        ]
    }

    /// Crops of 1920x1088 by 2x2 units at their bounds.
    pub(crate) fn crops() -> [(Sps, Option<(u32, u32)>); 8] {
        [
            ([959, 0, 0, 543], Some((2, 2))),
            ([480, 479, 272, 271], Some((2, 2))),
            ([480, 480, 0, 0], None),
            ([0, 0, 0, 544], None),
            ([0, 961, 0, 0], None),
            ([0, 0, 545, 0], None),
            // Twice these wraps to 0; and the sum of these does, to 0 again.
            ([0x8000_0000, 0, 0, 0], None),
            ([0, 0, u32::MAX - 1, 2], None),
        ]
        .map(|(crop, size)| (Sps { crop: Some(crop), ..Sps::default() }, size))
    }

    /// An SPS of each chroma format, frame and field coded, with every scaling
    /// list spelt out and a crop of 3 units across and 7 down.
    pub(crate) fn high_sps() -> [(Sps, (u32, u32)); 8] {
        [
            (0, true, (1917, 1081)),
            (0, false, (1917, 1074)),
            (1, true, (1914, 1074)),
            (1, false, (1914, 1060)),
            (2, true, (1914, 1081)),
            (2, false, (1914, 1074)),
            (3, true, (1917, 1081)),
            (3, false, (1917, 1074)),
        ]
        .map(|(chroma, frame_mbs_only, size)| {
            let height_in_map_units_minus1 = if frame_mbs_only { 67 } else { 33 };
            let crop = Some([1, 2, 3, 4]);
            let d = Sps::default();
            (Sps { profile: 244, chroma, flat_lists: true, height_in_map_units_minus1, frame_mbs_only, crop, ..d }, size)
        })
    }

    /// Real SPS NAL units, x264's and two captures', as `(what, SPS, size)`,
    /// the size as ffprobe reads it.
    pub(crate) fn real_sps() -> [(&'static str, Vec<u8>, (u32, u32)); 11] {
        [
            ("Baseline 720p", "6742c01fdc05005bb011000003000100000300320f183380", (1280, 720)),
            ("Main, interlaced SD", "674d401ef816849b011000000300100000030321f142b8", (720, 576)),
            ("Main, interlaced 1440", "674d4028f80b4089fbc044000003000400000300c87c58be", (1440, 1080)),
            ("High, PAFF capture", "67640029ac133140780447de03ea020203e000000300200000065280", (1920, 1080)),
            (
                "High, MBAFF capture with scaling lists",
                concat!(
                    "67640028ad843588742050b041074421ac47361882a210ffa69a4d5a31526c862c92c8a431cb104111251c",
                    "28c716161d1e09050b05d06d087fd7af93f11fd7e6f8af0870d608a0a0b290078044fde036a02020280000",
                    "0300080000030194a0",
                ),
                (1920, 1080),
            ),
            ("High 4:2:2 10-bit, interlaced", "677a1028b6cf01e0113f2e022000000300200000064280", (1920, 1080)),
            ("High 4:2:2 10-bit 4K", "677a1033b6cb80780087d80880000003008000001902", (3840, 2160)),
            ("High 10 8K", "676e103ca6cb803c0010ed80880000030008000003019020", (7680, 4320)),
            ("High 4:2:2, 8192 wide", "677a103cbcb801000043b6022000000300200000064080", (8192, 4320)),
            ("16384 wide, the widest taken", "6742c03cdc0040011b011000000300100000030320f0804380", (16384, 128)),
            ("High 4:4:4, cropped by 14 each way", "67f4100d919705421e3e3f01100000030010000003032040", (322, 242)),
        ]
        .map(|(what, sps, size)| (what, hx(sps), size))
    }

    /// `n` SPS NAL units of random bytes, the same ones each time. Most name a
    /// profile the parsers know, and most hold a run or three of 27 to 31
    /// zeros, which is what a code near the top of ue(v)'s range starts with.
    pub(crate) fn random_sps(n: usize) -> impl Iterator<Item = Vec<u8>> {
        const PROFILES: [u8; 8] = [66, 77, 88, 100, 110, 122, 244, 44];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..n).map(move |_| {
            let shape = random();
            let mut nal = vec![0x67];
            while nal.len() < 1 + (shape % 56) as usize {
                // Ones weighted up in every other one: short codes, so the
                // parser is still reading where the runs are.
                let word = random() | if shape & 0x100 == 0 { random() } else { 0 };
                nal.extend(word.to_be_bytes());
            }
            if nal.len() > 4 && shape & 0xE00 != 0 {
                nal[1] = PROFILES[(shape >> 12) as usize % PROFILES.len()];
            }
            for _ in 0..(shape >> 16) % 4 {
                let at = random();
                let from = 32 + (at % 256) as usize;
                for bit in from..from + 27 + (at >> 32) as usize % 5 {
                    if let Some(byte) = nal.get_mut(bit / 8) {
                        *byte &= !(0x80 >> (bit % 8));
                    }
                }
            }
            nal
        })
    }

    /// How many of [`random_sps`] each parser is given. Until 2026-11 each of
    /// them panicked on some, under overflow checks.
    pub(crate) const RANDOM: usize = 100_000;

    #[test]
    fn a_size_is_bounded_and_a_crop_leaves_a_picture() {
        for (sps, size) in sizes().into_iter().chain(crops()) {
            let crop = sps.crop.map_or((0, 0, 0, 0), |[l, r, t, b]| (l, r, t, b));
            let (w, h) = (sps.width_in_mbs_minus1, sps.height_in_map_units_minus1);
            assert_eq!(picture_size(w, h, sps.frame_mbs_only, crop, (2, 2)), size, "{w} {h} {crop:?}");
        }
        // No code is this large, and none of it is taken to be 0.
        assert_eq!(picture_size(u32::MAX, 67, true, (0, 0, 0, 0), (2, 2)), None);
        assert_eq!(picture_size(119, 67, true, (0, u32::MAX, 0, 0), (1, 1)), None);
        // Field-coded 4:2:0: a column of it crops two, a row four lines.
        assert_eq!(picture_size(119, 33, false, (1, 0, 0, 2), (2, 4)), Some((1918, 1080)));
        assert_eq!(picture_size(119, 33, false, (0, 0, 0, 272), (2, 4)), None);
        assert_eq!(picture_size(119, 67, true, (0, 1, 0, 1), (1, 1)), Some((1919, 1087)));
    }

    #[test]
    fn a_scaling_list_is_skipped_delta_by_delta() {
        let read = |size, deltas: &[i32]| {
            let mut left = deltas.iter().copied();
            skip_scaling_list(size, || left.next()).map(|()| left.len())
        };
        assert_eq!(read(16, &[0; 20]), Some(4), "spelt out");
        assert_eq!(read(64, &[0; 64]), Some(0));
        assert_eq!(read(16, &[0; 15]), None, "cut short");
        assert_eq!(read(16, &[-8, 1, 1]), Some(2), "ended by its first delta");
        // 8 + 127 + 127 wraps to 6, 6 - 6 ends it; 8 - 128 to 136.
        assert_eq!(read(16, &[127, 127, -6, 1]), Some(1));
        assert_eq!(read(16, &[-128, -128, -8, 1]), Some(1));
        for delta in [-129, 128, i32::MIN, i32::MAX] {
            assert_eq!(read(16, &[delta; 16]), None, "{delta}");
            assert_eq!(read(16, &[1, delta, -8]), None, "{delta}");
        }
    }
}
