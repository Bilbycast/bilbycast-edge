// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Every parameter set a passthrough source uses, by id — what its CMAF init
//! has to carry.
//!
//! A decoder keeps parameter sets by id, and a slice names the PPS it decodes
//! against (`pic_parameter_set_id` in H.264, `slice_pic_parameter_set_id` in
//! HEVC), which names its SPS, which in HEVC names its VPS. An `avc1` / `hvc1`
//! sample entry carries them out of band only — the samples are stripped of
//! theirs (`nalu::filter_frame_nalus_h264` / `_h265`) — so every id a slice
//! can name has to be in the init's `avcC` / `hvcC`.
//!
//! The track used to be built from the demuxer's cache, which holds one set of
//! each kind, the last one seen. A source whose random-access points carry
//! more than one PPS — the 770_H capture sends VPS0, SPS0, PPS0, PPS1, PPS2 at
//! every IRAP and its slices name PPS0 and PPS1 — ended every IRAP with PPS2
//! in the cache, so the init carried PPS2 alone and every frame failed to
//! decode with "PPS id out of range" (bilbycast-edge#204). The cache was also
//! read after the whole datagram had been demuxed, so the sets it held could
//! belong to a frame later in the batch than the one being handled.
//!
//! [`ParamSetCollector`] keeps a table per kind, by id, fed one access unit
//! at a time in decode order. Its rule: **a random-access access unit defines
//! each kind it carries** — after it, that kind holds exactly the sets the
//! access unit itself carried (the last of an id within it winning), and the
//! kinds it does not carry are left as they were. Any other access unit merges
//! its sets in by id. So the table always holds the last set seen of every
//! kind, a superset of what the single-slot cache held; a source that changes
//! underneath drops its predecessor's extra ids at its first random-access
//! point that re-sends them; and a source that sends the same sets at every
//! random-access point holds one stable table.
//!
//! A parameter set is stored under the id it names. One that names none — cut
//! short (a PES truncated by loss can end on a one-byte NAL), an id out of the
//! codec's range, or an HEVC set for a layer other than the base layer — is
//! ignored, never stored: it cannot be matched to what a slice names, and in
//! an init it is a record a strict decoder may refuse whole. So is one longer
//! than a configuration record's 16-bit length can describe: real ones are a
//! few hundred bytes at most, and with nothing else bounding an entry the
//! table could hold hundreds of them as long as a PES.
//!
//! Every read of source bytes here is bounds-checked: these NALs come off the
//! wire, and a panic would take the output's task down with it.

use super::codecs::BitReader;
use super::fmp4::VideoCodec;
use super::nalu::{
    H264_NAL_PPS, H264_NAL_SPS, H265_NAL_PPS, H265_NAL_SPS, H265_NAL_VPS, h264_nal_type,
    h265_nal_type,
};

/// `numOfSequenceParameterSets` in an `avcC` is five bits.
const AVCC_MAX_SPS: usize = 31;
/// `numOfPictureParameterSets` in an `avcC` is eight bits.
const AVCC_MAX_PPS: usize = 255;

/// The shortest NAL taken for a parameter set. The smallest well-formed one of
/// any kind in either codec is an H.264 PPS: a one-byte header, 17 bits of
/// mandatory syntax and the stop bit — four bytes. Anything shorter is a
/// fragment of one.
const MIN_PARAM_SET_LEN: usize = 4;

/// The longest NAL taken for a parameter set: each `avcC` / `hvcC` entry
/// carries its length in sixteen bits, so a longer one cannot be written
/// into an init — it would go in under a truncated length, a malformed
/// record. A real parameter set is nowhere near it; one this long is corrupt.
const MAX_PARAM_SET_LEN: usize = u16::MAX as usize;

/// One parameter set: its id, and the NAL (no start code, no length prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSet {
    pub id: u32,
    pub nal: Vec<u8>,
}

/// The parameter sets a track carries, each list sorted by id with no id
/// twice. `vps` is empty for H.264.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamSets {
    pub vps: Vec<ParamSet>,
    pub sps: Vec<ParamSet>,
    pub pps: Vec<ParamSet>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Vps,
    Sps,
    Pps,
}

impl ParamSets {
    /// One parameter set of each kind, as an encoder hands them over — the
    /// re-encoder's output and the tests. Each is kept whatever its bytes: the
    /// list has one entry, so nothing has to be found by its id, and the init
    /// it writes is the one a single-set track has always written. The id is
    /// read where it can be, and 0 otherwise.
    pub fn single(codec: VideoCodec, vps: Vec<u8>, sps: Vec<u8>, pps: Vec<u8>) -> Self {
        let entry = |kind: Kind, nal: Vec<u8>| {
            vec![ParamSet { id: read_id(codec, kind, &nal).unwrap_or(0), nal }]
        };
        Self {
            vps: match codec {
                VideoCodec::H264 => Vec::new(),
                VideoCodec::H265 => entry(Kind::Vps, vps),
            },
            sps: entry(Kind::Sps, sps),
            pps: entry(Kind::Pps, pps),
        }
    }

