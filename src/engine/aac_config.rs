// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The AudioSpecificConfig a raw-AAC container needs for an AAC stream that
//! arrived in ADTS, including the **program_config_element** (PCE) a stream
//! with `channel_configuration` 0 describes its channels in.
//!
//! FLV (`AACPacketType` 0) and MP4 (`esds`) carry the decoder's configuration
//! once, out of band, as an AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1).
//! For channel configurations 1–7 two bytes say everything. Channel
//! configuration 0 says "the layout is in a PCE", and the configuration must
//! then carry that PCE (GASpecificConfig, §4.4.1): the fixed two bytes it used
//! to get are a header no decoder opens — ffmpeg stops at "decode_pce: Input
//! buffer exhausted before END element found", GStreamer and faad refuse it.
//! An ADTS stream carries the PCE in band instead, as the first element of a
//! raw_data_block, and not necessarily in every frame: on ISDB-T 1seg (NHK,
//! HE-AAC, a 24 kHz core) about one frame in twelve opens with it, and the
//! first frames of a capture need not. So a writer holds its header back until
//! a frame that opens with the PCE has been seen ([`AacConfig::from_adts`]),
//! and copies it bit for bit, as FFmpeg's `aac_adtstoasc` filter does
//! (`ff_copy_pce_data`), into the configuration ([`AacConfig::audio_specific_config`]).
//! Once the header carries it, every frame decodes, with or without a PCE of
//! its own, so the frames themselves go out as they came.
//!
//! The other direction, [`adts_with_pce`], is for the encoder: fdk-aac's 7.1
//! mode is channel configuration 12, which the 3-bit ADTS field cannot hold —
//! fdk writes it masked to 4 (4.0) and no PCE — so the edge rewrites such a
//! frame to channel configuration 0 with the PCE fdk itself would write for
//! that mode ([`pce_7_1_back`]).

/// `id_syn_ele` of a program_config_element (ISO/IEC 14496-3 Table 4.85).
const ID_PCE: u32 = 5;

/// How long a writer holds a PCE-signalled stream back, waiting for a frame
/// that carries its PCE, before it says so ([`LayoutWait`]). A source that
/// repeats it at all repeats it far more often than this (ISDB-T 1seg: about
/// every half second).
pub const LAYOUT_WAIT_WARN_SECS: u64 = 5;

