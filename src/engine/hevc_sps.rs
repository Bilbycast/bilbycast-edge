// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The picture size the HEVC SPS parsers share, as [`super::h264_sps`] holds
//! the H.264 ones'.
//!
//! An SPS out of range gets the answer each parser has for one whose size it
//! cannot read: media analysis takes no resolution, profile, level or frame
//! rate off it, content analysis signalling has no aspect ratio, and the
//! media library probe looks at the next. Until 2026-11 (edge#213) the three
//! cropped the size unchecked, and media analysis cut it to 16 bits first:
//! such a size wrapped in a release build and panicked a build with overflow
//! checks.

use super::h264_sps::cropped;

/// The picture's width and height once cropped to the conformance window
/// (ITU-T H.265 §7.4.3.2.1). `conf_win` is the four `conf_win_*_offset` in
/// the SPS's own order (left, right, top, bottom), and `crop_unit` the luma
/// samples one of them is, across and down. `None` for a size over
/// [`super::h264_sps::MAX_DIMENSION`] and for a window that leaves no
/// picture.
pub fn picture_size(
    pic_width_in_luma_samples: u32,
    pic_height_in_luma_samples: u32,
    conf_win: (u32, u32, u32, u32),
    crop_unit: (u32, u32),
) -> Option<(u32, u32)> {
    Some((
        cropped(pic_width_in_luma_samples, (conf_win.0, conf_win.1), crop_unit.0)?,
        cropped(pic_height_in_luma_samples, (conf_win.2, conf_win.3), crop_unit.1)?,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::cmaf::param_sets::tests::{Bits, escaped, hx};

    /// An HEVC SPS, 4:2:0 and of one sub-layer.
    #[derive(Clone, Copy)]
    pub(crate) struct Sps {
        pub width: u32,
        pub height: u32,
        /// Left, right, top, bottom.
        pub conf_win: Option<[u32; 4]>,
        pub log2_max_poc_lsb_minus4: u32,
        /// `num_short_term_ref_pic_sets`; the first 65 are written.
        pub ref_pic_sets: u32,
        /// `(num_negative_pics, num_positive_pics)` of each set; the first 17
        /// of either are written.
        pub pics: (u32, u32),
        /// `num_long_term_ref_pics_sps`, the first 33 written; `None` where
        /// there are no long-term pictures.
        pub long_term_pics: Option<u32>,
        /// A VUI that states 25 frames a second.
        pub vui: bool,
    }

    /// 1920x1080, not cropped, 25 fps, no reference picture sets.
    impl Default for Sps {
        fn default() -> Self {
            Self {
                width: 1920,
                height: 1080,
                conf_win: None,
                log2_max_poc_lsb_minus4: 4,
                ref_pic_sets: 0,
                pics: (1, 0),
                long_term_pics: None,
                vui: true,
            }
        }
    }

    impl Sps {
        /// The NAL unit, as a stream carries it.
        pub(crate) fn nal(&self) -> Vec<u8> {
            let mut b = Bits::new(&[0x42, 0x01, 0x01]);
            b.put(0x01, 8).put(0x6000_0000, 32).put(0x9000_0000_0000, 48).put(120, 8); // PTL
            b.ue(0).ue(1).ue(self.width).ue(self.height);
            b.put(u64::from(self.conf_win.is_some()), 1);
            for offset in self.conf_win.into_iter().flatten() {
                b.ue(offset);
            }
            b.ue(0).ue(0).ue(self.log2_max_poc_lsb_minus4);
            b.put(1, 1).ue(4).ue(2).ue(0); // sub-layer ordering
            b.ue(0).ue(3).ue(0).ue(3).ue(0).ue(0); // block sizes
            b.put(0, 4); // no scaling lists, AMP, SAO or PCM
            b.ue(self.ref_pic_sets);
            for set in 0..self.ref_pic_sets.min(65) {
                if set > 0 {
                    b.put(0, 1); // inter_ref_pic_set_prediction_flag
                }
                let (negative, positive) = self.pics;
                b.ue(negative).ue(positive);
                for _ in 0..negative.min(17) + positive.min(17) {
                    b.ue(0).put(1, 1);
                }
            }
            b.put(u64::from(self.long_term_pics.is_some()), 1);
            if let Some(pics) = self.long_term_pics {
                b.ue(pics);
                for _ in 0..pics.min(33) {
                    b.put(0, self.log2_max_poc_lsb_minus4.min(13) + 4).put(0, 1);
                }
            }
            b.put(0, 2); // no temporal MVP or strong intra smoothing
            b.put(u64::from(self.vui), 1);
            if self.vui {
                b.put(0, 8).put(1, 1).put(1, 32).put(25, 32).put(0, 3);
            }
            b.put(0, 1); // sps_extension_present_flag
            escaped(b.done(), 2)
        }

        /// [`Self::nal`] behind a start code, a PPS behind it.
        pub(crate) fn annex_b(&self) -> Vec<u8> {
            [&[0, 0, 0, 1], &self.nal()[..], &[0, 0, 0, 1, 0x44, 0x01, 0xC0, 0x73]].concat()
        }
    }

    /// The SPS of edge#213 that states as many reference picture sets as a
    /// code can. Media analysis asked for 16 GiB to hold their counts, and the
    /// process ended where it could not have it.
    pub(crate) fn edge_213() -> Sps {
        Sps { ref_pic_sets: u32::MAX - 1, ..Sps::default() }
    }

    /// Sizes at their bounds, as `(SPS, its size)`.
    pub(crate) fn sizes() -> [(Sps, Option<(u32, u32)>); 14] {
        let d = Sps::default;
        [
            (d(), Some((1920, 1080))),
            (Sps { height: 1088, conf_win: Some([0, 0, 0, 4]), ..d() }, Some((1920, 1080))),
            (Sps { width: 1, height: 1, ..d() }, Some((1, 1))),
            (Sps { width: 16_384, height: 16_384, ..d() }, Some((16_384, 16_384))),
            (Sps { width: 0, ..d() }, None),
            (Sps { height: 0, ..d() }, None),
            (Sps { width: 16_385, ..d() }, None),
            (Sps { height: 16_385, ..d() }, None),
            // The last code there is, and a size the crop would bring back
            // under.
            (Sps { width: u32::MAX - 1, ..d() }, None),
            (Sps { height: u32::MAX - 1, ..d() }, None),
            (Sps { width: 16_400, conf_win: Some([0, 8, 0, 0]), ..d() }, None),
            // 4294967294 less twice 2147482687 is 1920.
            (Sps { width: u32::MAX - 1, conf_win: Some([0, 0x7FFF_FC3F, 0, 0]), ..d() }, None),
            // These cut to 16 bits are 1920 and 1080.
            (Sps { width: 0x1_0780, ..d() }, None),
            (Sps { height: 0x1_0438, ..d() }, None),
        ]
    }

    /// Crops of 1920x1080 by 2x2 units at their bounds.
    pub(crate) fn crops() -> [(Sps, Option<(u32, u32)>); 8] {
        [
            ([959, 0, 0, 539], Some((2, 2))),
            ([480, 479, 270, 269], Some((2, 2))),
            ([480, 480, 0, 0], None),
            ([0, 0, 0, 540], None),
            ([0, 961, 0, 0], None),
            ([0, 0, 541, 0], None),
            // Twice these wraps to 0; and the sum of these does.
            ([0x8000_0000, 0, 0, 0], None),
            ([0, 0, u32::MAX - 1, 2], None),
        ]
        .map(|(conf_win, size)| (Sps { conf_win: Some(conf_win), ..Sps::default() }, size))
    }

    /// Real SPS NAL units, encoders' and captures', as `(what, SPS, size,
    /// frames a second)`, the size and rate as ffprobe reads them.
    pub(crate) fn real_sps() -> [(&'static str, Vec<u8>, (u32, u32), f64); 15] {
        [
            (
                "x265 1080p",
                "420101016000000300900000030000030078a003c0801107cb965654a4c2f0168080000003008000000c84",
                (1920, 1080),
                25.0,
            ),
            (
                "x265 1366x768",
                "420101016000000300900000030000030078a002b080301cde595952930bc05a020000030002000003003210",
                (1366, 768),
                25.0,
            ),
            (
                "x265 8192 wide",
                "4201010160000003009000000300000300b4a00040020010e16595952930bc05a02000000300200000030321",
                (8192, 4320),
                25.0,
            ),
            (
                "x265 29.97",
                "420101016000000300900000030000030078a003c0801107cb965654a4c2f01680800001f480003a9804",
                (1920, 1080),
                30_000.0 / 1001.0,
            ),
            (
                "x265, a default display window",
                "420101016000000300900000030000030078a003c0801107cb965654a4c2f01f408b0f85414c0000030004000003006420",
                (1920, 1080),
                25.0,
            ),
            (
                "x265 HDR10 4K, an HRD",
                concat!(
                    "420101022000000300900000030000030096a001e020021c4d965654a4c2f016a12201228000000300800000",
                    "0cb0924b2f0000989680004c4b44",
                ),
                (3840, 2160),
                25.0,
            ),
            (
                "x265 4:0:0",
                "4201010408000003009fc800000300003cc02a20101c7c7e595952930bc05b020000030002000003003210",
                (322, 242),
                25.0,
            ),
            (
                "x265 4:2:2 10-bit",
                "4201010408000003009d0800000300003cb00a880407111ed965654a4c2f016808000003000800000300c840",
                (322, 242),
                25.0,
            ),
            (
                "x265 4:4:4",
                "4201010408000003009e0800000300003c9001510080e3e3f2caca94985e02d0100000030010000003019080",
                (322, 242),
                25.0,
            ),
            (
                "x265, five sub-layers",
                concat!(
                    "42010801600000030090000003000003003c0000a00a080f16595844ac22561131022728452930bc05a02000",
                    "000300200000030321",
                ),
                (320, 240),
                25.0,
            ),
            (
                "x265 720p50, 11 reference picture sets",
                concat!(
                    "420101016000000300900000030000030078a00280802d165959529301845aae6f5d444aa4a8b2555444aaaa",
                    "2495535ae4fd492a949116a4ef0168080000030008000003019040",
                ),
                (1280, 720),
                50.0,
            ),
            (
                "VAAPI 720p",
                concat!(
                    "420101016000000300b0000003000003005da00280802d1624d6491b6ac05a0200000300020000030033af84",
                    "0208",
                ),
                (1280, 720),
                25.0,
            ),
            (
                "TNT 2160p50 HLG capture",
                concat!(
                    "420101022000000300b00000030000030099a001e020021c4d94d08b9084dffff9220b087fffffffffffffff",
                    "ffffffffffffffff90820b087fffffffffffffffffffffffffffffffe42082c21fffffffffffffffe42f016a",
                    "1220122800000300080000030196f898f1e8",
                ),
                (3840, 2160),
                50.0,
            ),
            (
                "Russian DTTV capture, fields of 576i",
                concat!(
                    "42010101600000030060000003000003005aa005a201216343066491be10fffe45087fff22843fffffffffff",
                    "ffff91421fffffffffffffffc8a10ffffffffffffffff22843fffffffffffffffc8a10ffffffffffffffff84",
                    "3fffffffffffffffd5e088418181bbfc000003000400000300c820",
                ),
                (720, 288),
                50.0,
            ),
            (
                "Russian DTTV capture, fields of 1080i, Main 10",
                concat!(
                    "42010102200000030060000003000003007ba003c080221f6d8d0c199246f843fff91421fffc8a10ffffffff",
                    "fffffffe45087fffffffffffffff22843fffffffffffffffc8a10ffffffffffffffff22843ffffffffffffff",
                    "fe10ffffffffffffffff5780a1010101eff00000030010000003032080",
                ),
                (1920, 540),
                50.0,
            ),
        ]
        .map(|(what, sps, size, rate)| (what, hx(sps), size, rate))
    }

    /// The SPS of [`real_sps`] that are not 4:2:0, pictures of 322x242, as
    /// `(what, SPS, size)`: the size as the probe and signalling read it. They
    /// count a crop in units of 2x2 luma samples whatever the chroma format.
    pub(crate) fn cropped_by_2x2() -> [(&'static str, Vec<u8>, (u32, u32)); 3] {
        [("x265 4:0:0", (308, 228)), ("x265 4:2:2 10-bit", (322, 228)), ("x265 4:4:4", (308, 228))].map(
            |(of, size)| {
                let (what, sps, ..) = real_sps().into_iter().find(|(what, ..)| *what == of).unwrap();
                (what, sps, size)
            },
        )
    }

    /// xorshift64, the same numbers each time.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// One of 0..=`max`.
        fn up_to(&mut self, max: u32) -> u32 {
            (self.next() >> 32) as u32 % (max + 1)
        }
    }

    /// An SPS being written, one of whose ue(v) codes may be another than the
    /// one given.
    struct Writer {
        bits: Bits,
        codes: u32,
        spoilt: Option<(u32, u32)>,
    }

    impl Writer {
        fn ue(&mut self, v: u32) {
            let v = match self.spoilt {
                Some((at, code)) if at == self.codes => code,
                _ => v,
            };
            self.codes += 1;
            self.bits.ue(v);
        }
        fn put(&mut self, v: u32, width: u32) {
            self.bits.put(u64::from(v), width);
        }
    }

    /// An SPS of random fields, each in its range (§7.4.3.2.1) but `spoilt`,
    /// as `(which ue(v) code of it, the code)`. Returns the NAL unit and how
    /// many ue(v) codes it holds.
    fn random_fields(seed: u64, spoilt: Option<(u32, u32)>) -> (Vec<u8>, u32) {
        let mut r = Random(seed);
        let mut w = Writer { bits: Bits::new(&[0x42, 0x01]), codes: 0, spoilt };
        let sub_layers = if r.up_to(2) == 0 { r.up_to(6) } else { 0 };
        w.put(0, 4);
        w.put(sub_layers, 3);
        w.put(1, 1);
        for width in [8, 32, 16, 32, 8] {
            w.put(r.next() as u32, width); // profile_tier_level
        }
        let present = [(); 6].map(|()| (r.up_to(1), r.up_to(1)));
        let present = &present[..sub_layers as usize];
        for &(profile, level) in present {
            w.put(profile, 1);
            w.put(level, 1);
        }
        if sub_layers > 0 {
            w.put(0, 2 * (8 - sub_layers));
        }
        for &(profile, level) in present {
            for width in if profile == 1 { &[8, 32, 16, 32][..] } else { &[] } {
                w.put(r.next() as u32, *width);
            }
            w.put(r.next() as u32, 8 * level);
        }
        w.ue(r.up_to(15));
        let chroma = r.up_to(3);
        w.ue(chroma);
        if chroma == 3 {
            w.put(r.up_to(1), 1);
        }
        w.ue(64 + r.up_to(16_320));
        w.ue(64 + r.up_to(16_320));
        let cropped = r.up_to(1);
        w.put(cropped, 1);
        for _ in 0..4 * cropped {
            w.ue(r.up_to(15));
        }
        w.ue(r.up_to(8));
        w.ue(r.up_to(8));
        let lsb_bits = r.up_to(12);
        w.ue(lsb_bits);
        let every_sub_layer = r.up_to(1);
        w.put(every_sub_layer, 1);
        for _ in 0..1 + every_sub_layer * sub_layers {
            w.ue(r.up_to(15));
            w.ue(r.up_to(15));
            w.ue(r.up_to(15));
        }
        for max in [3, 3, 3, 3, 4, 4] {
            w.ue(r.up_to(max)); // block sizes
        }
        let scaling_lists = r.up_to(1);
        w.put(scaling_lists, 1);
        if scaling_lists == 1 {
            let spelt_out = r.up_to(1);
            w.put(spelt_out, 1);
            for size in (0..4).filter(|_| spelt_out == 1) {
                for _ in 0..if size == 3 { 2 } else { 6 } {
                    let coded = r.up_to(1);
                    w.put(coded, 1);
                    let codes = match (coded, size) {
                        (0, _) => 1, // scaling_list_pred_matrix_id_delta
                        (_, 0) => 16,
                        (_, 1) => 64,
                        _ => 65,
                    };
                    for _ in 0..codes {
                        w.ue(r.up_to(4));
                    }
                }
            }
        }
        w.put(r.up_to(3), 2);
        let pcm = r.up_to(1);
        w.put(pcm, 1);
        if pcm == 1 {
            w.put(r.up_to(255), 8);
            w.ue(r.up_to(2));
            w.ue(r.up_to(2));
            w.put(r.up_to(1), 1);
        }
        let sets = if r.up_to(7) == 0 { 64 } else { r.up_to(8) };
        w.ue(sets);
        let mut delta_pocs = 0;
        for set in 0..sets {
            let predicted = if set > 0 { r.up_to(1) } else { 0 };
            w.put(predicted, u32::from(set > 0));
            if predicted == 1 {
                // From the set before it, as every one in an SPS is.
                w.put(r.up_to(1), 1);
                w.ue(r.up_to(7));
                let from = delta_pocs;
                delta_pocs = 0;
                for _ in 0..=from {
                    let used = r.up_to(1);
                    w.put(used, 1);
                    let kept = if used == 1 { 1 } else { r.up_to(1) };
                    w.put(kept, 1 - used);
                    delta_pocs += kept;
                }
            } else {
                let negative = r.up_to(8);
                let positive = r.up_to(7);
                w.ue(negative);
                w.ue(positive);
                for _ in 0..negative + positive {
                    w.ue(r.up_to(7));
                    w.put(r.up_to(1), 1);
                }
                delta_pocs = negative + positive;
            }
        }
        let long_term = r.up_to(1);
        w.put(long_term, 1);
        if long_term == 1 {
            let pics = r.up_to(32);
            w.ue(pics);
            for _ in 0..pics {
                w.put(r.next() as u32, lsb_bits + 4);
                w.put(r.up_to(1), 1);
            }
        }
        w.put(r.up_to(3), 2);
        w.put(1, 1); // vui_parameters_present_flag
        let extended_sar = r.up_to(1);
        w.put(extended_sar, 1);
        if extended_sar == 1 {
            w.put(255, 8);
            w.put(r.next() as u32, 32);
        }
        w.put(0, 7);
        w.put(1, 1);
        w.put(r.up_to(2), 32);
        w.put(r.next() as u32, 32);
        w.put(0, 4);
        (escaped(w.bits.done(), 2), w.codes)
    }

    /// `n` SPS NAL units, the same ones each time: one in four of random
    /// bytes, the others in range but for one code of theirs, which goes up
    /// to the last there is.
    pub(crate) fn random_sps(n: usize) -> impl Iterator<Item = Vec<u8>> {
        let mut r = Random(0x9E37_79B9_7F4A_7C15);
        (0..n).map(move |i| {
            let seed = r.next();
            if i % 4 == 0 {
                let mut nal = vec![0x42, 0x01, (r.next() & 0x0F) as u8];
                for _ in 0..r.up_to(12) {
                    // Ones weighted up in every other one: short codes, so
                    // the parser is still reading where the long ones are.
                    let word = r.next() | if seed & 1 == 0 { r.next() } else { 0 };
                    nal.extend(word.to_be_bytes());
                }
                return nal;
            }
            let (_, codes) = random_fields(seed, None);
            let code = ((r.next() >> 32) as u32 >> r.up_to(31)).min(u32::MAX - 1);
            random_fields(seed, Some((r.up_to(codes - 1), code))).0
        })
    }

    /// How many of [`random_sps`] each parser is given. Until 2026-11 media
    /// analysis, signalling and the probe panicked on some, under overflow
    /// checks.
    pub(crate) const RANDOM: usize = 4_000;

    #[test]
    fn a_size_is_bounded_and_a_crop_leaves_a_picture() {
        for (sps, size) in sizes().into_iter().chain(crops()) {
            let crop = sps.conf_win.map_or((0, 0, 0, 0), |[l, r, t, b]| (l, r, t, b));
            let (w, h) = (sps.width, sps.height);
            assert_eq!(picture_size(w, h, crop, (2, 2)), size, "{w} {h} {crop:?}");
        }
        // No code is this large, and none of it is taken to be 0.
        assert_eq!(picture_size(u32::MAX, 1080, (0, 0, 0, 0), (2, 2)), None);
        assert_eq!(picture_size(1920, 1080, (0, u32::MAX, 0, 0), (1, 1)), None);
        // Each edge by its own unit: 4:2:2 and 4:4:4.
        assert_eq!(picture_size(1920, 1088, (1, 2, 3, 4), (2, 1)), Some((1914, 1081)));
        assert_eq!(picture_size(1920, 1088, (1, 2, 3, 4), (1, 1)), Some((1917, 1081)));
    }
}