    /// Whether these are enough to describe a `codec` track: an SPS and a PPS,
    /// and for HEVC a VPS.
    pub fn is_complete(&self, codec: VideoCodec) -> bool {
        !self.sps.is_empty()
            && !self.pps.is_empty()
            && (codec == VideoCodec::H264 || !self.vps.is_empty())
    }

    /// Whether every set in `other` is here, under the same id with the same
    /// bytes. Extra sets here do not matter: a random-access point that leaves
    /// out an id the track carries is not a change, and rotating on it would
    /// churn generations on an encoder that re-sends a varying subset. No
    /// allocation — it runs on every IDR.
    pub fn contains_all(&self, other: &ParamSets) -> bool {
        fn covers(have: &[ParamSet], want: &[ParamSet]) -> bool {
            want.iter().all(|w| {
                have.binary_search_by_key(&w.id, |e| e.id).is_ok_and(|i| have[i].nal == w.nal)
            })
        }
        covers(&self.vps, &other.vps) && covers(&self.sps, &other.sps) && covers(&self.pps, &other.pps)
    }

    fn list_mut(&mut self, kind: Kind) -> &mut Vec<ParamSet> {
        match kind {
            Kind::Vps => &mut self.vps,
            Kind::Sps => &mut self.sps,
            Kind::Pps => &mut self.pps,
        }
    }
}

/// The parameter sets a passthrough source has sent, by id, under the
/// random-access rule in the module docs. Fed every access unit, in decode
/// order, before the track is built from it or checked against it.
#[derive(Debug, Default)]
pub struct ParamSetCollector {
    /// The codec the table holds sets for. A change of codec starts the
    /// table again: an H.264 SPS read as HEVC is nonsense.
    codec: Option<VideoCodec>,
    sets: ParamSets,
}

impl ParamSetCollector {
    /// Take in one access unit. `is_rap` is the demuxer's keyframe flag: an
    /// IDR for H.264, any IRAP for HEVC.
    ///
    /// On an access unit that carries no parameter set — almost all of them —
    /// this is one length check and one header byte per NAL.
    pub fn observe(&mut self, codec: VideoCodec, nalus: &[Vec<u8>], is_rap: bool) {
        if self.codec != Some(codec) {
            self.codec = Some(codec);
            self.sets = ParamSets::default();
        }
        let mut carried = [false; 3];
        for nal in nalus {
            if let Some((kind, _)) = classify(codec, nal) {
                carried[kind as usize] = true;
            }
        }
        if carried == [false; 3] {
            return;
        }
        if is_rap {
            for kind in [Kind::Vps, Kind::Sps, Kind::Pps] {
                if carried[kind as usize] {
                    self.sets.list_mut(kind).retain(|e| {
                        nalus.iter().any(|n| classify(codec, n) == Some((kind, e.id)))
                    });
                }
            }
        }
        for nal in nalus {
            if let Some((kind, id)) = classify(codec, nal) {
                upsert(self.sets.list_mut(kind), id, nal, cap(codec, kind));
            }
        }
    }

    /// The table, if it holds `codec`'s sets.
    pub fn sets(&self, codec: VideoCodec) -> Option<&ParamSets> {
        (self.codec == Some(codec)).then_some(&self.sets)
    }

    /// The table, if it holds enough of `codec`'s sets to describe a track.
    pub fn complete_sets(&self, codec: VideoCodec) -> Option<&ParamSets> {
        self.sets(codec).filter(|s| s.is_complete(codec))
    }
}

/// Insert or replace one set by id. The bytes are copied only when they
/// differ, and a new id is taken only while the list is under `cap`.
fn upsert(list: &mut Vec<ParamSet>, id: u32, nal: &[u8], cap: usize) {
    match list.binary_search_by_key(&id, |e| e.id) {
        Ok(i) => {
            if list[i].nal != nal {
                list[i].nal.clear();
                list[i].nal.extend_from_slice(nal);
            }
        }
        Err(i) => {
            if list.len() < cap {
                list.insert(i, ParamSet { id, nal: nal.to_vec() });
            }
        }
    }
}

/// How many sets of a kind the codec's configuration record can hold. HEVC's
/// arrays count in sixteen bits, and its id ranges (16 / 16 / 64) bound them
/// well under that.
fn cap(codec: VideoCodec, kind: Kind) -> usize {
    match (codec, kind) {
        (VideoCodec::H264, Kind::Sps) => AVCC_MAX_SPS,
        (VideoCodec::H264, _) => AVCC_MAX_PPS,
        (VideoCodec::H265, _) => usize::MAX,
    }
}

/// The kind and id of a parameter set, or `None` for any other NAL, and for a
/// parameter set that is too short to be one, too long for a configuration
/// record, names no id in range, or (HEVC) belongs to a layer other than the
/// base layer.
fn classify(codec: VideoCodec, nal: &[u8]) -> Option<(Kind, u32)> {
    if !(MIN_PARAM_SET_LEN..=MAX_PARAM_SET_LEN).contains(&nal.len()) {
        return None;
    }
    let kind = match codec {
        VideoCodec::H264 => match h264_nal_type(nal) {
            H264_NAL_SPS => Kind::Sps,
            H264_NAL_PPS => Kind::Pps,
            _ => return None,
        },
        VideoCodec::H265 => {
            let kind = match h265_nal_type(nal) {
                H265_NAL_VPS => Kind::Vps,
                H265_NAL_SPS => Kind::Sps,
                H265_NAL_PPS => Kind::Pps,
                _ => return None,
            };
            // nuh_layer_id: the low bit of the first header byte and the top
            // five of the second. A base-layer decoder ignores the others.
            let layer = ((nal.first()? & 1) << 5) | (nal.get(1)? >> 3);
            if layer != 0 {
                return None;
            }
            kind
        }
    };
    Some((kind, read_id(codec, kind, nal)?))
}