/// MSB-first reader over a byte slice, positioned in bits.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next `n` (at most 16) bits, `None` past the end.
    fn read(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    /// On to the next byte boundary of the slice.
    fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

/// MSB-first writer, the counterpart of [`BitReader`].
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.out.push(0);
            }
            if (v >> i) & 1 != 0 {
                let last = self.out.len() - 1;
                self.out[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    /// Zero bits up to the next byte boundary of what has been written.
    fn align(&mut self) {
        self.bits = self.bits.div_ceil(8) * 8;
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Copy `n` bits from `r` to `w`, returning them.
fn copy(r: &mut BitReader<'_>, w: &mut BitWriter, n: u32) -> Option<u32> {
    let v = r.read(n)?;
    w.put(n, v);
    Some(v)
}

/// A program_config_element (ISO/IEC 14496-3 §4.4.1.1), as an
/// AudioSpecificConfig carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramConfig {
    /// Every field after the 3-bit element id, laid out from a byte boundary
    /// with its `byte_alignment()` taken from there — which is where it sits
    /// in an AudioSpecificConfig, after the two bytes before it.
    bytes: Vec<u8>,
    /// The channels its front, side, back and LFE elements carry: one for a
    /// single channel element, two for a pair, one for each LFE.
    channels: u8,
}

impl ProgramConfig {
    /// The PCE `raw` (one raw_data_block, its ADTS header taken off) opens
    /// with, copied field by field as FFmpeg's `ff_copy_pce_data` copies it;
    /// `None` when the block does not open with one, when it is cut short, or
    /// when it names no channel. Its `byte_alignment()` is relative to the
    /// start of the block in a raw_data_block and to the start of the
    /// configuration in an AudioSpecificConfig, so the copy re-aligns.
    pub fn from_raw_data_block(raw: &[u8]) -> Option<Self> {
        let mut reader = BitReader::new(raw);
        if reader.read(3)? != ID_PCE {
            return None;
        }
        let mut writer = BitWriter::default();
        let (r, w) = (&mut reader, &mut writer);
        // element_instance_tag, object_type, sampling_frequency_index.
        copy(r, w, 10)?;
        let front = copy(r, w, 4)?;
        let side = copy(r, w, 4)?;
        let back = copy(r, w, 4)?;
        let lfe = copy(r, w, 2)?;
        let assoc_data = copy(r, w, 3)?;
        let cc = copy(r, w, 4)?;
        // Mono and stereo mixdown element numbers, the matrix mixdown index
        // and pseudo-surround flag, each behind its present flag.
        for width in [4, 4, 3] {
            if copy(r, w, 1)? == 1 {
                copy(r, w, width)?;
            }
        }
        let mut channels = 0u32;
        for _ in 0..front + side + back {
            channels += 1 + copy(r, w, 1)?;
            copy(r, w, 4)?;
        }
        for _ in 0..lfe {
            copy(r, w, 4)?;
            channels += 1;
        }
        for _ in 0..assoc_data {
            copy(r, w, 4)?;
        }
        for _ in 0..cc {
            copy(r, w, 5)?;
        }
        r.align();
        w.align();
        let comment = copy(r, w, 8)?;
        for _ in 0..comment {
            copy(r, w, 8)?;
        }
        if channels == 0 {
            return None;
        }
        Some(Self { bytes: writer.finish(), channels: channels.min(u32::from(u8::MAX)) as u8 })
    }

    /// The channel count the element describes.
    pub fn channels(&self) -> u8 {
        self.channels
    }
}

/// One element of a PCE's front, side or back list: a channel pair or a
/// single channel, and its `element_instance_tag`.
#[derive(Clone, Copy, Debug)]
pub struct PceElement {
    pub is_cpe: bool,
    pub tag: u8,
}

/// The channel elements a PCE written by [`raw_block_pce`] describes.
#[derive(Clone, Copy, Debug)]
pub struct PceLayout<'a> {
    pub front: &'a [PceElement],
    pub side: &'a [PceElement],
    pub back: &'a [PceElement],
    /// The `element_instance_tag` of each LFE element.
    pub lfe: &'a [u8],
}

/// A program_config_element for `layout`, with its 3-bit element id, as it
/// opens a raw_data_block: instance tag 0, `profile` (the ADTS profile, the
/// audio object type less one) and `sr_idx`, no mixdown, data or coupling
/// elements, aligned from the start of the block and an empty comment —
/// a whole number of bytes, so the elements after it in the block keep
/// their bits.
pub fn raw_block_pce(profile: u8, sr_idx: u8, layout: &PceLayout<'_>) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(3, ID_PCE);
    w.put(4, 0);
    w.put(2, u32::from(profile & 0x03));
    w.put(4, u32::from(sr_idx & 0x0F));
    w.put(4, layout.front.len() as u32);
    w.put(4, layout.side.len() as u32);
    w.put(4, layout.back.len() as u32);
    w.put(2, layout.lfe.len() as u32);
    // No associated data or coupling elements; no mono, stereo or matrix
    // mixdown.
    w.put(3, 0);
    w.put(4, 0);
    w.put(3, 0);
    for e in layout.front.iter().chain(layout.side).chain(layout.back) {
        w.put(1, u32::from(e.is_cpe));
        w.put(4, u32::from(e.tag & 0x0F));
    }
    for &tag in layout.lfe {
        w.put(4, u32::from(tag & 0x0F));
    }
    w.align();
    w.put(8, 0);
    w.finish()
}

/// The PCE fdk-aac writes for its 7.1 mode (`MODE_7_1_BACK`, channel
/// configuration 12) when it writes one — its `elType_7_1_back` table and
/// tag numbering (`transportEnc_writePCE`): front a single channel element
/// (centre) and a pair (left / right), back two pairs (the surround pair,
/// then the rear pair), one LFE. The element tags match the ones its
/// encoder puts in the raw_data_block: SCE 0, CPE 0, 1 and 2, LFE 0.
pub fn pce_7_1_back(profile: u8, sr_idx: u8) -> Vec<u8> {
    raw_block_pce(
        profile,
        sr_idx,
        &PceLayout {
            front: &[PceElement { is_cpe: false, tag: 0 }, PceElement { is_cpe: true, tag: 0 }],
            side: &[],
            back: &[PceElement { is_cpe: true, tag: 1 }, PceElement { is_cpe: true, tag: 2 }],
            lfe: &[0],
        },
    )
}

/// `frame` (one whole ADTS frame of one raw data block, no CRC) rewritten to
/// channel configuration 0 with `pce` (from [`raw_block_pce`]) opening its
/// raw_data_block, its length grown to match. `None` for a frame of any other
/// shape, which is left as it is.
#[cfg_attr(not(feature = "fdk-aac"), allow(dead_code))]
pub fn adts_with_pce(frame: &[u8], pce: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 7 || frame[0] != 0xFF || frame[1] & 0xF7 != 0xF1 || frame[6] & 0x03 != 0 {
        return None;
    }
    let len = (usize::from(frame[3] & 0x03) << 11) | (usize::from(frame[4]) << 3) | (usize::from(frame[5]) >> 5);
    let new_len = len + pce.len();
    if len != frame.len() || new_len >= 1 << 13 {
        return None;
    }
    let mut out = Vec::with_capacity(new_len);
    out.extend_from_slice(&frame[..7]);
    // channel_configuration: the low bit of byte 2, the top two of byte 3.
    out[2] &= !0x01;
    out[3] &= 0x3F;
    out[3] = (out[3] & !0x03) | ((new_len >> 11) & 0x03) as u8;
    out[4] = (new_len >> 3) as u8;
    out[5] = (out[5] & 0x1F) | (((new_len & 0x07) as u8) << 5);
    out.extend_from_slice(pce);
    out.extend_from_slice(&frame[7..]);
    Some(out)
}

/// The decoder configuration of an AAC stream that arrived in ADTS: the
/// header's `(profile, sampling_frequency_index, channel_configuration)` and,
/// for channel configuration 0, the PCE the layout is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// The ADTS profile: the audio object type less one (1 = AAC-LC, which
    /// an HE-AAC stream in ADTS carries too, SBR / PS signalled implicitly).
    pub profile: u8,
    pub sr_idx: u8,
    pub ch_cfg: u8,
    pce: Option<ProgramConfig>,
}

