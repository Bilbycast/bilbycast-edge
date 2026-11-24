// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the HEVC SPS parsers share, as [`super::h264_sps`] holds the H.264
//! ones': the picture's size, and the way past the sub-layers in front of it
//! and the reference picture sets between it and the VUI.
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

/// Skip what a `profile_tier_level` says of the sub-layers (§7.3.3),
/// `read_bits(n)` reading the next n bits. `None` where that does.
///
/// Until 2026-11 content analysis signalling, the media library probe and the
/// PES splice sentinel skipped 112 bits for a profile, which is 88, and CMAF's
/// track size skipped neither profiles nor levels: each read the picture size
/// of an SPS that has them from the wrong bits.
pub fn skip_sub_layers(
    max_sub_layers_minus1: u32,
    mut read_bits: impl FnMut(u8) -> Option<u32>,
) -> Option<()> {
    if max_sub_layers_minus1 == 0 {
        return Some(());
    }
    // Eight times profile_present and level_present, reserved past the last
    // sub-layer.
    let present = read_bits(16)?;
    for pair in (0..8).rev().take(max_sub_layers_minus1 as usize) {
        let present = present >> (2 * pair);
        if present & 2 != 0 {
            // Space, tier and idc, compatibility flags, constraint flags.
            for width in [8, 32, 32, 16] {
                read_bits(width)?;
            }
        }
        if present & 1 != 0 {
            read_bits(8)?; // sub_layer_level_idc
        }
    }
    Some(())
}