/// The id a parameter set names, if it can be read and is in range.
fn read_id(codec: VideoCodec, kind: Kind, nal: &[u8]) -> Option<u32> {
    match (codec, kind) {
        (VideoCodec::H264, Kind::Sps) => h264_sps_id(nal),
        (VideoCodec::H264, Kind::Pps) => h264_pps_id(nal),
        (VideoCodec::H264, Kind::Vps) => None,
        (VideoCodec::H265, Kind::Vps) => hevc_vps_id(nal),
        (VideoCodec::H265, Kind::Sps) => hevc_sps_id(nal),
        (VideoCodec::H265, Kind::Pps) => hevc_pps_id(nal),
    }
}

/// H.264 §7.3.2.1.1: `seq_parameter_set_id` ue(v), 0..=31, after
/// `profile_idc`, the constraint flags and `level_idc`.
fn h264_sps_id(nal: &[u8]) -> Option<u32> {
    let mut r = BitReader::new(nal.get(1..)?);
    r.skip_bits(24)?;
    r.ue().filter(|id| *id <= 31)
}

/// H.264 §7.3.2.2: `pic_parameter_set_id` ue(v), 0..=255, first.
fn h264_pps_id(nal: &[u8]) -> Option<u32> {
    BitReader::new(nal.get(1..)?).ue().filter(|id| *id <= 255)
}

/// H.265 §7.3.2.1: `vps_video_parameter_set_id` u(4), first.
fn hevc_vps_id(nal: &[u8]) -> Option<u32> {
    nal.get(2).map(|b| u32::from(b >> 4))
}