impl AacConfig {
    /// The configuration an ADTS header names alone — `None` for channel
    /// configuration 0, which needs its PCE ([`Self::from_adts`]).
    pub fn new(profile: u8, sr_idx: u8, ch_cfg: u8) -> Option<Self> {
        (ch_cfg != 0).then_some(Self { profile, sr_idx, ch_cfg, pce: None })
    }

    /// The configuration of a frame: its ADTS header's fields and `raw`, its
    /// raw_data_block. For channel configuration 0, `None` unless the block
    /// opens with the PCE ([`ProgramConfig::from_raw_data_block`]).
    pub fn from_adts(profile: u8, sr_idx: u8, ch_cfg: u8, raw: &[u8]) -> Option<Self> {
        if ch_cfg != 0 {
            return Self::new(profile, sr_idx, ch_cfg);
        }
        let pce = ProgramConfig::from_raw_data_block(raw)?;
        Some(Self { profile, sr_idx, ch_cfg, pce: Some(pce) })
    }

    /// The configuration with the PCE a stream signalled its layout in.
    pub fn with_pce(profile: u8, sr_idx: u8, pce: ProgramConfig) -> Self {
        Self { profile, sr_idx, ch_cfg: 0, pce: Some(pce) }
    }

    /// The AudioSpecificConfig: `audioObjectType` (5 bits),
    /// `samplingFrequencyIndex` (4), `channelConfiguration` (4), then the
    /// GASpecificConfig — 1024-sample frames, no core coder, no extension
    /// (3 zero bits) — and, for channel configuration 0, the PCE. Two bytes
    /// otherwise, exactly as before.
    pub fn audio_specific_config(&self) -> Vec<u8> {
        let aot = (self.profile + 1) & 0x1F;
        let mut asc = vec![(aot << 3) | ((self.sr_idx >> 1) & 0x07), ((self.sr_idx & 0x01) << 7) | ((self.ch_cfg & 0x0F) << 3)];
        if let Some(pce) = &self.pce {
            asc.extend_from_slice(&pce.bytes);
        }
        asc
    }

    /// The channels the core carries (what the configuration says, before an
    /// HE-AAC v2 decoder widens a mono core to stereo).
    pub fn channels(&self) -> u8 {
        match (&self.pce, self.ch_cfg) {
            (Some(pce), _) => pce.channels(),
            (None, 7) => 8,
            (None, c) => c,
        }
    }

    /// The core's sample rate, `None` for a reserved index.
    pub fn sample_rate(&self) -> Option<u32> {
        super::audio_decode::sample_rate_from_index(self.sr_idx)
    }