/// Skip an SPS's reference picture sets (§7.3.2.2.1), from
/// `num_short_term_ref_pic_sets` to the last long-term picture, `bits` and
/// `ue` reading the next u(n) and ue(v) from `reader`. `None` where those do,
/// and for a count or a `log2_max_pic_order_cnt_lsb_minus4` out of range.
pub fn skip_ref_pic_sets<R>(
    reader: &mut R,
    log2_max_pic_order_cnt_lsb_minus4: u32,
    bits: impl Fn(&mut R, u8) -> Option<u32>,
    ue: impl Fn(&mut R) -> Option<u32>,
) -> Option<()> {
    if log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return None; // §7.4.3.2.1
    }
    let lsb_bits = log2_max_pic_order_cnt_lsb_minus4 as u8 + 4;
    // At most 64 (§7.4.3.2.1), and nothing is allocated by the count. Until
    // 2026-11 (edge#213) media analysis did: 16 GiB for 2^32 − 2, and the
    // process ended where it could not have it.
    let sets = ue(reader).filter(|sets| *sets <= 64)?;
    // The delta POCs of the set before, which a predicted set (§7.3.7) names
    // in this order: the negative ones, then the positive, each nearest first.
    let mut before: Vec<i64> = Vec::new();
    for set in 0..sets {
        let mut delta_pocs = Vec::new();
        if set > 0 && bits(reader, 1)? == 1 {
            // RefRpsIdx is the set before: `delta_idx_minus1` is in a slice
            // header alone. Until 2026-11 media analysis read one here, and
            // so every SPS with a predicted set from the wrong bits on.
            let negative = bits(reader, 1)? == 1; // delta_rps_sign
            let delta_rps = i64::from(ue(reader)?) + 1;
            let delta_rps = if negative { -delta_rps } else { delta_rps };
            // One more than the set has: the picture it was the set of.
            for delta_poc in before.into_iter().chain([0]) {
                // used_by_curr_pic_flag, else use_delta_flag.
                let kept = bits(reader, 1)? == 1 || bits(reader, 1)? == 1;
                // (7-61), (7-62): an entry that lands on the picture this is
                // the set of is in neither half.
                if kept && delta_poc + delta_rps != 0 {
                    delta_pocs.push(delta_poc + delta_rps);
                }
            }
            delta_pocs.sort_unstable_by_key(|delta_poc| (*delta_poc > 0, delta_poc.abs()));
        } else {
            // A decoder holds at most 16 pictures (MaxDpbSize, §A.4.2).
            let negative = ue(reader).filter(|pics| *pics <= 16)?;
            let positive = ue(reader).filter(|pics| *pics <= 16)?;
            for (pics, sign) in [(negative, -1), (positive, 1)] {
                let mut delta_poc = 0;
                for _ in 0..pics {
                    delta_poc += sign * (i64::from(ue(reader)?) + 1);
                    bits(reader, 1)?; // used_by_curr_pic_s0_flag, or s1
                    delta_pocs.push(delta_poc);
                }
            }
        }
        before = delta_pocs;
    }
    if bits(reader, 1)? == 1 {
        // long_term_ref_pics_present_flag; at most 32 (§7.4.3.2.1).
        for _ in 0..ue(reader).filter(|pics| *pics <= 32)? {
            bits(reader, lsb_bits)?; // lt_ref_pic_poc_lsb_sps
            bits(reader, 1)?; // used_by_curr_pic_lt_sps_flag
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::cmaf::param_sets::tests::{Bits, escaped, hx};
    use crate::engine::content_analysis::bitreader::BitReader;

    /// An HEVC SPS, 4:2:0.
    #[derive(Clone, Copy)]
    pub(crate) struct Sps {
        /// `(sub_layer_profile_present_flag, sub_layer_level_present_flag)`
        /// of each sub-layer but the highest.
        pub sub_layers: &'static [(bool, bool)],
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
        pub vui: bool,
        /// What the VUI states of it: `(vui_num_units_in_tick,
        /// vui_time_scale)`.
        pub timing: (u32, u32),
    }

    /// 1920x1080, not cropped, 25 fps, one sub-layer, no reference picture
    /// sets.
    impl Default for Sps {
        fn default() -> Self {
            Self {
                sub_layers: &[],
                width: 1920,
                height: 1080,
                conf_win: None,
                log2_max_poc_lsb_minus4: 4,
                ref_pic_sets: 0,
                pics: (1, 0),
                long_term_pics: None,
                vui: true,
                timing: (1, 25),
            }
        }
    }

    impl Sps {
        /// The NAL unit, as a stream carries it.
        pub(crate) fn nal(&self) -> Vec<u8> {
            let mut b = Bits::new(&[0x42, 0x01]);
            b.put(0, 4).put(self.sub_layers.len() as u64, 3).put(1, 1);
            b.put(0x01, 8).put(0x6000_0000, 32).put(0x9000_0000_0000, 48).put(120, 8); // PTL
            for &(profile, level) in self.sub_layers {
                b.put(u64::from(profile), 1).put(u64::from(level), 1);
            }
            if !self.sub_layers.is_empty() {
                b.put(0, 16 - 2 * self.sub_layers.len() as u32);
            }
            for &(profile, level) in self.sub_layers {
                if profile {
                    b.put(0x01, 8).put(0x6000_0000, 32).put(0x9000_0000_0000, 48);
                }
                if level {
                    b.put(93, 8);
                }
            }
            b.ue(0).ue(1).ue(self.width).ue(self.height);
            b.put(u64::from(self.conf_win.is_some()), 1);
            for offset in self.conf_win.into_iter().flatten() {
                b.ue(offset);
            }
            b.ue(0).ue(0).ue(self.log2_max_poc_lsb_minus4);
            b.put(1, 1); // sps_sub_layer_ordering_info_present_flag
            for _ in 0..=self.sub_layers.len() {
                b.ue(4).ue(2).ue(0);
            }
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
                b.put(0, 8).put(1, 1);
                b.put(u64::from(self.timing.0), 32).put(u64::from(self.timing.1), 32).put(0, 3);
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

    /// SPS of 720x576 at every kind of sub-layer: none, and one to six with
    /// the profile, the level, both and neither of each there.
    pub(crate) fn sub_layered() -> impl Iterator<Item = Sps> {
        const KINDS: [[(bool, bool); 6]; 5] = [
            [(false, false); 6],
            [(false, true); 6],
            [(true, false); 6],
            [(true, true); 6],
            [(false, true), (true, false), (true, true), (false, false), (true, false), (false, true)],
        ];
        KINDS.iter().flat_map(|kind| {
            (0..=6).map(|n| Sps { sub_layers: &kind[..n], width: 720, height: 576, ..Sps::default() })
        })
    }

    /// Real SPS NAL units, encoders', captures' and JCT-VC conformance
    /// streams', as `(what, SPS, size, frames a second)`, the size and rate as
    /// ffprobe reads them. No rate where the SPS states none.
    pub(crate) fn real_sps() -> [(&'static str, Vec<u8>, (u32, u32), Option<f64>); 25] {
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
                "TNT 2160p50 PQ capture",
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
            (
                "Quick Sync 720p50, 3 of 4 reference picture sets predicted",
                concat!(
                    "420101014000000300900000030000030078a00280802d16c479246d8a9fa69ccb64a9ec0500a00000030020",
                    "00000641",
                ),
                (1280, 720),
                50.0,
            ),
            (
                "Quick Sync 1080p59.94, 3 of 4 sets predicted",
                concat!(
                    "42010121400000030090000003000003007ba003c08010e5b11e491b62a7e9a732d92a7b01402800001f4800",
                    "07530040",
                ),
                (1920, 1080),
                60_000.0 / 1001.0,
            ),
            (
                "Quick Sync 2160p59.94 PQ, 3 of 4 sets predicted",
                concat!(
                    "420101222000000300900000030000030099a001e020021c4db11e491b62a7e9a732d92a7b01421220122800",
                    "001f480007530040",
                ),
                (3840, 2160),
                60_000.0 / 1001.0,
            ),
            (
                "Quick Sync 2160p50 HLG, 7 of 8 sets predicted",
                concat!(
                    "420101222000000300900000030000030099a001e020021c4d90526491b61289e8469d36b968f4b989ac8953",
                    "2a3ec0508489048a0000030002000003006410",
                ),
                (3840, 2160),
                50.0,
            ),
            (
                "VAAPI 2160p59.94 PQ",
                concat!(
                    "420101022000000300b00000030000030096a001e020021c4d89359246dab016a1220120800001f480007530",
                    "6be10082",
                ),
                (3840, 2160),
                60_000.0 / 1001.0,
            ),
            (
                "x265 2160p59.94 PQ, an HRD",
                concat!(
                    "420101022000000300900000030000030099a001e020021c4d965654a4c2f016a1220120800001f480007530",
                    "30924c3f0000989680004c4b44",
                ),
                (3840, 2160),
                60_000.0 / 1001.0,
            ),
            (
                "x265 1080p59.94 PQ, three sub-layers, an HRD",
                concat!(
                    "42010402200000030090000003000003007b0000a003c08010e4d965652b295964932bc05a84880482000007",
                    "d20001d4c0c02924fc000112a88000225517000044aa2000089545c000112a880002255110",
                ),
                (1920, 1080),
                60_000.0 / 1001.0,
            ),
            (
                "HRD_A_Fujitsu_3, 11 of 13 sets predicted, an HRD",
                concat!(
                    "4201010160000003000003000003000003003fa00d080f1fe5979246d871792493f9e7f3cbffffff3f9fcfcf",
                    "e778001f80041eb000cdfe603d88f62333ca5e0092402be014d0049300492015f00a68024990",
                ),
                (416, 240),
                50.0,
            ),
            (
                "SLPPLP_A_VIDYO_2, three sub-layers, two of them with a profile and a level",
                concat!(
                    "4201040160000003000003000003000003005df0000140000003000003000003000003003f01400000030000",
                    "03000003000003005aa00d080f1fe5932e5e491b60817924922d4924917a9248924924bf9fe4ffcff99ffe36",
                    "40",
                ),
                (416, 240),
                0.0,
            ),
            (
                "LTRPSPS_A_Qualcomm_1, 9 of 12 sets predicted, 8 long-term pictures",
                concat!(
                    "4201010160000003000003000003000003003fa00d080f1fe58c26491b61a588aa93130cbecfaf37e59f5e14",
                    "46272ee27008180c000080c08072",
                ),
                (416, 240),
                0.0,
            ),
        ]
        .map(|(what, sps, size, rate)| (what, hx(sps), size, Some(rate).filter(|rate| *rate > 0.0)))
    }

    /// `(what, aspect ratio, colours, range)`.
    type Signalled = (&'static str, Option<&'static str>, Option<[&'static str; 3]>, Option<&'static str>);

    /// What content analysis signalling shows of SPS of [`real_sps`]: nothing
    /// of one whose scaling lists are spelt out, which it does not read past.
    pub(crate) fn signalled() -> [Signalled; 13] {
        const PQ: Option<[&str; 3]> = Some(["bt2020", "smpte2084", "bt2020-ncl"]);
        const HLG: Option<[&str; 3]> = Some(["bt2020", "arib-std-b67", "bt2020-ncl"]);
        const WIDE: Option<&str> = Some("16:9");
        const LIMITED: Option<&str> = Some("limited");
        [
            ("Quick Sync 720p50, 3 of 4 reference picture sets predicted", WIDE, None, LIMITED),
            ("Quick Sync 1080p59.94, 3 of 4 sets predicted", WIDE, None, LIMITED),
            ("Quick Sync 2160p59.94 PQ, 3 of 4 sets predicted", WIDE, PQ, LIMITED),
            ("Quick Sync 2160p50 HLG, 7 of 8 sets predicted", WIDE, HLG, LIMITED),
            ("VAAPI 2160p59.94 PQ", WIDE, PQ, LIMITED),
            ("x265 2160p59.94 PQ, an HRD", WIDE, PQ, LIMITED),
            ("x265 1080p59.94 PQ, three sub-layers, an HRD", WIDE, PQ, LIMITED),
            ("x265 720p50, 11 reference picture sets", WIDE, None, LIMITED),
            ("HRD_A_Fujitsu_3, 11 of 13 sets predicted, an HRD", Some("26:15"), None, None),
            (
                "SLPPLP_A_VIDYO_2, three sub-layers, two of them with a profile and a level",
                Some("26:15"),
                None,
                None,
            ),
            (
                "LTRPSPS_A_Qualcomm_1, 9 of 12 sets predicted, 8 long-term pictures",
                Some("26:15"),
                None,
                None,
            ),
            ("TNT 2160p50 PQ capture", None, None, None),
            ("Russian DTTV capture, fields of 576i", None, None, None),
        ]
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

    /// What an SPS of [`random_fields`] says of what the parsers read.
    pub(crate) struct Fields {
        pub profile: u8,
        pub level: u8,
        pub chroma: u32,
        pub coded: (u32, u32),
        /// Left, right, top, bottom.
        pub conf_win: (u32, u32, u32, u32),
        pub bit_depths: (u8, u8),
        /// Scaling lists spelt out.
        pub scaling_lists: bool,
        pub sample_aspect: (u32, u32),
        pub full_range: Option<bool>,
        /// `colour_primaries`, `transfer_characteristics`,
        /// `matrix_coeffs`.
        pub colours: Option<[u8; 3]>,
        /// `(vui_num_units_in_tick, vui_time_scale)`.
        pub timing: Option<(u32, u32)>,
    }

    /// `[DeltaPocS0, DeltaPocS1]` predicted from those of the set `before`
    /// (§7.4.8, (7-61) and (7-62)).
    fn predicted(before: &[Vec<i64>; 2], delta_rps: i64, use_delta: &[bool]) -> [Vec<i64>; 2] {
        let [s0, s1] = before;
        let in_s0 = |j: usize| (s0[j] + delta_rps, use_delta[j]);
        let in_s1 = |j: usize| (s1[j] + delta_rps, use_delta[s0.len() + j]);
        let own = (delta_rps, use_delta[s0.len() + s1.len()]);
        let negative = (0..s1.len()).rev().map(in_s1).chain([own]).chain((0..s0.len()).map(in_s0));
        let positive = (0..s0.len()).rev().map(in_s0).chain([own]).chain((0..s1.len()).map(in_s1));
        [
            negative.filter(|(delta, kept)| *kept && *delta < 0).map(|(delta, _)| delta).collect(),
            positive.filter(|(delta, kept)| *kept && *delta > 0).map(|(delta, _)| delta).collect(),
        ]
    }

    /// An SPS of random fields, each in its range (§7.4.3.2.1) but `spoilt`,
    /// as `(which ue(v) code of it, the code)`. Returns the NAL unit, how many
    /// ue(v) codes it holds, and what it says where none is spoilt.
    fn random_fields(seed: u64, spoilt: Option<(u32, u32)>) -> (Vec<u8>, u32, Fields) {
        let mut r = Random(seed);
        let mut w = Writer { bits: Bits::new(&[0x42, 0x01]), codes: 0, spoilt };
        let sub_layers = if r.up_to(2) == 0 { r.up_to(6) } else { 0 };
        w.put(0, 4);
        w.put(sub_layers, 3);
        w.put(1, 1);
        let (profile, level) = (r.up_to(31) as u8, r.up_to(255) as u8);
        w.put(u32::from(profile), 8);
        for width in [32, 16, 32] {
            w.put(r.next() as u32, width); // compatibility and constraint flags
        }
        w.put(u32::from(level), 8);
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
        let coded = (64 + r.up_to(16_320), 64 + r.up_to(16_320));
        w.ue(coded.0);
        w.ue(coded.1);
        let cropped = r.up_to(1);
        w.put(cropped, 1);
        let conf_win = [(); 4].map(|()| r.up_to(15) * cropped);
        for offset in conf_win.iter().take(4 * cropped as usize) {
            w.ue(*offset);
        }
        let depths = (r.up_to(8), r.up_to(8));
        w.ue(depths.0);
        w.ue(depths.1);
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
        let spelt_out = if scaling_lists == 1 { r.up_to(1) } else { 0 };
        w.put(spelt_out, scaling_lists);
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
        let mut delta_pocs = [Vec::new(), Vec::new()];
        for set in 0..sets {
            let predict = if set > 0 { r.up_to(1) } else { 0 };
            w.put(predict, u32::from(set > 0));
            if predict == 1 {
                let (sign, minus1) = (r.up_to(1), r.up_to(7));
                w.put(sign, 1);
                w.ue(minus1);
                let delta_rps = i64::from(minus1 + 1) * if sign == 1 { -1 } else { 1 };
                let use_delta: Vec<bool> = (0..=delta_pocs[0].len() + delta_pocs[1].len())
                    .map(|_| {
                        let used = r.up_to(1);
                        w.put(used, 1);
                        let kept = if used == 1 { 1 } else { r.up_to(1) };
                        w.put(kept, 1 - used);
                        kept == 1
                    })
                    .collect();
                delta_pocs = predicted(&delta_pocs, delta_rps, &use_delta);
            } else {
                let pics = [r.up_to(8), r.up_to(7)];
                w.ue(pics[0]);
                w.ue(pics[1]);
                delta_pocs = [(pics[0], -1), (pics[1], 1)].map(|(pics, sign)| {
                    let mut delta_poc = 0;
                    (0..pics)
                        .map(|_| {
                            let minus1 = r.up_to(7);
                            w.ue(minus1);
                            w.put(r.up_to(1), 1);
                            delta_poc += sign * i64::from(minus1 + 1);
                            delta_poc
                        })
                        .collect()
                });
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
        let mut sample_aspect = (1, 1);
        if extended_sar == 1 {
            sample_aspect = (r.up_to(0xFFFF), r.up_to(0xFFFF));
            w.put(255, 8);
            w.put(sample_aspect.0, 16);
            w.put(sample_aspect.1, 16);
        }
        w.put(0, 1); // overscan_info_present_flag
        let signal_type = r.up_to(1);
        w.put(signal_type, 1);
        let (mut full_range, mut colours) = (None, None);
        if signal_type == 1 {
            w.put(r.up_to(7), 3);
            full_range = Some(r.up_to(1) == 1);
            w.put(u32::from(full_range == Some(true)), 1);
            let codes = [(); 3].map(|()| r.up_to(22) as u8);
            colours = (r.up_to(1) == 1).then_some(codes);
            w.put(u32::from(colours.is_some()), 1);
            for code in colours.into_iter().flatten() {
                w.put(u32::from(code), 8);
            }
        }
        w.put(0, 1); // chroma_loc_info_present_flag
        w.put(r.up_to(7), 3);
        let window = r.up_to(1);
        w.put(window, 1);
        for _ in 0..4 * window {
            w.ue(r.up_to(15));
        }
        let timed = r.up_to(1);
        w.put(timed, 1);
        let timing = match r.up_to(3) {
            0 => (r.up_to(2), r.next() as u32),
            1 => (1001, 60_000),
            _ => (1 + r.up_to(999), 1 + r.up_to(99_999)),
        };
        if timed == 1 {
            w.put(timing.0, 32);
            w.put(timing.1, 32);
            w.put(0, 2);
        }
        w.put(0, 2); // bitstream_restriction_flag, sps_extension_present_flag
        let fields = Fields {
            profile,
            level,
            chroma,
            coded,
            conf_win: conf_win.into(),
            bit_depths: (depths.0 as u8 + 8, depths.1 as u8 + 8),
            scaling_lists: spelt_out == 1,
            sample_aspect,
            full_range,
            colours,
            timing: (timed == 1).then_some(timing),
        };
        (escaped(w.bits.done(), 2), w.codes, fields)
    }

    /// `n` SPS NAL units, the same ones each time, and what each says.
    pub(crate) fn valid_sps(n: usize) -> impl Iterator<Item = (Vec<u8>, Fields)> {
        let mut r = Random(0x2545_F491_4F6C_DD1D);
        (0..n).map(move |_| {
            let (nal, _, fields) = random_fields(r.next(), None);
            (nal, fields)
        })
    }

    /// How many of [`valid_sps`] each parser is given.
    pub(crate) const VALID: usize = 1_000;

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
            let (_, codes, _) = random_fields(seed, None);
            let code = ((r.next() >> 32) as u32 >> r.up_to(31)).min(u32::MAX - 1);
            random_fields(seed, Some((r.up_to(codes - 1), code))).0
        })
    }

    /// How many of [`random_sps`] each parser is given. Until 2026-11 media
    /// analysis, signalling and the probe panicked on some, under overflow
    /// checks.
    pub(crate) const RANDOM: usize = 4_000;

    /// §7.3.3: no bits where there is one sub-layer, else sixteen of flags,
    /// and 88 for each profile and 8 for each level there is.
    #[test]
    fn sub_layers_are_skipped_by_what_there_is_of_each() {
        // The bits read of `left`, of flags and then ones.
        let read = |max_sub_layers_minus1: u32, flags: u16, mut left: u32| {
            let (all, mut first) = (left, true);
            skip_sub_layers(max_sub_layers_minus1, |n| {
                left = left.checked_sub(u32::from(n))?;
                let flags = std::mem::take(&mut first).then_some(u32::from(flags));
                Some(flags.unwrap_or(u32::MAX >> (32 - n)))
            })
            .map(|()| all - left)
        };
        assert_eq!(read(0, 0xFFFF, 0), Some(0));
        for flags in (0..=0xFFFF).step_by(255) {
            for sub_layers in 1..=7 {
                let bits = (0..sub_layers).fold(16, |bits, i| {
                    bits + 88 * u32::from(flags >> (15 - 2 * i) & 1) + 8 * u32::from(flags >> (14 - 2 * i) & 1)
                });
                assert_eq!(read(sub_layers, flags, 1_000), Some(bits), "{sub_layers} {flags:#06x}");
                assert_eq!(read(sub_layers, flags, bits), Some(bits), "{sub_layers} {flags:#06x}");
                for short in [0, 15, bits - 1] {
                    assert_eq!(read(sub_layers, flags, short), None, "{sub_layers} {flags:#06x}");
                }
            }
        }
    }

    /// The sixteen bits behind the reference picture sets `write` writes, and
    /// `None` for every part of them.
    fn behind(lsb_minus4: u32, write: impl Fn(&mut Bits)) -> Option<u32> {
        let skip = |bytes: &[u8]| {
            let mut reader = BitReader::new(bytes);
            skip_ref_pic_sets(&mut reader, lsb_minus4, BitReader::read_bits, BitReader::read_ue)?;
            reader.read_bits(16)
        };
        let mut bits = Bits::new(&[]);
        write(&mut bits);
        let bytes = bits.put(0xACE5, 16).done();
        // `done` adds a stop bit and three bytes of payload.
        let cut = bytes.len() - 4;
        for len in (0..cut).step_by(cut / 16 + 1).chain([cut - 1]) {
            assert_eq!(skip(&bytes[..len]), None, "{len} of {cut} bytes");
        }
        skip(&bytes)
    }

    /// A set of `negative` and `positive` pictures, each one behind the last.
    fn explicit(b: &mut Bits, first: bool, negative: &[u32], positive: &[u32]) {
        b.put(0, u32::from(!first)).ue(negative.len() as u32).ue(positive.len() as u32);
        for minus1 in negative.iter().chain(positive) {
            b.ue(*minus1).put(1, 1);
        }
    }

    /// A set predicted from the one before it: `delta_rps`, and whether each
    /// entry of that one, and then its picture, is kept.
    fn predicted_by(b: &mut Bits, delta_rps: i32, kept: &[u8]) {
        b.put(1, 1).put(u64::from(delta_rps < 0), 1).ue(delta_rps.unsigned_abs() - 1);
        for (i, kept) in kept.iter().enumerate() {
            match (kept, i % 2) {
                (0, _) => b.put(0b00, 2),
                (_, 0) => b.put(0b01, 2),
                _ => b.put(1, 1),
            };
        }
    }

    const BEHIND: Option<u32> = Some(0xACE5);

    /// §7.3.7: an SPS's predicted set holds no `delta_idx_minus1`, and one
    /// flag or two for each picture of the set before it and one more.
    #[test]
    fn a_predicted_set_is_of_the_set_before_it() {
        assert_eq!(behind(0, |b| _ = b.ue(0).put(0, 1)), BEHIND, "no sets");
        let sets = |more: &dyn Fn(&mut Bits), n| {
            behind(4, |b| {
                b.ue(n);
                explicit(b, true, &[0, 1], &[2]); // -1 and -3, 3
                more(b);
                b.put(0, 1);
            })
        };
        assert_eq!(sets(&|_| (), 1), BEHIND);
        assert_eq!(sets(&|b| explicit(b, false, &[0; 16], &[0; 16]), 2), BEHIND);
        for kept in 0..16_u8 {
            let kept = [kept & 1, kept >> 1 & 1, kept >> 2 & 1, kept >> 3];
            let left = kept.iter().sum::<u8>() as usize;
            for delta_rps in [-5, -4, 4, 5] {
                let two = |b: &mut Bits| {
                    predicted_by(b, delta_rps, &kept);
                    predicted_by(b, 100, &vec![1; left + 1]);
                };
                assert_eq!(sets(&two, 3), BEHIND, "{delta_rps} {kept:?}");
            }
        }
    }

    /// §7.4.8: an entry that comes to a delta of 0 is in no set, whichever it
    /// is, and the others keep their places: the negative first, each half
    /// nearest first.
    #[test]
    fn a_predicted_set_leaves_out_its_own_picture() {
        let sets = |predictions: &[(i32, &[u8])]| {
            behind(4, |b| {
                b.ue(1 + predictions.len() as u32);
                explicit(b, true, &[0, 1], &[1]); // -1 and -3, 2
                for (delta_rps, kept) in predictions {
                    predicted_by(b, *delta_rps, kept);
                }
                b.put(0, 1);
            })
        };
        // Each of the three comes to 0, and so is not there to be predicted
        // from.
        for delta_rps in [1, 3, -2] {
            assert_eq!(sets(&[(delta_rps, &[1, 1, 1, 1]), (9, &[1, 1, 1, 1])]), BEHIND, "{delta_rps}");
        }
        // To -2 (its picture) and -3, in that order: of +2 the first is left
        // out.
        assert_eq!(sets(&[(-2, &[1, 0, 1, 1]), (2, &[1, 0, 1]), (9, &[1, 1])]), BEHIND);
        assert_eq!(sets(&[(-2, &[1, 0, 1, 1]), (3, &[0, 1, 1]), (9, &[1, 1])]), BEHIND);
        // To 1 (the second, -3 + 4), 3 (the first) and 4 (its picture).
        assert_eq!(sets(&[(4, &[1, 1, 0, 1]), (-1, &[1, 1, 1, 0]), (9, &[1, 1, 1])]), BEHIND);
        assert_eq!(sets(&[(4, &[1, 1, 0, 1]), (-3, &[0, 1, 1, 0]), (9, &[1, 1])]), BEHIND);
        // To -2 and -4, and 1: the negative ones come first.
        assert_eq!(sets(&[(-1, &[1, 1, 1, 0]), (-1, &[0, 0, 1, 0]), (9, &[1])]), BEHIND);
        assert_eq!(sets(&[(-1, &[1, 1, 1, 0]), (2, &[1, 0, 1, 0]), (9, &[1, 1])]), BEHIND);
    }

    /// The longest codes, as often as there can be, and as many pictures.
    #[test]
    fn the_largest_sets_there_are_fit() {
        let last = u32::MAX - 1;
        for sign in [0, 1] {
            let sets = |n: u32| {
                behind(12, |b| {
                    b.ue(n);
                    explicit(b, true, &[last; 16], &[last; 16]);
                    for set in 1..n.min(65) {
                        // Every picture of the set before, and its own.
                        b.put(1, 1).put(sign, 1).ue(last - 1).put(u64::MAX, 32).put(u64::MAX, set);
                    }
                    b.put(1, 1).ue(32);
                    for _ in 0..32 {
                        b.put(0xFFFF, 16).put(1, 1);
                    }
                })
            };
            assert_eq!(sets(64), BEHIND);
            assert_eq!(sets(65), None);
        }
    }

    #[test]
    fn a_long_term_picture_is_as_long_as_its_poc_lsb() {
        let pics = |lsb_minus4: u32, n: u32| {
            behind(lsb_minus4, |b| {
                b.ue(0).put(1, 1).ue(n);
                for _ in 0..n {
                    b.put(0, lsb_minus4 + 4).put(0, 1);
                }
            })
        };
        for lsb_minus4 in 0..=12 {
            for n in [0, 1, 2, 31, 32] {
                assert_eq!(pics(lsb_minus4, n), BEHIND, "{lsb_minus4} {n}");
            }
            assert_eq!(pics(lsb_minus4, 33), None);
        }
        assert_eq!(pics(13, 0), None);
        assert_eq!(pics(13, 1), None);
    }

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