/// H.265 §7.3.2.2.1: `sps_seq_parameter_set_id` ue(v), 0..=15, after the VPS
/// id, the sub-layer count, the nesting flag and the whole
/// `profile_tier_level`.
fn hevc_sps_id(nal: &[u8]) -> Option<u32> {
    let mut r = BitReader::new(nal.get(2..)?);
    r.skip_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.read_bits(3)?;
    r.skip_bits(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue().filter(|id| *id <= 15)
}

/// H.265 §7.3.2.3.1: `pps_pic_parameter_set_id` ue(v), 0..=63, first.
fn hevc_pps_id(nal: &[u8]) -> Option<u32> {
    BitReader::new(nal.get(2..)?).ue().filter(|id| *id <= 63)
}

/// H.265 §7.3.3 `profile_tier_level(1, max_sub_layers_minus1)`. The
/// sub-layer fields are present only where their flags say so — skipping them
/// unconditionally lands the reader in the wrong place on any stream with
/// temporal sub-layers whose flags are not all set.
fn skip_profile_tier_level(
    r: &mut BitReader<'_>,
    max_sub_layers_minus1: u32,
) -> Option<()> {
    // general_profile_space(2) general_tier_flag(1) general_profile_idc(5)
    // general_profile_compatibility_flag[32] the four source/constraint flags,
    // 43 reserved/constraint bits, one more, general_level_idc(8): 96 bits.
    r.skip_bits(96)?;
    let n = max_sub_layers_minus1.min(7) as usize;
    let mut present = [(false, false); 7];
    for p in present.iter_mut().take(n) {
        *p = (r.bit()?, r.bit()?);
    }
    if n > 0 {
        // reserved_zero_2bits for each sub-layer slot from n up to 8.
        r.skip_bits(2 * (8 - n as u32))?;
    }
    for &(profile_present, level_present) in present.iter().take(n) {
        if profile_present {
            // sub_layer_profile_space .. the 44 constraint/reserved bits.
            r.skip_bits(88)?;
        }
        if level_present {
            r.skip_bits(8)?; // sub_layer_level_idc
        }
    }
    Some(())
}


#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::engine::cmaf) fn hx(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// A small MSB-first bit writer, for parameter sets and slice headers
    /// whose fields are known. It inserts no emulation-prevention bytes, so
    /// what it builds must not hold `00 00 0x` (x ≤ 3) — the patterns used
    /// here do not.
    pub(in crate::engine::cmaf) struct Bits {
        out: Vec<u8>,
        n: usize,
    }

    impl Bits {
        pub(in crate::engine::cmaf) fn new(head: &[u8]) -> Self {
            Self { out: head.to_vec(), n: head.len() * 8 }
        }
        pub(in crate::engine::cmaf) fn put(&mut self, v: u64, width: u32) -> &mut Self {
            for i in (0..width).rev() {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                if (v >> i) & 1 == 1 {
                    *self.out.last_mut().unwrap() |= 0x80 >> (self.n % 8);
                }
                self.n += 1;
            }
            self
        }
        pub(in crate::engine::cmaf) fn ue(&mut self, v: u32) -> &mut Self {
            let code = u64::from(v) + 1;
            let len = 64 - code.leading_zeros();
            self.put(0, len - 1).put(code, len)
        }
        /// The stop bit, alignment, and a tail of payload, so no NAL built
        /// here is "too short".
        pub(in crate::engine::cmaf) fn done(&mut self) -> Vec<u8> {
            self.put(1, 1);
            while !self.n.is_multiple_of(8) {
                self.put(0, 1);
            }
            self.out.extend_from_slice(&[0xA5, 0x5A, 0xC3]);
            self.out.clone()
        }
    }

    /// H.264 SPS with `id`; `tag` varies the bytes behind the id.
    pub(in crate::engine::cmaf) fn h264_sps(id: u32, tag: u8) -> Vec<u8> {
        Bits::new(&[0x67, 0x64, 0x00, 0x28]).ue(id).put(u64::from(tag), 8).done()
    }
    pub(in crate::engine::cmaf) fn h264_pps(id: u32, tag: u8) -> Vec<u8> {
        Bits::new(&[0x68]).ue(id).put(u64::from(tag), 8).done()
    }
    pub(in crate::engine::cmaf) fn hevc_vps(id: u32, tag: u8) -> Vec<u8> {
        Bits::new(&[0x40, 0x01]).put(u64::from(id), 4).put(0xFFF, 12).put(u64::from(tag), 8).done()
    }
    /// HEVC SPS with `id` and one sub-layer per entry of `sub_layers`, each
    /// `(sub_layer_profile_present_flag, sub_layer_level_present_flag)`.
    pub(in crate::engine::cmaf) fn hevc_sps_with(
        id: u32,
        tag: u8,
        sub_layers: &[(bool, bool)],
    ) -> Vec<u8> {
        let msl = sub_layers.len() as u64;
        let mut b = Bits::new(&[0x42, 0x01]);
        b.put(0, 4).put(msl, 3).put(1, 1);
        // general PTL: Main, compat flag 1 and 2, progressive + frame-only,
        // level 4.0.
        b.put(0x01, 8).put(0x6000_0000, 32).put(0x9000_0000_0000, 48).put(120, 8);
        for &(p, l) in sub_layers {
            b.put(u64::from(p), 1).put(u64::from(l), 1);
        }
        if msl > 0 {
            for _ in msl..8 {
                b.put(0, 2);
            }
        }
        for &(p, l) in sub_layers {
            if p {
                b.put(0x0A5_A5A5_A5A5, 44).put(0x0A5_A5A5_A5A5, 44);
            }
            if l {
                b.put(0x5D, 8);
            }
        }
        b.ue(id).put(u64::from(tag), 8).done()
    }
    pub(in crate::engine::cmaf) fn hevc_sps(id: u32, tag: u8) -> Vec<u8> {
        hevc_sps_with(id, tag, &[])
    }
    pub(in crate::engine::cmaf) fn hevc_pps(id: u32, tag: u8) -> Vec<u8> {
        Bits::new(&[0x44, 0x01]).ue(id).put(u64::from(tag), 8).done()
    }

    const H264_IDR: [u8; 4] = [0x65, 0x88, 0x84, 0x21];
    const H264_P: [u8; 4] = [0x41, 0x9A, 0x02, 0x21];
    const HEVC_IDR: [u8; 4] = [0x26, 0x01, 0xAF, 0x21];
    const HEVC_TRAIL: [u8; 4] = [0x02, 0x01, 0xD0, 0x21];

    fn au(nals: &[&[u8]]) -> Vec<Vec<u8>> {
        nals.iter().map(|n| n.to_vec()).collect()
    }

    fn ids(list: &[ParamSet]) -> Vec<u32> {
        list.iter().map(|e| e.id).collect()
    }

    fn table(c: &ParamSetCollector, codec: VideoCodec) -> ParamSets {
        c.sets(codec).cloned().unwrap_or_default()
    }

    /// Real parameter sets — x264 told `sps-id=5`, the sky-witness capture's
    /// (id 0) — and synthetic ones across each id's range.
    #[test]
    fn h264_ids_are_read_from_sps_and_pps() {
        let c = VideoCodec::H264;
        let x264_sps = hx("6764000d32cd94141fb011000003000100000300320f142996");
        let x264_pps = hx("6831abe3cb22c0");
        assert_eq!(classify(c, &x264_sps), Some((Kind::Sps, 5)));
        assert_eq!(classify(c, &x264_pps), Some((Kind::Pps, 5)));
        let sky_sps = hx(concat!(
            "67640028ad843588742050b041074421ac47361882a210ffa69a4d5a31526c862c92c8a431cb10",
            "4111251c28c716161d1e09050b05d06d087fd7af93f11fd7e6f8af0870d608a0a0b290078044fd",
            "e036a020202800000300080000030194a0",
        ));
        assert_eq!(classify(c, &sky_sps), Some((Kind::Sps, 0)));
        assert_eq!(classify(c, &hx("68ef3cb0")), Some((Kind::Pps, 0)));
        for id in [0, 5, 31] {
            assert_eq!(classify(c, &h264_sps(id, 1)), Some((Kind::Sps, id)));
        }
        for id in [0, 1, 200, 255] {
            assert_eq!(classify(c, &h264_pps(id, 1)), Some((Kind::Pps, id)));
        }
        // An emulation-prevention byte on the way to the id: profile,
        // constraint and level all zero put `00 00 00` in the RBSP, which the
        // encoder escapes as `00 00 03 00`. Read without stripping it, the
        // id's byte would be taken as the level and the id read from the
        // wrong place.
        let escaped = [0x67, 0x00, 0x00, 0x03, 0x00, 0x30, 0xA5, 0x5A];
        assert_eq!(classify(c, &escaped), Some((Kind::Sps, 5)));
        // Out of range: an SPS id past 31, a PPS id past 255.
        assert_eq!(classify(c, &h264_sps(32, 1)), None);
        assert_eq!(classify(c, &h264_pps(256, 1)), None);
        // Not parameter sets at all.
        assert_eq!(classify(c, &H264_IDR), None);
        assert_eq!(classify(c, &H264_P), None);
    }

    /// The HEVC SPS id sits behind `profile_tier_level`, whose sub-layer
    /// fields are present only where their flags say so. A parser that skips
    /// them unconditionally, or not at all, reads the id from the wrong bits
    /// as soon as a stream has temporal sub-layers with a flag clear.
    #[test]
    fn hevc_ids_are_read_past_sub_layer_ptl() {
        let c = VideoCodec::H265;
        assert_eq!(classify(c, &hevc_vps(3, 1)), Some((Kind::Vps, 3)));
        assert_eq!(classify(c, &hevc_pps(1, 1)), Some((Kind::Pps, 1)));
        assert_eq!(classify(c, &hevc_pps(63, 1)), Some((Kind::Pps, 63)));
        assert_eq!(classify(c, &hevc_pps(64, 1)), None);
        for sub_layers in [
            &[][..],
            &[(true, true), (true, true)][..],
            // Mixed: the case an unconditional skip gets wrong.
            &[(false, true), (true, false)][..],
            &[(false, false), (false, false), (true, false)][..],
            &[(true, false); 6][..],
        ] {
            for id in [0, 7, 15] {
                assert_eq!(
                    classify(c, &hevc_sps_with(id, 1, sub_layers)),
                    Some((Kind::Sps, id)),
                    "{sub_layers:?}"
                );
            }
        }
        assert_eq!(classify(c, &hevc_sps(16, 1)), None);
        assert_eq!(classify(c, &HEVC_IDR), None);
        assert_eq!(classify(c, &HEVC_TRAIL), None);
    }

    /// Real encoder SPS, checked past the id against what the encoder was
    /// given: libx265 with two and three temporal sub-layers (4:4:4,
    /// 320x240), and the 770_H capture's (Main 10, emulation-prevention
    /// bytes in its PTL, 1920x544). The reader carries on past the id to the
    /// picture size, so a skip that is off by any number of bits fails here
    /// even where the id it happened to read was right.
    #[test]
    fn real_hevc_sps_parse_to_their_picture_size() {
        fn read(nal: &[u8]) -> (u32, u32, u32, u32) {
            let mut r = BitReader::new(&nal[2..]);
            r.skip_bits(4).unwrap();
            let msl = r.read_bits(3).unwrap();
            r.skip_bits(1).unwrap();
            skip_profile_tier_level(&mut r, msl).unwrap();
            let id = r.ue().unwrap();
            let chroma = r.ue().unwrap();
            if chroma == 3 {
                r.skip_bits(1).unwrap();
            }
            (id, chroma, r.ue().unwrap(), r.ue().unwrap())
        }
        let x265_tl3 = hx(concat!(
            "4201040408000003009e0800000300003c000090014101e2cb2b29594acb24995e02dc0808",
            "00100000030010000003019080",
        ));
        let x265_tl4 = hx(concat!(
            "4201060408000003009e0800000300003c000090014101e2cb2b125624ac4988264932bc05",
            "b81010002000000300200000030321",
        ));
        let h770 = hx(concat!(
            "42010102200000030060000003000003007ba003c080221f6d8d0c199246f843fff91421ff",
            "fc8a10fffffffffffffffe45087fffffffffffffff22843fffffffffffffffc8a10fffffff",
            "fffffffff22843fffffffffffffffe10ffffffffffffffff5780a1010101eff00000030010",
            "000003032080",
        ));
        assert_eq!((x265_tl3[2] >> 1) & 7, 2, "fixture: two sub-layers above the base");
        assert_eq!((x265_tl4[2] >> 1) & 7, 3);
        assert_eq!(read(&x265_tl3), (0, 3, 320, 240));
        assert_eq!(read(&x265_tl4), (0, 3, 320, 240));
        assert_eq!(read(&h770), (0, 1, 1920, 544));
        for nal in [&x265_tl3, &x265_tl4, &h770] {
            assert_eq!(classify(VideoCodec::H265, nal), Some((Kind::Sps, 0)));
        }
    }

    /// A PES cut short by loss can end on a one-byte NAL, and the bytes come
    /// off the wire: nothing too short to be a parameter set is indexed past
    /// its end, stored, or counted as a random-access point carrying its
    /// kind — which would empty that kind's table.
    #[test]
    fn short_nals_are_ignored_and_never_panic() {
        let cases: [(VideoCodec, u8); 5] = [
            (VideoCodec::H265, 0x40),
            (VideoCodec::H265, 0x42),
            (VideoCodec::H265, 0x44),
            (VideoCodec::H264, 0x67),
            (VideoCodec::H264, 0x68),
        ];
        for (codec, header) in cases {
            for len in 1..=3usize {
                // Bytes that, read further, would name id 0.
                let short: Vec<u8> = [header, 0x01, 0x80][..len].to_vec();
                for rap in [true, false] {
                    let mut c = ParamSetCollector::default();
                    c.observe(codec, std::slice::from_ref(&short), rap);
                    let empty = ParamSets::default();
                    assert_eq!(table(&c, codec), empty, "{header:#x} x{len}, rap {rap}");

                    let full = match codec {
                        VideoCodec::H264 => {
                            au(&[&h264_sps(0, 1), &h264_pps(0, 1), &h264_pps(1, 1)])
                        }
                        VideoCodec::H265 => au(&[
                            &hevc_vps(0, 1),
                            &hevc_sps(0, 1),
                            &hevc_pps(0, 1),
                            &hevc_pps(1, 1),
                        ]),
                    };
                    let mut c = ParamSetCollector::default();
                    c.observe(codec, &full, true);
                    let before = table(&c, codec);
                    let slice = match codec {
                        VideoCodec::H264 => H264_IDR.to_vec(),
                        VideoCodec::H265 => HEVC_IDR.to_vec(),
                    };
                    c.observe(codec, &[short.clone(), slice], rap);
                    assert_eq!(table(&c, codec), before, "{header:#x} x{len}, rap {rap}");
                }
            }
        }
    }

    /// A parameter set that names no id — truncated mid-field, or out of
    /// range — is never stored, so it can neither sit in an init beside the
    /// good ones nor be the reason for a rotation.
    #[test]
    fn a_malformed_parameter_set_is_never_stored() {
        let c = VideoCodec::H264;
        let mut col = ParamSetCollector::default();
        let truncated = vec![0x68, 0x00, 0x00, 0x00]; // ue() runs off the end
        let out_of_range = h264_pps(300, 1);
        let rap = au(&[
            &h264_sps(0, 1),
            &h264_pps(0, 1),
            &h264_pps(1, 1),
            &h264_pps(2, 1),
            &truncated,
            &out_of_range,
            &H264_IDR,
        ]);
        col.observe(c, &rap, true);
        let t = table(&col, c);
        assert_eq!(ids(&t.pps), vec![0, 1, 2]);
        assert!(t.pps.iter().all(|e| e.nal != truncated && e.nal != out_of_range));
        // A clean random-access point leaves exactly its own sets.
        col.observe(c, &au(&[&h264_sps(0, 1), &h264_pps(0, 1), &h264_pps(1, 1), &H264_IDR]), true);
        assert_eq!(ids(&table(&col, c).pps), vec![0, 1]);

        // An HEVC SPS cut off inside its profile_tier_level.
        let sps = hevc_sps(0, 1);
        let mut col = ParamSetCollector::default();
        col.observe(VideoCodec::H265, &[sps[..9].to_vec()], true);
        assert_eq!(table(&col, VideoCodec::H265), ParamSets::default());

        // An id field opening with 32 zero bits. No `00 00 0x` start code
        // ends such a NAL, so the demuxer hands it over whole; read as an
        // Exp-Golomb code it is `1 << 32` — a shift overflow, which panicked
        // the output's task under overflow checks and stored garbage as id 0
        // without them.
        let zeros = [0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0xA5];
        let cases = [
            (VideoCodec::H264, [&[0x68][..], &zeros].concat()),
            (VideoCodec::H264, [&[0x67, 0x64, 0x00, 0x28][..], &zeros].concat()),
            (VideoCodec::H265, [&[0x44, 0x01][..], &zeros].concat()),
        ];
        for (codec, nal) in cases {
            assert_eq!(classify(codec, &nal), None, "{nal:02x?}");
            let mut col = ParamSetCollector::default();
            col.observe(codec, std::slice::from_ref(&nal), true);
            col.observe(codec, std::slice::from_ref(&nal), false);
            assert_eq!(table(&col, codec), ParamSets::default());
        }
    }

    /// A parameter set longer than a configuration record's 16-bit length is
    /// never stored: written into an init it would carry a truncated length,
    /// and nothing else bounds what one entry can hold.
    #[test]
    fn a_parameter_set_too_long_for_the_record_is_never_stored() {
        let c = VideoCodec::H264;
        let sized = |len: usize, tag: u8| {
            let mut nal = h264_pps(1, tag);
            nal.resize(len, 0xA5);
            nal
        };
        assert_eq!(classify(c, &sized(MAX_PARAM_SET_LEN, 1)), Some((Kind::Pps, 1)));
        assert_eq!(classify(c, &sized(MAX_PARAM_SET_LEN + 1, 1)), None);
        let mut col = ParamSetCollector::default();
        col.observe(c, &au(&[&h264_sps(0, 1), &h264_pps(0, 1), &H264_IDR]), true);
        let before = table(&col, c);
        let oversize = sized(MAX_PARAM_SET_LEN + 1, 2);
        // Merged between random-access points, and as the only PPS of a
        // random-access point — which therefore does not carry its kind.
        col.observe(c, &au(&[&oversize, &H264_P]), false);
        assert_eq!(table(&col, c), before);
        col.observe(c, &au(&[&h264_sps(0, 1), &oversize, &H264_IDR]), true);
        assert_eq!(table(&col, c), before);
    }

    #[test]
    fn a_rap_defines_each_kind_it_carries() {
        let c = VideoCodec::H264;
        let mut col = ParamSetCollector::default();
        let rap = au(&[&h264_sps(0, 1), &h264_pps(0, 1), &h264_pps(1, 1), &h264_pps(2, 1), &H264_IDR]);
        col.observe(c, &rap, true);
        let t = table(&col, c);
        assert_eq!((ids(&t.sps), ids(&t.pps)), (vec![0], vec![0, 1, 2]));
        assert!(t.vps.is_empty());
        assert!(col.complete_sets(c).is_some());

        // Between random-access points, a set is merged in by id.
        col.observe(c, &au(&[&h264_pps(3, 1), &H264_P]), false);
        assert_eq!(ids(&table(&col, c).pps), vec![0, 1, 2, 3]);

        // A random-access point carrying a kind replaces it with its own.
        col.observe(c, &au(&[&h264_sps(0, 1), &h264_pps(0, 1), &H264_IDR]), true);
        let t = table(&col, c);
        assert_eq!((ids(&t.sps), ids(&t.pps)), (vec![0], vec![0]));

        // One carrying none leaves everything.
        col.observe(c, &au(&[&H264_IDR]), true);
        assert_eq!(table(&col, c), t);

        // One carrying only some kinds leaves the others: an SPS alone keeps
        // the PPS, a PPS alone keeps the SPS.
        col.observe(c, &au(&[&h264_sps(1, 2), &H264_IDR]), true);
        let t = table(&col, c);
        assert_eq!((ids(&t.sps), ids(&t.pps)), (vec![1], vec![0]));
        col.observe(c, &au(&[&h264_pps(5, 2), &H264_IDR]), true);
        let t = table(&col, c);
        assert_eq!((ids(&t.sps), ids(&t.pps)), (vec![1], vec![5]));

        // Within one access unit the last of an id wins.
        col.observe(c, &au(&[&h264_pps(5, 3), &h264_pps(5, 4), &H264_IDR]), true);
        assert_eq!(table(&col, c).pps, vec![ParamSet { id: 5, nal: h264_pps(5, 4) }]);
    }

    /// Whatever the access units, after each one the table holds the last
    /// parameter set seen of every kind, with its bytes — never less than
    /// the single-slot cache the track used to be built from.
    #[test]
    fn the_last_set_of_each_kind_is_always_kept() {
        let c = VideoCodec::H265;
        // (is_rap, [(kind byte, id, tag)]) — 0 = VPS, 1 = SPS, 2 = PPS.
        let script: &[(bool, &[(u8, u32, u8)])] = &[
            (true, &[(0, 0, 1), (1, 0, 1), (2, 0, 1), (2, 1, 1), (2, 2, 1)]),
            (false, &[]),
            (false, &[(2, 1, 2)]),
            (true, &[(2, 4, 1)]),
            (true, &[(1, 3, 1), (2, 0, 9), (2, 4, 1)]),
            (false, &[(0, 1, 1), (2, 7, 1)]),
            (true, &[]),
            (true, &[(0, 0, 2), (1, 0, 2), (1, 3, 2), (2, 0, 3)]),
            (false, &[(1, 0, 5), (2, 0, 3), (2, 0, 4)]),
            (true, &[(2, 9, 1), (2, 2, 2), (2, 9, 2)]),
        ];
        let make = |k: u8, id: u32, tag: u8| match k {
            0 => hevc_vps(id, tag),
            1 => hevc_sps(id, tag),
            _ => hevc_pps(id, tag),
        };
        let mut col = ParamSetCollector::default();
        let mut last: [Option<(u32, Vec<u8>)>; 3] = [None, None, None];
        for (step, (rap, sets)) in script.iter().enumerate() {
            let mut nals: Vec<Vec<u8>> = sets.iter().map(|&(k, id, tag)| make(k, id, tag)).collect();
            nals.push(if *rap { HEVC_IDR.to_vec() } else { HEVC_TRAIL.to_vec() });
            for &(k, id, tag) in sets.iter() {
                last[k as usize] = Some((id, make(k, id, tag)));
            }
            col.observe(c, &nals, *rap);
            let t = table(&col, c);
            for (k, list) in [&t.vps, &t.sps, &t.pps].into_iter().enumerate() {
                if let Some((id, nal)) = &last[k] {
                    assert!(
                        list.iter().any(|e| e.id == *id && e.nal == *nal),
                        "step {step}: kind {k} lost its last set (id {id})"
                    );
                }
                assert!(list.windows(2).all(|w| w[0].id < w[1].id), "step {step}: sorted, unique");
            }
        }
    }

    /// A parameter set for a layer other than the base layer is not one a
    /// base-layer decoder uses, and must not displace the base layer's.
    #[test]
    fn non_base_layer_hevc_sets_are_ignored() {
        let c = VideoCodec::H265;
        let mut layer1 = hevc_pps(0, 9);
        layer1[1] = (1 << 3) | 1; // nuh_layer_id 1, temporal_id_plus1 1
        let mut layer32 = hevc_pps(0, 8);
        layer32[0] |= 1; // nuh_layer_id 32
        assert_eq!(classify(c, &layer1), None);
        assert_eq!(classify(c, &layer32), None);
        let mut col = ParamSetCollector::default();
        let base = au(&[&hevc_vps(0, 1), &hevc_sps(0, 1), &hevc_pps(0, 1), &HEVC_IDR]);
        col.observe(c, &base, true);
        let before = table(&col, c);
        col.observe(c, &au(&[&layer1, &layer32, &HEVC_IDR]), true);
        assert_eq!(table(&col, c), before);
    }

    #[test]
    fn a_codec_change_resets_the_table() {
        let mut col = ParamSetCollector::default();
        col.observe(VideoCodec::H264, &au(&[&h264_sps(0, 1), &h264_pps(0, 1), &H264_IDR]), true);
        assert!(col.complete_sets(VideoCodec::H264).is_some());
        assert!(col.sets(VideoCodec::H265).is_none());
        col.observe(VideoCodec::H265, &au(&[&hevc_pps(0, 1), &HEVC_TRAIL]), false);
        assert!(col.sets(VideoCodec::H264).is_none());
        let t = table(&col, VideoCodec::H265);
        assert_eq!((ids(&t.vps), ids(&t.sps), ids(&t.pps)), (vec![], vec![], vec![0]));
        assert!(col.complete_sets(VideoCodec::H265).is_none(), "no VPS or SPS yet");
    }

    /// `avcC` counts SPS in five bits and PPS in eight; a source past either
    /// keeps the first ids it sent that fit rather than writing a count that
    /// wraps — the lowest ones, for a source sending them in id order.
    #[test]
    fn the_avcc_caps_hold() {
        let c = VideoCodec::H264;
        let mut nals: Vec<Vec<u8>> = (0..=31).map(|id| h264_sps(id, 1)).collect();
        nals.extend((0..=255).map(|id| h264_pps(id, 1)));
        nals.push(H264_IDR.to_vec());
        let mut col = ParamSetCollector::default();
        col.observe(c, &nals, true);
        let t = table(&col, c);
        assert_eq!(t.sps.len(), AVCC_MAX_SPS);
        assert_eq!(t.pps.len(), AVCC_MAX_PPS);
        assert_eq!(ids(&t.pps), (0..255).collect::<Vec<u32>>());
    }

    #[test]
    fn contains_all_is_a_subset_check() {
        let set = |pps: &[(u32, u8)]| ParamSets {
            vps: Vec::new(),
            sps: vec![ParamSet { id: 0, nal: h264_sps(0, 1) }],
            pps: pps.iter().map(|&(id, tag)| ParamSet { id, nal: h264_pps(id, tag) }).collect(),
        };
        let track = set(&[(0, 1), (1, 1), (2, 1)]);
        assert!(track.contains_all(&track));
        assert!(track.contains_all(&set(&[(0, 1), (1, 1)])), "an omission is not a change");
        assert!(!track.contains_all(&set(&[(0, 1), (1, 2)])), "changed bytes are");
        assert!(!track.contains_all(&set(&[(0, 1), (3, 1)])), "so is a new id");
        let mut other_sps = set(&[(0, 1)]);
        other_sps.sps[0].nal = h264_sps(0, 2);
        assert!(!track.contains_all(&other_sps));
    }

    /// `single` keeps one entry of each kind whatever the bytes — the
    /// re-encoder's and the tests' tracks — and no VPS for H.264.
    #[test]
    fn single_keeps_one_of_each_whatever_the_bytes() {
        let s = ParamSets::single(
            VideoCodec::H264,
            vec![0x40],
            vec![0x67, 0x42, 0xC0, 0x1E],
            vec![0x68, 0xCE],
        );
        assert!(s.vps.is_empty());
        assert_eq!(s.sps, vec![ParamSet { id: 0, nal: vec![0x67, 0x42, 0xC0, 0x1E] }]);
        assert_eq!(s.pps, vec![ParamSet { id: 0, nal: vec![0x68, 0xCE] }]);
        let s = ParamSets::single(VideoCodec::H265, Vec::new(), hevc_sps(4, 1), hevc_pps(9, 1));
        assert_eq!((ids(&s.vps), ids(&s.sps), ids(&s.pps)), (vec![0], vec![4], vec![9]));
        assert!(s.vps[0].nal.is_empty());
        assert!(s.is_complete(VideoCodec::H265));
    }
}