    /// The configuration in words, for an operator: `AOT 2, 48000 Hz, 2 ch`,
    /// with `, channel_configuration 0 (PCE)` after it for a PCE-signalled
    /// one. The rate is the core's, which an HE-AAC stream doubles.
    pub fn describe(&self) -> String {
        let rate = self.sample_rate().unwrap_or(0);
        let pce = if self.pce.is_some() { ", channel_configuration 0 (PCE)" } else { "" };
        format!("AOT {}, {rate} Hz, {} ch{pce}", self.profile + 1, self.channels())
    }
}

/// How long a writer has held a PCE-signalled stream back for want of a frame
/// carrying its PCE, so that it says so once the wait passes
/// [`LAYOUT_WAIT_WARN_SECS`] rather than publishing a header no decoder can
/// open, or nothing, without a word.
#[derive(Debug, Default)]
pub struct LayoutWait {
    /// Samples held back, at the core rate (each ADTS frame is 1024).
    held_samples: u64,
    warned: bool,
}

impl LayoutWait {
    /// One frame at `sample_rate` held back: `true` the once the wait first
    /// passes [`LAYOUT_WAIT_WARN_SECS`].
    pub fn hold(&mut self, sample_rate: u32) -> bool {
        self.held_samples += 1024;
        if self.warned || self.held_samples < LAYOUT_WAIT_WARN_SECS * u64::from(sample_rate.max(1)) {
            return false;
        }
        self.warned = true;
        true
    }

    /// The layout arrived: a later wait is counted, and said, afresh.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Seconds held back so far.
    pub fn held_secs(&self, sample_rate: u32) -> u64 {
        self.held_samples / u64::from(sample_rate.max(1))
    }
}

/// Raise `aac_channel_layout_missing`: `kind` output `output_id` (`category`
/// its event category) has held a PCE-signalled AAC source back for
/// `held_secs` because no frame carried the PCE its header needs.
pub fn emit_layout_missing(
    sender: &crate::manager::events::EventSender,
    category: &str,
    kind: &str,
    output_id: &str,
    flow_id: &str,
    held_secs: u64,
) {
    let message = format!(
        "{kind} output '{output_id}': the source's AAC signals its channel layout in a \
         program_config_element (channel_configuration 0) and no frame in {held_secs} s has \
         carried one; its audio is held back rather than published with a header no decoder \
         can open"
    );
    tracing::warn!(error_code = "aac_channel_layout_missing", "{message}");
    sender.emit_output_with_details(
        crate::manager::events::EventSeverity::Warning,
        category,
        message,
        output_id,
        serde_json::json!({
            "error_code": "aac_channel_layout_missing",
            "held_secs": held_secs,
            "flow_id": flow_id,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PCE that opens NHK's PID 1411 frames (ISDB-T 1seg, HE-AAC, a
    /// 24 kHz core: two front single channel elements, tags 0 and 1), as the
    /// first bytes of the raw_data_block, followed by the SCE after it.
    const NHK_BLOCK: &[u8] = &[
        0xa0, 0xb1, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0x36, 0x12, 0x2a, 0x32, 0xd0, 0x11, 0x91,
        0x66,
    ];

    #[test]
    fn the_pce_a_block_opens_with_is_copied_re_aligned() {
        let pce = ProgramConfig::from_raw_data_block(NHK_BLOCK).expect("a PCE");
        // tag 0, object type 1 (LC), index 6 (24 kHz), two front single
        // channel elements (tags 0 and 1), nothing else: 44 bits after the
        // id, aligned to 48 from a byte boundary, then an empty comment. In
        // the block the same fields end on bit 47, aligned to 48 from the
        // block's start, so the SCE after it starts at byte 7.
        assert_eq!(pce.channels(), 2);
        assert_eq!(pce.bytes, [0x05, 0x88, 0x00, 0x00, 0x00, 0x10, 0x00]);
        let cfg = AacConfig::with_pce(1, 6, pce);
        assert_eq!(cfg.audio_specific_config(), [0x13, 0x00, 0x05, 0x88, 0x00, 0x00, 0x00, 0x10, 0x00]);
        assert_eq!(cfg.channels(), 2);
    }

    #[test]
    fn a_block_opening_with_anything_else_has_no_pce() {
        // A single channel element (id 0), as most of NHK's frames open.
        assert_eq!(ProgramConfig::from_raw_data_block(&[0x01, 0x3c, 0x12, 0x18]), None);
        assert_eq!(AacConfig::from_adts(1, 6, 0, &[0x01, 0x3c, 0x12, 0x18]), None);
        // Cut short inside the PCE.
        assert_eq!(ProgramConfig::from_raw_data_block(&NHK_BLOCK[..4]), None);
        assert_eq!(ProgramConfig::from_raw_data_block(&[]), None);
    }

    #[test]
    fn a_header_naming_its_layout_needs_no_pce() {
        let cfg = AacConfig::from_adts(1, 3, 2, &[0x21]).expect("stereo");
        assert_eq!(cfg.audio_specific_config(), [0x11, 0x90], "the two bytes it always was");
        assert_eq!(cfg.channels(), 2);
        assert_eq!(AacConfig::new(1, 3, 7).unwrap().channels(), 8);
        assert_eq!(AacConfig::new(1, 6, 0), None, "channel configuration 0 needs its PCE");
    }

    /// A PCE the writer builds reads back the same, through a block that
    /// starts with it and carries more after it: the copy is the PCE alone,
    /// and the block's own elements start on the byte after it.
    #[test]
    fn a_written_pce_reads_back() {
        let pce = pce_7_1_back(1, 3);
        let mut block = pce.clone();
        block.extend_from_slice(&[0x00, 0x11, 0x22]);
        let read = ProgramConfig::from_raw_data_block(&block).expect("a PCE");
        assert_eq!(read.channels(), 8, "C, L / R, Ls / Rs, Lr / Rr, LFE");
        // 3 + 34 fixed bits, 4 elements of 5 bits and an LFE tag: 61 bits,
        // aligned to 64, then the 8-bit empty comment.
        assert_eq!(pce.len(), 9);
        let cfg = AacConfig::with_pce(1, 3, read);
        let asc = cfg.audio_specific_config();
        assert_eq!(&asc[..2], [0x11, 0x80], "AAC-LC, 48 kHz, channel configuration 0");
        // Without its id the same fields take 58 bits, aligned to 64: still
        // nine bytes with the comment.
        assert_eq!(asc.len(), 2 + 9);
    }

    /// The bounded hold says so once, at five seconds of the core's audio,
    /// and again only after the layout came and went.
    #[test]
    fn a_layout_wait_says_so_once() {
        let mut wait = LayoutWait::default();
        let frames_5s = (5 * 24_000u64).div_ceil(1024);
        let fired: Vec<u64> = (1..=frames_5s * 3).filter(|_| wait.hold(24_000)).collect();
        assert_eq!(fired, [frames_5s]);
        assert_eq!(wait.held_secs(24_000), 15);
        wait.clear();
        assert!(!wait.hold(24_000));
        assert_eq!(wait.held_secs(24_000), 0);
    }

    #[test]
    fn a_pce_goes_into_a_whole_adts_frame() {
        // A 7-byte header (no CRC, one raw data block), AAC-LC 48 kHz, the
        // channel configuration fdk writes for 7.1 (12, masked to 4), and a
        // three-byte body.
        let mut frame = vec![0xFF, 0xF1, 0x4D, 0x00, 0x00, 0x00, 0xFC, 0xAA, 0xBB, 0xCC];
        frame[3] |= ((frame.len() >> 11) & 0x03) as u8;
        frame[4] = (frame.len() >> 3) as u8;
        frame[5] = ((frame.len() & 0x07) as u8) << 5 | 0x1F;
        let pce = pce_7_1_back(1, 3);
        let out = adts_with_pce(&frame, &pce).expect("rewritten");
        let crate::engine::audio_au::Head::Valid(h) =
            crate::engine::audio_au::parse_header(crate::engine::audio_au::AuFormat::Adts, &out)
        else {
            panic!("not an ADTS frame");
        };
        assert_eq!(h.len, out.len(), "the length counts the PCE");
        assert_eq!(h.key & 0x07, 0, "channel configuration 0");
        assert_eq!(h.key >> 4, 0x13, "profile (LC) and rate (48 kHz) unchanged");
        assert_eq!(&out[7..7 + pce.len()], &pce[..]);
        assert_eq!(&out[7 + pce.len()..], &frame[7..], "the elements after it unchanged");
        assert_eq!(out[5] & 0x1F, 0x1F, "buffer fullness unchanged");
        // A frame with a CRC, or cut short, is left alone.
        let mut crc = frame.clone();
        crc[1] &= !0x01;
        assert_eq!(adts_with_pce(&crc, &pce), None);
        assert_eq!(adts_with_pce(&frame[..9], &pce), None);
    }
}
