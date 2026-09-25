// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared PMT editing for every stage that rewrites a PMT: the audio and
//! video transcode replacers, the role-keyed and mechanical PID rewriters,
//! and the HLS audio remux.
//!
//! Three pieces:
//!
//! 1. [`parse_pmt`] / [`rebuild_pmt_section`] — parse a complete PMT
//!    section and rebuild it with new stream_types, a per-target
//!    descriptor policy, an optional forced `PCR_PID`, a recomputed
//!    `section_length` and CRC. Before this module each rewriter flipped
//!    the `stream_type` byte in place and copied the ES_info loop verbatim,
//!    so an MP2 output kept the source's AAC descriptor (0x7C), an AC-3
//!    output kept the source's MPEG audio_stream_descriptor (0x03) and a
//!    128 kbps `maximum_bitrate_descriptor` on a 448 kbps stream, and an
//!    H.264 output kept the MPEG-2 video_stream_descriptor.
//! 2. [`OutVersion`] — the output PMT's `version_number`, derived from
//!    content: it bumps whenever the rebuilt section differs from the last
//!    one (and on a source reset), and otherwise stays put. A private
//!    counter that only bumped on the replacer's own resets left the
//!    version pinned while a source PMT update changed the content, and
//!    the second replacer of a chain re-stamped over the first one's bump.
//! 3. [`PsiUnitStage`] — the PMT-PID stage. It reassembles each payload
//!    unit with continuity checking, holds the unit's source packets until
//!    every section that started in it is complete, hands the complete
//!    sections to the caller, and re-packetises only when a section
//!    changed. One code path covers a single-packet PMT, a packet carrying
//!    several sections (VH1's 0xC0 section ahead of the PMT) and a PMT
//!    spanning several packets — the case that used to leave the CRC in
//!    the continuation packet stale after the first packet was edited.

use std::ops::Range;

use super::ts_parse::{
    max_section_length, mpeg2_crc32, ts_cc, ts_payload_offset, ts_pusi, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};

// ───────────────────────────── PMT view ─────────────────────────────

/// One elementary-stream entry of a parsed PMT. Ranges index the section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PmtEs {
    pub stream_type: u8,
    pub pid: u16,
    /// The ES_info descriptor loop.
    pub info: Range<usize>,
}

/// A complete, well-formed PMT section.
#[derive(Clone, Debug)]
pub struct PmtView<'a> {
    pub section: &'a [u8],
    pub version: u8,
    pub pcr_pid: u16,
    /// The program_info descriptor loop.
    pub program_info: Range<usize>,
    pub es: Vec<PmtEs>,
}

impl PmtView<'_> {
    pub fn es_info(&self, es: &PmtEs) -> &[u8] {
        &self.section[es.info.clone()]
    }
    pub fn program_info_bytes(&self) -> &[u8] {
        &self.section[self.program_info.clone()]
    }
}

/// Parse a complete PMT section (starting at `table_id`). Strict: returns
/// `None` unless the section is a long-form 0x02 section whose program_info
/// and every ES entry fit exactly inside the body — an editor must never
/// rebuild from a table it only half understood.
pub fn parse_pmt(section: &[u8]) -> Option<PmtView<'_>> {
    if section.len() < 16 || section[0] != 0x02 || section[1] & 0x80 == 0 {
        return None;
    }
    let section_length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
    if !(13..=super::ts_parse::PSI_MAX_SECTION_LENGTH).contains(&section_length)
        || 3 + section_length > section.len()
    {
        return None;
    }
    let body_end = 3 + section_length - 4;
    let version = (section[5] >> 1) & 0x1F;
    let pcr_pid = (((section[8] & 0x1F) as u16) << 8) | section[9] as u16;
    let pil = (((section[10] & 0x0F) as usize) << 8) | section[11] as usize;
    let program_info = 12..12 + pil;
    if program_info.end > body_end {
        return None;
    }
    let mut es = Vec::new();
    let mut pos = program_info.end;
    while pos < body_end {
        if pos + 5 > body_end {
            return None;
        }
        let stream_type = section[pos];
        let pid = (((section[pos + 1] & 0x1F) as u16) << 8) | section[pos + 2] as u16;
        let info_len = (((section[pos + 3] & 0x0F) as usize) << 8) | section[pos + 4] as usize;
        let info = pos + 5..pos + 5 + info_len;
        if info.end > body_end {
            return None;
        }
        pos = info.end;
        es.push(PmtEs { stream_type, pid, info });
    }
    Some(PmtView { section, version, pcr_pid, program_info, es })
}

/// True when `section` is a long-form PMT for `program_number`.
pub fn is_pmt_for(section: &[u8], program_number: u16) -> bool {
    section.len() >= 5
        && section[0] == 0x02
        && section[1] & 0x80 != 0
        && u16::from_be_bytes([section[3], section[4]]) == program_number
}

/// Index of `program`'s PMT among a unit's sections. An exact
/// program_number match wins; when the PAT maps no other program to the
/// PID (`pid_shared == false`) the first long-form PMT is accepted too, as
/// every parser did before, so a mux whose PMT program_number disagrees
/// with its PAT keeps working.
pub fn pmt_index(sections: &[Vec<u8>], program: u16, pid_shared: bool) -> Option<usize> {
    sections.iter().position(|s| is_pmt_for(s, program)).or_else(|| {
        if pid_shared {
            None
        } else {
            sections
                .iter()
                .position(|s| s.len() >= 5 && s[0] == 0x02 && s[1] & 0x80 != 0)
        }
    })
}

/// Walk a descriptor loop, yielding `(tag, body)`. Stops at the first
/// descriptor that overruns the loop.
pub fn descriptors(loop_bytes: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 2 > loop_bytes.len() {
            return None;
        }
        let tag = loop_bytes[pos];
        let len = loop_bytes[pos + 1] as usize;
        if pos + 2 + len > loop_bytes.len() {
            return None;
        }
        let body = &loop_bytes[pos + 2..pos + 2 + len];
        pos += 2 + len;
        Some((tag, body))
    })
}

// ───────────────────────── Signalling flavour ─────────────────────────

/// Which carriage convention an AC-3 output ES uses in the PMT.
///
/// - `Dvb` (ETSI EN 300 468 / TS 101 154): `stream_type 0x06` +
///   `registration_descriptor "AC-3"` + `AC-3_descriptor (0x6A)`.
/// - `Atsc` (A/52 Annex A, and what Apple HLS / hls.js expect):
///   `stream_type 0x81` + `registration_descriptor "AC-3"`.
///
/// This matches FFmpeg's `mpegtsenc`, which always writes the "AC-3"
/// registration and adds 0x6A only for DVB (`system_b`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsFlavour {
    Dvb,
    Atsc,
}

/// Infer the signalling convention of a source PMT. Evidence order:
/// 1. an audio ES already carried DVB-style (`0x06` + 0x6A / 0x7A / 0x7C)
///    ⇒ DVB;
/// 2. ATSC evidence — `stream_type` 0x81 / 0x87, ATSC descriptor tags
///    0x81 / 0x86 / 0xCC / 0xA3 not governed by a DVB
///    private_data_specifier, or a "GA94" registration ⇒ ATSC;
/// 3. DVB descriptor tags (0x45, 0x46, 0x52, 0x56, 0x59, 0x5F, 0x66, 0x6A,
///    0x7A, 0x7B, 0x7C, 0x7F) ⇒ DVB;
/// 4. otherwise ATSC, which is the AC-3 signalling every release before
///    this one emitted.
///
/// A user-private table_id on the PMT PID is deliberately NOT evidence:
/// such tables occur on DVB muxes too.
pub fn detect_flavour(view: &PmtView<'_>) -> TsFlavour {
    // Rule 1.
    for es in &view.es {
        if es.stream_type == 0x06
            && descriptors(view.es_info(es)).any(|(t, _)| matches!(t, 0x6A | 0x7A | 0x7C))
        {
            return TsFlavour::Dvb;
        }
    }
    let loops: Vec<&[u8]> = std::iter::once(view.program_info_bytes())
        .chain(view.es.iter().map(|e| view.es_info(e)))
        .collect();
    // Rule 2.
    let atsc_st = view.es.iter().any(|e| matches!(e.stream_type, 0x81 | 0x87));
    let atsc_tags = loops.iter().any(|l| {
        let mut pds = false;
        descriptors(l).any(|(t, b)| {
            if t == 0x5F {
                pds = true;
            }
            (!pds && matches!(t, 0x81 | 0x86 | 0xCC | 0xA3)) || (t == 0x05 && b.starts_with(b"GA94"))
        })
    });
    if atsc_st || atsc_tags {
        return TsFlavour::Atsc;
    }
    // Rule 3.
    let dvb_tags = loops.iter().any(|l| {
        descriptors(l).any(|(t, _)| {
            matches!(
                t,
                0x45 | 0x46 | 0x52 | 0x56 | 0x59 | 0x5F | 0x66 | 0x6A | 0x7A | 0x7B | 0x7C | 0x7F
            )
        })
    });
    if dvb_tags { TsFlavour::Dvb } else { TsFlavour::Atsc }
}

// ───────────────────────── Descriptor policy ─────────────────────────

/// Target of a replaced audio ES.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioTarget {
    /// AAC-LC / HE-AAC v1 / v2 in ADTS.
    Aac,
    /// MPEG-1 / MPEG-2 (low sampling frequency) Layer II.
    Mp2 { lsf: bool },
    /// AC-3, signalled per the flavour.
    Ac3 { flavour: TsFlavour },
}

impl AudioTarget {
    pub fn stream_type(self) -> u8 {
        match self {
            AudioTarget::Aac => 0x0F,
            AudioTarget::Mp2 { lsf: false } => 0x03,
            AudioTarget::Mp2 { lsf: true } => 0x04,
            AudioTarget::Ac3 { flavour: TsFlavour::Dvb } => 0x06,
            AudioTarget::Ac3 { flavour: TsFlavour::Atsc } => 0x81,
        }
    }
}

/// What to do to one ES entry of the rebuilt PMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EsEdit {
    /// Re-encoded audio: new stream_type and the audio descriptor policy.
    Audio { pid: u16, target: AudioTarget },
    /// Re-encoded video: new stream_type and the video descriptor policy.
    Video { pid: u16, stream_type: u8 },
    /// PID rename only (descriptor loop and stream_type byte-identical).
    Repid { pid: u16, new_pid: u16 },
}

impl EsEdit {
    fn pid(&self) -> u16 {
        match *self {
            EsEdit::Audio { pid, .. } | EsEdit::Video { pid, .. } | EsEdit::Repid { pid, .. } => pid,
        }
    }
    fn re_encodes(&self) -> bool {
        !matches!(self, EsEdit::Repid { .. })
    }
}

/// Descriptors appended to an AC-3 target: the "AC-3" registration (always)
/// and the DVB AC-3_descriptor with no optional fields (DVB flavour only).
const AC3_REGISTRATION: [u8; 6] = [0x05, 0x04, b'A', b'C', b'-', b'3'];
const DVB_AC3_DESCRIPTOR: [u8; 3] = [0x6A, 0x01, 0x00];

/// Apply the audio policy to a replaced ES's descriptor loop.
///
/// Kept: everything that does not describe the codec — ISO 639 (0x0A),
/// stream_identifier (0x52), private_data_specifier (0x5F) and the private
/// descriptors it governs, supplementary_audio and other non-codec 0x7F
/// extensions. Dropped: maximum_bitrate (0x0E, it describes the source
/// rate), the ES-level CA descriptor (0x09 — a re-encoded ES leaves in the
/// clear), and every codec-identity descriptor that does not describe the
/// target, using the same tag set as `ts_parse::descriptor_audio_kind` so
/// the rewriter and the classifier cannot drift: 0x03, 0x6A, 0x7A, 0x7B,
/// 0x7C, 0x7F ext {0x0E DTS-HD, 0x0F DTS Neural, 0x15 AC-4, 0x21 DTS-UHD},
/// ATSC 0x81 / 0xCC / 0xAC (unless a private_data_specifier governs them),
/// and any 0x05 registration whose identifier is not the target's.
/// Per target: AAC normalises a 0x7C to `7C 01 FE` (never adds one); MP2
/// keeps a 0x03 rewritten to `03 01 67` (`03 01 27` for LSF); AC-3 always
/// carries the "AC-3" registration and, DVB-flavoured, `6A 01 00`.
fn audio_descriptors(src: &[u8], target: AudioTarget, with_additions: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + 9);
    let mut pds = false;
    let mut has_ac3_reg = false;
    for (tag, body) in descriptors(src) {
        match tag {
            0x5F => {
                pds = true;
                push_desc(&mut out, tag, body);
            }
            0x0E | 0x09 | 0x6A | 0x7A | 0x7B => {}
            0x03 => {
                if let AudioTarget::Mp2 { lsf } = target {
                    out.extend_from_slice(&[0x03, 0x01, if lsf { 0x27 } else { 0x67 }]);
                }
            }
            0x7C => {
                if target == AudioTarget::Aac {
                    out.extend_from_slice(&[0x7C, 0x01, 0xFE]);
                }
            }
            0x7F if body.first().is_some_and(|e| matches!(e, 0x0E | 0x0F | 0x15 | 0x21)) => {}
            0x05 => {
                if matches!(target, AudioTarget::Ac3 { .. }) && body.starts_with(b"AC-3") {
                    has_ac3_reg = true;
                    push_desc(&mut out, tag, body);
                }
            }
            0x81 | 0xCC | 0xAC if !pds => {}
            _ => push_desc(&mut out, tag, body),
        }
    }
    if let AudioTarget::Ac3 { flavour } = target
        && with_additions
    {
        if !has_ac3_reg {
            out.extend_from_slice(&AC3_REGISTRATION);
        }
        if flavour == TsFlavour::Dvb {
            out.extend_from_slice(&DVB_AC3_DESCRIPTOR);
        }
    }
    out
}

/// Apply the video policy to a replaced ES's descriptor loop: drop the
/// codec-specific video descriptors of the SOURCE codec (MPEG-2 0x02,
/// MPEG-4 0x1B, AVC 0x28 / 0x2A, HEVC 0x38), maximum_bitrate (0x0E), the
/// ES-level CA descriptor (0x09) and a registration whose identifier is not
/// the target's; keep the rest (0x52, 0x0A, 0x06, …).
fn video_descriptors(src: &[u8], stream_type: u8) -> Vec<u8> {
    let target_ident: Option<&[u8]> = match stream_type {
        0x24 => Some(b"HEVC"),
        _ => None,
    };
    let mut out = Vec::with_capacity(src.len());
    for (tag, body) in descriptors(src) {
        match tag {
            0x02 | 0x1B | 0x28 | 0x2A | 0x38 | 0x0E | 0x09 => {}
            0x05 if !target_ident.is_some_and(|id| body.starts_with(id)) => {}
            _ => push_desc(&mut out, tag, body),
        }
    }
    out
}

/// Program-level descriptors that describe the multiplex rate / buffer
/// model of the SOURCE and are wrong once any ES of the program is
/// re-encoded: multiplex_buffer_utilization (0x0C), maximum_bitrate (0x0E),
/// smoothing_buffer (0x10) and STD (0x11).
fn program_descriptors(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for (tag, body) in descriptors(src) {
        if !matches!(tag, 0x0C | 0x0E | 0x10 | 0x11) {
            push_desc(&mut out, tag, body);
        }
    }
    out
}

fn push_desc(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.push(body.len() as u8);
    out.extend_from_slice(body);
}

/// A PMT edit plan.
#[derive(Clone, Debug, Default)]
pub struct PmtEdit<'a> {
    pub es: &'a [EsEdit],
    /// Force `PCR_PID` to this value.
    pub pcr_pid: Option<u16>,
    /// When `false`, the AC-3 registration / 0x6A additions are left out
    /// (the growth fallback — see [`rebuild_pmt_section`]).
    pub no_additions: bool,
}

/// Rebuild a complete PMT section per `edit`. Returns `None` when the
/// section does not parse (it is then emitted untouched by the caller).
/// The version_number is copied from the source; stamp it afterwards with
/// [`OutVersion::stamp`]. The CRC is always recomputed.
///
/// ES entries without an edit, and every descriptor outside the replaced
/// ES entries, are copied byte-for-byte. When any ES is re-encoded the
/// program-level rate descriptors (0x0C / 0x0E / 0x10 / 0x11) are dropped
/// too — an audio 128 → 448 kbps change invalidates them as much as a
/// video re-encode does.
pub fn rebuild_pmt_section(section: &[u8], edit: &PmtEdit<'_>) -> Option<Vec<u8>> {
    let view = parse_pmt(section)?;
    let any_reencode = view
        .es
        .iter()
        .any(|es| edit.es.iter().any(|e| e.pid() == es.pid && e.re_encodes()));

    let mut body: Vec<u8> = Vec::with_capacity(section.len() + 16);
    // program_number, version/cni, section_number, last_section_number.
    body.extend_from_slice(&section[3..8]);
    let pcr_pid = edit.pcr_pid.unwrap_or(view.pcr_pid);
    body.push((section[8] & 0xE0) | ((pcr_pid >> 8) as u8 & 0x1F));
    body.push(pcr_pid as u8);
    let program_info = if any_reencode {
        program_descriptors(view.program_info_bytes())
    } else {
        view.program_info_bytes().to_vec()
    };
    push_loop_len(&mut body, section[10], program_info.len());
    body.extend_from_slice(&program_info);

    for es in &view.es {
        let entry = &section[es.info.start - 5..es.info.start];
        let info = view.es_info(es);
        let e = edit.es.iter().find(|e| e.pid() == es.pid);
        let (stream_type, pid, info): (u8, u16, std::borrow::Cow<'_, [u8]>) = match e {
            None => (es.stream_type, es.pid, info.into()),
            Some(EsEdit::Repid { new_pid, .. }) => (es.stream_type, *new_pid, info.into()),
            Some(EsEdit::Audio { target, .. }) => {
                let target = if edit.no_additions {
                    match target {
                        AudioTarget::Ac3 { .. } => AudioTarget::Ac3 { flavour: TsFlavour::Atsc },
                        t => *t,
                    }
                } else {
                    *target
                };
                (
                    target.stream_type(),
                    es.pid,
                    audio_descriptors(info, target, !edit.no_additions).into(),
                )
            }
            Some(EsEdit::Video { stream_type, .. }) => {
                (*stream_type, es.pid, video_descriptors(info, *stream_type).into())
            }
        };
        body.push(stream_type);
        body.push((entry[1] & 0xE0) | ((pid >> 8) as u8 & 0x1F));
        body.push(pid as u8);
        push_loop_len(&mut body, entry[3], info.len());
        body.extend_from_slice(&info);
    }

    let section_length = body.len() + 4;
    if section_length > super::ts_parse::PSI_MAX_SECTION_LENGTH {
        return None;
    }
    let mut out = Vec::with_capacity(3 + section_length);
    out.push(0x02);
    out.push((section[1] & 0xF0) | ((section_length >> 8) as u8 & 0x0F));
    out.push(section_length as u8);
    out.extend_from_slice(&body);
    let crc = mpeg2_crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    Some(out)
}

/// Rebuild section `i` of `unit` per `edit`, falling back to the
/// growth-free edit (`no_additions`: an AC-3 target signalled as
/// self-identifying 0x81 with no added descriptors) whenever the full edit
/// does not work out — either the grown unit would need more packets than
/// the source's (so a single-packet PMT stays single-packet for every
/// downstream single-packet parser), or the grown section would exceed the
/// 1021-byte PMT limit, when [`rebuild_pmt_section`] refuses it outright.
/// The fallback used to hang off the full rebuild's success, so the
/// overflow case — the one it exists for — emitted the source PMT
/// untouched (the old stream_type over a re-encoded ES).
pub fn rebuild_pmt_section_fitting(unit: &PsiUnit, i: usize, edit: &PmtEdit<'_>) -> Option<Vec<u8>> {
    let section = &unit.sections()[i];
    rebuild_pmt_section(section, edit)
        .filter(|full| unit.fits_source_packets_with(i, full))
        .or_else(|| rebuild_pmt_section(section, &PmtEdit { no_additions: true, ..edit.clone() }))
}

fn push_loop_len(body: &mut Vec<u8>, reserved_src: u8, len: usize) {
    body.push((reserved_src & 0xF0) | ((len >> 8) as u8 & 0x0F));
    body.push(len as u8);
}

/// Recompute the CRC_32 of a complete long-form section in place.
pub fn recompute_crc(section: &mut [u8]) {
    let n = section.len();
    if n < 4 {
        return;
    }
    let crc = mpeg2_crc32(&section[..n - 4]);
    section[n - 4..].copy_from_slice(&crc.to_be_bytes());
}

// ───────────────────────── Output version ─────────────────────────

/// Output PMT `version_number`, derived from content.
///
/// [`Self::stamp`] compares the rebuilt section — minus its version bits
/// and CRC — with the last one it stamped and bumps (mod 32) when they
/// differ; an unchanged section keeps its version, so a PMT repeated a
/// hundred times does not flap. Once a stage has stamped (see
/// [`Self::has_stamped`]) it stamps the PMTs it passes through unedited
/// too, so rebuilt and passthrough PMTs share one version sequence.
/// [`Self::bump`] forces a bump on the next stamp (a source reset).
/// Because each stage compares its own
/// input-derived output, a chained video stage sees the audio stage's
/// changed stream_type and bumps too: chained stages compose.
#[derive(Clone, Debug)]
pub struct OutVersion {
    version: u8,
    last_body: Option<Vec<u8>>,
    pending_bump: bool,
}

impl Default for OutVersion {
    fn default() -> Self {
        Self::new()
    }
}

impl OutVersion {
    /// Starts at 1, the value the replacers always stamped first.
    pub fn new() -> Self {
        Self { version: 1, last_body: None, pending_bump: false }
    }

    #[cfg(test)]
    pub fn current(&self) -> u8 {
        self.version
    }

    /// Force the next stamp to carry a new version.
    pub fn bump(&mut self) {
        self.pending_bump = true;
    }

    /// True once any section has been stamped. From then on every PMT the
    /// stage emits for its program — rebuilt or passed through — must be
    /// stamped, so the output carries one version sequence: a passthrough
    /// PMT keeping its source version could repeat the version a rebuilt
    /// one already used (a receiver caching by version would then never
    /// re-parse the new content).
    pub fn has_stamped(&self) -> bool {
        self.last_body.is_some()
    }

    /// Stamp the version into a complete long-form section and recompute
    /// its CRC. Bumps first when the content changed or a bump is pending.
    pub fn stamp(&mut self, section: &mut [u8]) {
        if section.len() < 12 {
            return;
        }
        let mut body = section[..section.len() - 4].to_vec();
        body[5] &= 0xC1;
        let changed = self.last_body.as_ref().is_some_and(|b| *b != body);
        if changed || self.pending_bump {
            self.version = (self.version + 1) & 0x1F;
            self.pending_bump = false;
        }
        self.last_body = Some(body);
        section[5] = (section[5] & 0xC1) | (self.version << 1);
        recompute_crc(section);
    }
}

// ───────────────────────── PMT-PID stage ─────────────────────────

/// Maximum packets one unit may span before it is treated as corrupt.
/// A 4096-byte private section needs 23 packets.
const MAX_UNIT_PACKETS: usize = 32;

/// One complete PSI payload unit on a PMT PID: its source packets and
/// every complete section that started in it.
#[derive(Debug)]
pub struct PsiUnit {
    /// The unit's payload-carrying packets (the re-layout templates).
    packets: Vec<[u8; TS_PACKET_SIZE]>,
    /// Adaptation-field-only packets that arrived while the unit was held,
    /// each with the number of payload packets that preceded it — so they
    /// go out in their source position rather than ahead of the unit.
    af_only: Vec<(usize, [u8; TS_PACKET_SIZE])>,
    sections: Vec<Vec<u8>>,
    changed: bool,
}

impl PsiUnit {
    pub fn sections(&self) -> &[Vec<u8>] {
        &self.sections
    }

    /// Replace section `i`. A byte-identical replacement is a no-op, so an
    /// unchanged unit is re-emitted as its original packets.
    pub fn replace_section(&mut self, i: usize, new: Vec<u8>) {
        if self.sections[i] != new {
            self.sections[i] = new;
            self.changed = true;
        }
    }

    /// First table_id of the unit (for diagnostics).
    pub fn first_table_id(&self) -> Option<u8> {
        self.sections.first().and_then(|s| s.first().copied())
    }

    /// Would the unit, with section `i` replaced by `new`, still fit the
    /// source's packet count? Callers use it to prefer a growth-free edit
    /// so a single-packet PMT stays single-packet.
    pub fn fits_source_packets_with(&self, i: usize, new: &[u8]) -> bool {
        let refs: Vec<&[u8]> = self
            .sections
            .iter()
            .enumerate()
            .map(|(j, s)| if j == i { new } else { s.as_slice() })
            .collect();
        layout(&refs, &self.packets).len() <= self.packets.len()
    }
}

/// Reassembling, re-packetising PSI stage for one PMT PID.
///
/// - **Input.** Every packet on the PID goes through [`Self::push`]. A
///   unit runs from a PUSI packet until no section is in flight at a
///   packet end, so a section that finishes in the next PUSI packet's
///   pointer tail keeps both packets in one unit. A long-form section
///   reassembled across packets must pass its CRC — that, rather than the
///   CC, proves nothing was lost (some muxers never advance the CC on
///   PSI). A failed CRC, a pointer tail that does not end the section
///   exactly, or a unit longer than 32 packets drops the held packets
///   (counted, logged once). A duplicate continuation packet (same CC,
///   byte-identical) is dropped; so is an orphan continuation (joined
///   mid-unit).
/// - **Output.** [`Self::emit`] writes an unchanged unit as its original
///   packets — byte-identical. A changed unit is re-laid out with
///   pointer_field and PUSI set on every packet in which a section starts,
///   0xFF stuffing after the last section, and each output packet reusing
///   the matching source packet's header bits and adaptation field (so a
///   PCR or private AF data on the PMT PID survives). Source CCs are kept
///   while every edited unit keeps its packet count; once a count changes
///   (or packets are dropped) the stage owns the CC on this PID from then
///   on. A surplus source packet whose adaptation field carries data is
///   re-emitted adaptation-field-only.
/// - Packets without payload pass straight through when no unit is held;
///   while one is held they wait in their source position (an
///   adaptation-field-only packet — a PCR on the PMT PID — must follow the
///   payload packet whose CC it repeats, not overtake the held unit) and go
///   out with it, or on their own if the unit is dropped.
#[derive(Debug)]
pub struct PsiUnitStage {
    held: Vec<[u8; TS_PACKET_SIZE]>,
    /// Payload-less packets received while `held` is non-empty, with the
    /// count of held payload packets ahead of each.
    held_af: Vec<(usize, [u8; TS_PACKET_SIZE])>,
    sections: Vec<Vec<u8>>,
    inflight: Vec<u8>,
    inflight_active: bool,
    last_in_cc: Option<u8>,
    /// The last accepted input packet — a same-CC packet is a duplicate
    /// only when it is byte-identical to it.
    last_in: [u8; TS_PACKET_SIZE],
    owned_cc: bool,
    last_out_cc: Option<u8>,
    dropped_packets: u64,
    warned: bool,
    what: &'static str,
}

impl PsiUnitStage {
    /// `what` names the owner in the one-shot drop warning.
    pub fn new(what: &'static str) -> Self {
        Self {
            held: Vec::new(),
            held_af: Vec::new(),
            sections: Vec::new(),
            inflight: Vec::new(),
            inflight_active: false,
            last_in_cc: None,
            last_in: [0u8; TS_PACKET_SIZE],
            owned_cc: false,
            last_out_cc: None,
            dropped_packets: 0,
            warned: false,
            what,
        }
    }

    /// Packets dropped as corrupt / unrecoverable since construction.
    #[cfg(test)]
    pub fn dropped_packets(&self) -> u64 {
        self.dropped_packets
    }

    fn drop_held(&mut self, reason: &str, extra: u64, out: &mut Vec<u8>) {
        let n = self.held.len() as u64 + extra;
        self.held.clear();
        self.sections.clear();
        self.inflight.clear();
        self.inflight_active = false;
        if n == 0 {
            self.flush_held_af(out);
            return;
        }
        self.dropped_packets += n;
        // The emitted CC sequence now has a hole this stage made; own the
        // counter from here on so it stays continuous. Before anything was
        // emitted there is no sequence to keep, so the source CCs stay.
        if self.last_out_cc.is_some() {
            self.owned_cc = true;
        }
        // Payload-less packets held behind the dropped unit carry no PSI
        // (a PCR, private AF data): they still go out.
        self.flush_held_af(out);
        if !self.warned {
            self.warned = true;
            tracing::warn!(
                "{}: dropped {n} PMT-PID packet(s) — {reason}; further drops are counted silently",
                self.what
            );
        }
    }

    /// Write out the payload-less packets held behind a unit that will not
    /// be emitted, in their source order.
    fn flush_held_af(&mut self, out: &mut Vec<u8>) {
        for (_, p) in std::mem::take(&mut self.held_af) {
            self.write_packet(&p, out);
        }
    }

    /// Needed length of the in-flight section once its header is present.
    fn inflight_needed(&self) -> Result<Option<usize>, ()> {
        if self.inflight.len() < 3 {
            return Ok(None);
        }
        let table_id = self.inflight[0];
        let len = (((self.inflight[1] as usize) & 0x0F) << 8) | self.inflight[2] as usize;
        match max_section_length(table_id) {
            Some(max) if len <= max => Ok(Some(3 + len)),
            _ => Err(()),
        }
    }

    /// Walk the sections starting at `pos` in a PUSI payload. An invalid
    /// header or 0xFF ends the walk (the remainder is treated as stuffing).
    fn walk(&mut self, payload: &[u8], mut pos: usize) {
        while pos < payload.len() && payload[pos] != 0xFF {
            let remaining = payload.len() - pos;
            if remaining < 3 {
                self.inflight.clear();
                self.inflight.extend_from_slice(&payload[pos..]);
                self.inflight_active = true;
                return;
            }
            let table_id = payload[pos];
            let len = (((payload[pos + 1] as usize) & 0x0F) << 8) | payload[pos + 2] as usize;
            match max_section_length(table_id) {
                Some(max) if len <= max => {}
                _ => return,
            }
            let total = 3 + len;
            if total <= remaining {
                self.sections.push(payload[pos..pos + total].to_vec());
                pos += total;
            } else {
                self.inflight.clear();
                self.inflight.extend_from_slice(&payload[pos..]);
                self.inflight_active = true;
                return;
            }
        }
    }

    /// Move the in-flight section (now at least `total` bytes) to the
    /// unit. A long-form section reassembled across packets must carry a
    /// valid CRC_32 — that, not the CC, is what proves no packet was lost
    /// or damaged (some muxers never advance the CC on PSI at all).
    fn finish_inflight(&mut self, total: usize, out: &mut Vec<u8>) -> bool {
        self.inflight.truncate(total);
        let long_form = self.inflight.len() > 1 && self.inflight[1] & 0x80 != 0;
        if long_form && mpeg2_crc32(&self.inflight) != 0 {
            self.drop_held("a section reassembled across packets failed its CRC", 0, out);
            return false;
        }
        let s = std::mem::take(&mut self.inflight);
        self.sections.push(s);
        self.inflight_active = false;
        true
    }

    /// Feed one packet on the PMT PID. Returns a complete unit for the
    /// caller to inspect / edit and hand to [`Self::emit`]; packets that
    /// need no editing decision (no payload) are written to `out` directly.
    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<u8>) -> Option<PsiUnit> {
        if pkt.len() != TS_PACKET_SIZE || pkt[0] != TS_SYNC_BYTE {
            out.extend_from_slice(pkt);
            return None;
        }
        let has_payload = (pkt[3] >> 4) & 0x01 != 0;
        let off = ts_payload_offset(pkt);
        if !has_payload || off >= TS_PACKET_SIZE {
            if self.held.is_empty() {
                self.write_packet(pkt, out);
            } else {
                // Emitting it now would put it ahead of the held unit: a
                // PCR moved in front of the PMT bytes, and an AF-only CC
                // that no longer repeats the payload CC before it.
                let mut p = [0u8; TS_PACKET_SIZE];
                p.copy_from_slice(pkt);
                self.held_af.push((self.held.len(), p));
            }
            return None;
        }
        let cc = ts_cc(pkt);
        let pusi = ts_pusi(pkt);
        if !pusi && self.last_in_cc == Some(cc) && self.last_in[..] == pkt[..TS_PACKET_SIZE] {
            // Duplicate continuation (ISO 13818-1 allows one): appending it
            // would repeat its bytes. A duplicate PUSI packet just restarts
            // the same unit, which is harmless.
            return None;
        }
        self.last_in_cc = Some(cc);
        self.last_in.copy_from_slice(&pkt[..TS_PACKET_SIZE]);
        let payload = &pkt[off..TS_PACKET_SIZE];
        let mut buf = [0u8; TS_PACKET_SIZE];
        buf.copy_from_slice(pkt);

        if pusi {
            let sec_start = 1 + payload[0] as usize;
            if sec_start > payload.len() {
                self.drop_held("pointer_field points past the packet", 1, out);
                return None;
            }
            if self.inflight_active {
                // Bytes before the pointer target end the section in flight
                // — exactly, or the unit is corrupt.
                self.inflight.extend_from_slice(&payload[1..sec_start]);
                match self.inflight_needed() {
                    Ok(Some(total)) if total == self.inflight.len() => {
                        self.finish_inflight(total, out);
                    }
                    _ => self.drop_held("section truncated by the next unit start", 0, out),
                }
            }
            self.held.push(buf);
            self.walk(payload, sec_start);
        } else {
            if !self.inflight_active {
                // Orphan continuation: joined mid-unit or after a drop.
                self.dropped_packets += 1;
                if self.last_out_cc.is_some() {
                    self.owned_cc = true;
                }
                return None;
            }
            self.held.push(buf);
            self.inflight.extend_from_slice(payload);
            match self.inflight_needed() {
                Ok(Some(total)) if self.inflight.len() >= total => {
                    if !self.finish_inflight(total, out) {
                        return None;
                    }
                }
                Ok(_) => {}
                Err(()) => {
                    self.drop_held("invalid section header", 0, out);
                    return None;
                }
            }
        }
        if self.held.len() > MAX_UNIT_PACKETS {
            self.drop_held("unit longer than 32 packets", 0, out);
            return None;
        }
        if !self.inflight_active && !self.held.is_empty() {
            return Some(PsiUnit {
                packets: std::mem::take(&mut self.held),
                af_only: std::mem::take(&mut self.held_af),
                sections: std::mem::take(&mut self.sections),
                changed: false,
            });
        }
        None
    }

    /// Emit a unit returned by [`Self::push`]. Payload-less packets held
    /// with the unit go out in their source position: behind the payload
    /// packet they followed (output packet `k - 1` for one that followed
    /// `k` payload packets), repeating its CC.
    pub fn emit(&mut self, unit: PsiUnit, out: &mut Vec<u8>) {
        let mut af = unit.af_only.into_iter().peekable();
        if !unit.changed {
            for (j, p) in unit.packets.iter().enumerate() {
                while let Some((_, a)) = af.next_if(|(k, _)| *k <= j) {
                    self.write_packet(&a, out);
                }
                self.write_packet(p, out);
            }
            for (_, a) in af {
                self.write_packet(&a, out);
            }
            return;
        }
        let refs: Vec<&[u8]> = unit.sections.iter().map(|s| s.as_slice()).collect();
        let pkts = layout(&refs, &unit.packets);
        if pkts.len() != unit.packets.len() {
            self.owned_cc = true;
        }
        for (j, p) in pkts.iter().enumerate() {
            while let Some((_, a)) = af.next_if(|(k, _)| *k <= j) {
                self.write_packet(&a, out);
            }
            self.write_packet(p, out);
        }
        // Surplus source packets carrying adaptation-field data (a PCR on
        // the PMT PID, private data) survive as AF-only packets, in source
        // order with the held payload-less packets around them.
        for (j, src) in unit.packets.iter().enumerate().skip(pkts.len()) {
            while let Some((_, a)) = af.next_if(|(k, _)| *k <= j) {
                self.write_packet(&a, out);
            }
            if let Some(af_only) = af_only_copy(src) {
                self.write_packet(&af_only, out);
            }
        }
        for (_, a) in af {
            self.write_packet(&a, out);
        }
    }

    /// Append one packet, restamping the CC when this stage owns it.
    fn write_packet(&mut self, pkt: &[u8], out: &mut Vec<u8>) {
        let mut p = [0u8; TS_PACKET_SIZE];
        p.copy_from_slice(pkt);
        let has_payload = (p[3] >> 4) & 0x01 != 0;
        if self.owned_cc {
            let cc = match (has_payload, self.last_out_cc) {
                (true, Some(prev)) => (prev + 1) & 0x0F,
                (true, None) => ts_cc(&p),
                (false, Some(prev)) => prev,
                (false, None) => ts_cc(&p),
            };
            p[3] = (p[3] & 0xF0) | cc;
        }
        if has_payload || self.last_out_cc.is_none() {
            self.last_out_cc = Some(ts_cc(&p));
        }
        out.extend_from_slice(&p);
    }
}

/// Re-stamp `version` into every long-form PMT section of a cached PMT-PID
/// unit (its source packets, first one PUSI), recomputing each CRC, and
/// lay the unit out again over the same packets. Other sections — a 0xC0
/// table ahead of the PMT — stay byte-identical. Used by the input-switch
/// PSI injection, which used to stamp the section at the pointer target of
/// the unit's FIRST packet only: a short-form 0xC0 table got a "version"
/// and a "CRC" written over its data, and a PMT continuing into a second
/// packet could not be stamped at all. Returns the packets unchanged when
/// they do not reassemble into a complete unit.
pub fn restamp_pmt_unit(
    packets: &[[u8; TS_PACKET_SIZE]],
    version: u8,
) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut stage = PsiUnitStage::new("psi_restamp");
    let mut out = Vec::with_capacity(packets.len() * TS_PACKET_SIZE);
    let mut unit = None;
    for p in packets {
        if let Some(u) = stage.push(p, &mut out) {
            unit = Some(u);
        }
    }
    let Some(mut unit) = unit else {
        return packets.to_vec();
    };
    for i in 0..unit.sections().len() {
        let mut section = unit.sections()[i].clone();
        if section.first() == Some(&0x02)
            && super::ts_parse::set_psi_version_at(&mut section, 0, version)
        {
            unit.replace_section(i, section);
        }
    }
    stage.emit(unit, &mut out);
    out.chunks_exact(TS_PACKET_SIZE)
        .map(|c| {
            let mut p = [0u8; TS_PACKET_SIZE];
            p.copy_from_slice(c);
            p
        })
        .collect()
}

/// Adaptation-field bytes (length byte included) of a packet, or `None`.
fn af_bytes(pkt: &[u8; TS_PACKET_SIZE]) -> Option<&[u8]> {
    if (pkt[3] >> 4) & 0x02 == 0 {
        return None;
    }
    let len = pkt[4] as usize;
    if 5 + len > TS_PACKET_SIZE {
        return None;
    }
    Some(&pkt[4..5 + len])
}

/// An AF-only copy of `src` when its adaptation field carries anything
/// (flags or data); `None` for a stuffing-only AF.
fn af_only_copy(src: &[u8; TS_PACKET_SIZE]) -> Option<[u8; TS_PACKET_SIZE]> {
    let af = af_bytes(src)?;
    if af.len() < 2 || (af[1] == 0 && af[2..].iter().all(|&b| b == 0xFF)) {
        return None;
    }
    let mut p = [0xFFu8; TS_PACKET_SIZE];
    p[..4].copy_from_slice(&src[..4]);
    p[1] &= 0xBF; // no PUSI
    p[3] = (p[3] & 0x0F) | 0x20; // AFC = adaptation field only
    p[4] = 183;
    p[5..5 + af.len() - 1].copy_from_slice(&af[1..]);
    Some(p)
}

/// Lay `sections` out over packets, reusing `templates[i]`'s header bits
/// and adaptation field for output packet `i` and fresh payload-only
/// packets beyond them. PUSI + pointer_field are set on every packet in
/// which a section starts; a section may not begin on the last byte of a
/// packet without a pointer, so that byte becomes stuffing instead.
fn layout(sections: &[&[u8]], templates: &[[u8; TS_PACKET_SIZE]]) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut data = Vec::new();
    let mut starts = Vec::with_capacity(sections.len());
    for s in sections {
        starts.push(data.len());
        data.extend_from_slice(s);
    }
    let template0 = templates.first();
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut next = 0usize; // index into `starts` of the next section not yet begun
    while pos < data.len() {
        let i = out.len();
        let mut p = [0xFFu8; TS_PACKET_SIZE];
        p[0] = TS_SYNC_BYTE;
        let (hdr, af): ([u8; 3], Option<&[u8]>) = match templates.get(i) {
            Some(t) => ([t[1], t[2], t[3]], af_bytes(t)),
            None => {
                let t = template0.expect("a unit always has a first packet");
                ([t[1], t[2], (t[3] & 0xC0) | 0x10 | (t[3] & 0x0F)], None)
            }
        };
        // Header: keep TPR + PID; PUSI decided below; no TEI, no scrambling.
        p[1] = hdr[0] & 0x3F;
        p[2] = hdr[1];
        let cc = hdr[2] & 0x0F;
        let mut w = 4usize;
        match af {
            Some(af) if af.len() < TS_PACKET_SIZE - 5 => {
                p[3] = 0x30 | cc;
                p[4..4 + af.len()].copy_from_slice(af);
                w = 4 + af.len();
            }
            _ => p[3] = 0x10 | cc,
        }
        let cap = TS_PACKET_SIZE - w;
        while next < starts.len() && starts[next] < pos {
            next += 1;
        }
        let next_start = starts.get(next).copied();
        match next_start {
            Some(s) if s + 1 < pos + cap => {
                p[1] |= 0x40;
                p[w] = (s - pos) as u8;
                let n = (cap - 1).min(data.len() - pos);
                p[w + 1..w + 1 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            Some(s) if s + 1 == pos + cap => {
                let n = cap - 1;
                p[w..w + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            _ => {
                let n = cap.min(data.len() - pos);
                p[w..w + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
        }
        out.push(p);
    }
    // Fresh packets past the templates get consecutive CCs after the last
    // template; they only exist when the unit grew, and the stage owns the
    // CC in that case anyway.
    if let Some(last_t) = templates.last() {
        let mut cc = ts_cc(last_t);
        for p in out.iter_mut().skip(templates.len()) {
            cc = (cc + 1) & 0x0F;
            p[3] = (p[3] & 0xF0) | cc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_parse::{find_section_in_packet, verify_psi_crc};
    use crate::engine::ts_test_fixtures::{
        packetize_sections, pmt_section, two_packet_pmt, vh1_pmt_packet, VH1_PMT_OFFSET,
        VH1_PROGRAM,
    };

    fn es_descs<'a>(view: &'a PmtView<'a>, pid: u16) -> Vec<(u8, Vec<u8>)> {
        let es = view.es.iter().find(|e| e.pid == pid).expect("es present");
        descriptors(view.es_info(es)).map(|(t, b)| (t, b.to_vec())).collect()
    }

    fn crc_ok(section: &[u8]) -> bool {
        mpeg2_crc32(section) == 0
    }

    // Shapes taken from the diagnosis captures (pmt.py dumps).
    fn sky_sports_pmt() -> Vec<u8> {
        pmt_section(
            1,
            3,
            0x00C8,
            &[],
            &[
                (0x1B, 0x00C8, &[]),
                (0x0F, 0x00C9, &[0x0A, 0x04, b'e', b'n', b'g', 0x00, 0x7C, 0x01, 0x51]),
            ],
        )
    }

    fn witness_pmt() -> Vec<u8> {
        pmt_section(
            1,
            0,
            0x0282,
            &[],
            &[
                (0x1B, 0x0282, &[0x52, 0x01, 0x01]),
                (
                    0x04,
                    0x0283,
                    &[
                        0x5F, 0x04, b'O', b'T', b'V', 0x00, // PDS
                        0xFE, 0x04, b'A', b'U', b'D', 0x01, // private, governed
                        0x0A, 0x04, b'N', b'A', b'R', 0x00, //
                        0x0E, 0x03, 0xC0, 0x01, 0x40, // max bitrate 128 kbps
                        0x52, 0x01, 0x84, //
                        0x03, 0x01, 0x67, //
                    ],
                ),
                (0x06, 0x0297, &[0x6A, 0x03, 0xC0, 0x44, 0x08]),
            ],
        )
    }

    #[test]
    fn mp2_target_drops_the_aac_descriptor() {
        let src = sky_sports_pmt();
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit {
                es: &[EsEdit::Audio { pid: 0xC9, target: AudioTarget::Mp2 { lsf: false } }],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(crc_ok(&out));
        let v = parse_pmt(&out).unwrap();
        assert_eq!(v.es[1].stream_type, 0x03);
        assert_eq!(es_descs(&v, 0xC9), vec![(0x0A, b"eng\0".to_vec())]);
        // The video ES is byte-identical.
        assert_eq!(&out[12..17], &src[12..17]);
    }

    #[test]
    fn ac3_target_follows_the_flavour() {
        let src = sky_sports_pmt();
        let view = parse_pmt(&src).unwrap();
        assert_eq!(detect_flavour(&view), TsFlavour::Dvb, "0x7C is DVB evidence");
        let dvb = rebuild_pmt_section(
            &src,
            &PmtEdit {
                es: &[EsEdit::Audio { pid: 0xC9, target: AudioTarget::Ac3 { flavour: TsFlavour::Dvb } }],
                ..Default::default()
            },
        )
        .unwrap();
        let v = parse_pmt(&dvb).unwrap();
        assert_eq!(v.es[1].stream_type, 0x06);
        assert_eq!(
            es_descs(&v, 0xC9),
            vec![
                (0x0A, b"eng\0".to_vec()),
                (0x05, b"AC-3".to_vec()),
                (0x6A, vec![0x00]),
            ]
        );
        assert!(crc_ok(&dvb));
        // An output PMT fed to a second replacer still resolves AC-3.
        assert_eq!(
            crate::engine::ts_parse::descriptor_audio_kind(v.es_info(&v.es[1])),
            Some(crate::engine::ts_parse::PrivateEsAudioKind::Ac3)
        );

        let atsc = rebuild_pmt_section(
            &src,
            &PmtEdit {
                es: &[EsEdit::Audio { pid: 0xC9, target: AudioTarget::Ac3 { flavour: TsFlavour::Atsc } }],
                ..Default::default()
            },
        )
        .unwrap();
        let v = parse_pmt(&atsc).unwrap();
        assert_eq!(v.es[1].stream_type, 0x81);
        assert_eq!(es_descs(&v, 0xC9), vec![(0x0A, b"eng\0".to_vec()), (0x05, b"AC-3".to_vec())]);
    }

    #[test]
    fn witness_mp2_to_aac_keeps_non_codec_descriptors_in_order() {
        let src = witness_pmt();
        assert_eq!(detect_flavour(&parse_pmt(&src).unwrap()), TsFlavour::Dvb);
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Audio { pid: 0x283, target: AudioTarget::Aac }], ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        let tags: Vec<u8> = es_descs(&v, 0x283).iter().map(|(t, _)| *t).collect();
        assert_eq!(tags, vec![0x5F, 0xFE, 0x0A, 0x52], "03 and 0e dropped, order kept");
        assert_eq!(v.es[1].stream_type, 0x0F);
        // The untouched DVB AC-3 ES keeps its 0x6A byte-for-byte.
        assert_eq!(es_descs(&v, 0x297), vec![(0x6A, vec![0xC0, 0x44, 0x08])]);
    }

    #[test]
    fn mp2_rewrites_the_audio_stream_descriptor() {
        let src = witness_pmt();
        for (lsf, st, byte) in [(false, 0x03, 0x67), (true, 0x04, 0x27)] {
            let out = rebuild_pmt_section(
                &src,
                &PmtEdit {
                    es: &[EsEdit::Audio { pid: 0x283, target: AudioTarget::Mp2 { lsf } }],
                    ..Default::default()
                },
            )
            .unwrap();
            let v = parse_pmt(&out).unwrap();
            assert_eq!(v.es[1].stream_type, st);
            assert!(es_descs(&v, 0x283).contains(&(0x03, vec![byte])));
            assert!(!es_descs(&v, 0x283).iter().any(|(t, _)| *t == 0x0E));
        }
    }

    #[test]
    fn aac_normalises_a_long_aac_descriptor() {
        let src = pmt_section(1, 0, 0x100, &[], &[(0x11, 0x101, &[0x7C, 0x03, 0x58, 0x80, 0x01])]);
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Audio { pid: 0x101, target: AudioTarget::Aac }], ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        assert_eq!(es_descs(&v, 0x101), vec![(0x7C, vec![0xFE])]);
    }

    #[test]
    fn video_policy_and_program_level_drops() {
        // spain-shaped: MPEG-2 video with 0x02 + 0x0E, program-level 0x0C + 0x0E.
        let src = pmt_section(
            186,
            4,
            0x44D,
            &[0x0C, 0x04, 0x80, 0xB4, 0x81, 0x68, 0x0E, 0x03, 0xC0, 0x35, 0xD5, 0x09, 0x04, 0, 1, 0xE1, 0],
            &[
                (0x02, 0x44D, &[0x02, 0x03, 0x1A, 0x48, 0x5F, 0x0E, 0x03, 0xC0, 0x31, 0x43, 0x52, 0x01, 0x02]),
                (0x03, 0x44F, &[0x0A, 0x04, b's', b'p', b'a', 0x00, 0x03, 0x01, 0x67]),
            ],
        );
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Video { pid: 0x44D, stream_type: 0x1B }], pcr_pid: Some(0x44D), ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        assert_eq!(v.es[0].stream_type, 0x1B);
        assert_eq!(es_descs(&v, 0x44D), vec![(0x52, vec![0x02])]);
        let prog: Vec<u8> = descriptors(v.program_info_bytes()).map(|(t, _)| t).collect();
        assert_eq!(prog, vec![0x09], "0x0C and 0x0E dropped, CA kept");
        // The passthrough audio ES is untouched.
        assert_eq!(es_descs(&v, 0x44F).len(), 2);
        // An audio-only edit drops the program-level rate descriptors too.
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Audio { pid: 0x44F, target: AudioTarget::Aac }], ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        let prog: Vec<u8> = descriptors(v.program_info_bytes()).map(|(t, _)| t).collect();
        assert_eq!(prog, vec![0x09]);
        // A PID rename alone keeps program_info.
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Repid { pid: 0x44F, new_pid: 0x101 }], ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        assert_eq!(v.program_info_bytes(), parse_pmt(&src).unwrap().program_info_bytes());
        assert_eq!(v.es[1].pid, 0x101);
    }

    #[test]
    fn hevc_to_h264_drops_the_hevc_registration() {
        let src = pmt_section(1, 0, 0x100, &[], &[(0x24, 0x100, &[0x05, 0x04, b'H', b'E', b'V', b'C', 0x38, 0x01, 0x00])]);
        let out = rebuild_pmt_section(
            &src,
            &PmtEdit { es: &[EsEdit::Video { pid: 0x100, stream_type: 0x1B }], ..Default::default() },
        )
        .unwrap();
        let v = parse_pmt(&out).unwrap();
        assert!(es_descs(&v, 0x100).is_empty());
    }

    #[test]
    fn vh1_is_atsc_and_ac3_growth_fallback_drops_additions() {
        let pkt = vh1_pmt_packet();
        let s = find_section_in_packet(&pkt, 0x02, Some(VH1_PROGRAM)).unwrap();
        let sec = &pkt[s.start..s.end()];
        let v = parse_pmt(sec).unwrap();
        assert_eq!(detect_flavour(&v), TsFlavour::Atsc);
        assert_eq!(v.pcr_pid, 0x0E0F);
        let edit = [EsEdit::Audio { pid: 0xE10, target: AudioTarget::Ac3 { flavour: TsFlavour::Dvb } }];
        let with = rebuild_pmt_section(sec, &PmtEdit { es: &edit, ..Default::default() }).unwrap();
        let without =
            rebuild_pmt_section(sec, &PmtEdit { es: &edit, no_additions: true, ..Default::default() }).unwrap();
        assert_eq!(parse_pmt(&with).unwrap().es[1].stream_type, 0x06);
        let w = parse_pmt(&without).unwrap();
        assert_eq!(w.es[1].stream_type, 0x81, "fallback is self-identifying 0x81");
        assert_eq!(es_descs(&w, 0xE10), vec![(0x0A, b"eng\0".to_vec())]);
    }

    #[test]
    fn version_tracks_content_not_repetition() {
        let src = sky_sports_pmt();
        let mut ov = OutVersion::new();
        let mut first = src.clone();
        ov.stamp(&mut first);
        for _ in 0..100 {
            let mut s = src.clone();
            ov.stamp(&mut s);
            assert_eq!(s, first);
        }
        assert_eq!(ov.current(), 1, "unchanged PMT repeated: no flapping");
        // A source update that adds an ES bumps without any reset.
        let mut more = pmt_section(1, 3, 0x00C8, &[], &[(0x1B, 0x00C8, &[]), (0x0F, 0x00C9, &[]), (0x06, 0x00CA, &[0x56, 0x00])]);
        ov.stamp(&mut more);
        assert_eq!(ov.current(), 2);
        assert!(crc_ok(&more));
        assert_eq!((more[5] >> 1) & 0x1F, 2);
        // A reset bumps once.
        ov.bump();
        ov.stamp(&mut more);
        assert_eq!(ov.current(), 3);
        ov.stamp(&mut more);
        assert_eq!(ov.current(), 3);
    }

    #[test]
    fn chained_stages_compose_versions() {
        // Audio stage changes the stream_type; the video stage (which only
        // forces PCR_PID) sees a different input and bumps as well.
        let src = sky_sports_pmt();
        let mut audio_v = OutVersion::new();
        let mut video_v = OutVersion::new();
        let run = |target: AudioTarget, av: &mut OutVersion, vv: &mut OutVersion| {
            let mut a = rebuild_pmt_section(
                &src,
                &PmtEdit { es: &[EsEdit::Audio { pid: 0xC9, target }], ..Default::default() },
            )
            .unwrap();
            av.stamp(&mut a);
            let mut v = rebuild_pmt_section(
                &a,
                &PmtEdit { es: &[EsEdit::Video { pid: 0xC8, stream_type: 0x1B }], pcr_pid: Some(0xC8), ..Default::default() },
            )
            .unwrap();
            vv.stamp(&mut v);
            v
        };
        let first = run(AudioTarget::Aac, &mut audio_v, &mut video_v);
        let again = run(AudioTarget::Aac, &mut audio_v, &mut video_v);
        assert_eq!(first, again);
        let changed = run(AudioTarget::Mp2 { lsf: false }, &mut audio_v, &mut video_v);
        assert_ne!((changed[5] >> 1) & 0x1F, (first[5] >> 1) & 0x1F, "final output version changed");
    }

    // ── stage ──

    fn run_stage(stage: &mut PsiUnitStage, pkts: &[[u8; TS_PACKET_SIZE]], mut f: impl FnMut(&mut PsiUnit)) -> Vec<u8> {
        let mut out = Vec::new();
        for p in pkts {
            if let Some(mut u) = stage.push(p, &mut out) {
                f(&mut u);
                stage.emit(u, &mut out);
            }
        }
        out
    }

    #[test]
    fn unchanged_unit_is_byte_identical() {
        let pkt = vh1_pmt_packet();
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &[pkt], |_| {});
        assert_eq!(out, pkt.to_vec());
    }

    #[test]
    fn vh1_edit_keeps_the_private_section_count_and_cc() {
        let pkt = vh1_pmt_packet();
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &[pkt], |u| {
            let i = u.sections().iter().position(|s| is_pmt_for(s, VH1_PROGRAM)).unwrap();
            let mut new = rebuild_pmt_section(
                &u.sections()[i],
                &PmtEdit {
                    es: &[EsEdit::Audio { pid: 0xE10, target: AudioTarget::Aac }],
                    ..Default::default()
                },
            )
            .unwrap();
            OutVersion::new().stamp(&mut new);
            u.replace_section(i, new);
        });
        assert_eq!(out.len(), TS_PACKET_SIZE, "same packet count");
        assert_eq!(&out[..VH1_PMT_OFFSET], &pkt[..VH1_PMT_OFFSET], "header, pointer and 0xC0 identical");
        assert_eq!(out[3] & 0x0F, pkt[3] & 0x0F, "source CC kept");
        assert!(verify_psi_crc(&out, VH1_PMT_OFFSET));
        let s = find_section_in_packet(&out, 0x02, Some(VH1_PROGRAM)).unwrap();
        let v = parse_pmt(&out[s.start..s.end()]).unwrap();
        assert_eq!(v.es[1].stream_type, 0x0F);
    }

    #[test]
    fn two_packet_pmt_is_edited_with_a_valid_crc() {
        let (sec, target) = two_packet_pmt(7, 1);
        let pkts = packetize_sections(0x40, &[&sec], 3);
        let mut stage = PsiUnitStage::new("test");
        let mut learned = None;
        let out = run_stage(&mut stage, &pkts, |u| {
            let v = parse_pmt(&u.sections()[0]).unwrap();
            learned = v.es.iter().find(|e| e.stream_type == 0x03).map(|e| e.pid);
            let new = rebuild_pmt_section(
                &u.sections()[0],
                &PmtEdit { es: &[EsEdit::Audio { pid: target, target: AudioTarget::Aac }], ..Default::default() },
            )
            .unwrap();
            u.replace_section(0, new);
        });
        assert_eq!(learned, Some(target), "ES in the second packet is learned");
        assert_eq!(out.len() % TS_PACKET_SIZE, 0);
        let n = out.len() / TS_PACKET_SIZE;
        assert_eq!(n, 2);
        // Reassemble the output and check it.
        let mut asm = crate::engine::ts_parse::SectionAssembler::new();
        let mut got = Vec::new();
        for i in 0..n {
            got.extend(asm.push_packet(&out[i * 188..(i + 1) * 188]).map(|s| s.to_vec()));
        }
        assert_eq!(got.len(), 1);
        assert!(crc_ok(&got[0]));
        let v = parse_pmt(&got[0]).unwrap();
        assert_eq!(v.es.iter().find(|e| e.pid == target).unwrap().stream_type, 0x0F);
        // CC continuous and equal to the source's.
        assert_eq!(out[3] & 0x0F, 3);
        assert_eq!(out[188 + 3] & 0x0F, 4);
    }

    #[test]
    fn a_lost_or_damaged_packet_mid_unit_drops_the_unit() {
        // A lost middle packet: the section never completes, and the next
        // unit's PUSI (pointer 0) truncates it.
        let big = pmt_section(7, 0, 0x100, &[0u8; 500], &[(0x03, 0x101, &[])]);
        let pkts = packetize_sections(0x40, &[&big], 0);
        assert_eq!(pkts.len(), 3);
        let next = packetize_sections(0x40, &[&big], 3);
        let mut stage = PsiUnitStage::new("test");
        let mut units = 0;
        let out = run_stage(&mut stage, &[pkts[0], pkts[2], next[0], next[1], next[2]], |_| units += 1);
        assert_eq!(units, 1, "only the intact second unit");
        assert_eq!(out.len(), 3 * TS_PACKET_SIZE);
        assert_eq!(stage.dropped_packets(), 2);
        // A damaged continuation: the reassembled section fails its CRC.
        let (sec, _) = two_packet_pmt(7, 1);
        let mut pkts = packetize_sections(0x40, &[&sec], 3);
        pkts[1][6] ^= 0x01; // inside the section tail carried by packet 2
        let mut stage = PsiUnitStage::new("test");
        let mut units = 0;
        let out = run_stage(&mut stage, &pkts, |_| units += 1);
        assert_eq!(units, 0);
        assert!(out.is_empty());
        assert_eq!(stage.dropped_packets(), 2);
    }

    #[test]
    fn a_muxer_that_never_advances_the_cc_still_gets_through() {
        let (sec, _) = two_packet_pmt(7, 1);
        let mut pkts = packetize_sections(0x40, &[&sec], 0);
        pkts[1][3] &= 0xF0; // CC 0 on both packets
        let mut stage = PsiUnitStage::new("test");
        let mut units = 0;
        let mut out = Vec::new();
        for _ in 0..3 {
            out = run_stage(&mut stage, &pkts, |_| units += 1);
        }
        assert_eq!(units, 3);
        assert_eq!(out, [pkts[0], pkts[1]].concat());
    }

    #[test]
    fn shrinking_unit_owns_the_cc_afterwards() {
        // Two-packet PMT whose edit drops enough descriptors to fit one packet.
        let descs = [0x0E, 0x03, 0xC0, 0x01, 0x40];
        let es: Vec<(u8, u16, &[u8])> = (0..20u16).map(|i| (0x03, 0x200 + i, &descs[..])).collect();
        let sec = pmt_section(1, 0, 0x100, &[], &es);
        assert!(sec.len() > 183 && sec.len() < 183 + 70);
        let pkts = packetize_sections(0x40, &[&sec], 0);
        let mut stage = PsiUnitStage::new("test");
        let edits: Vec<EsEdit> = (0..20u16)
            .map(|i| EsEdit::Audio { pid: 0x200 + i, target: AudioTarget::Aac })
            .collect();
        let mut all = Vec::new();
        for round in 0..2 {
            let out = run_stage(&mut stage, &pkts, |u| {
                let new = rebuild_pmt_section(&u.sections()[0], &PmtEdit { es: &edits, ..Default::default() }).unwrap();
                u.replace_section(0, new);
            });
            assert_eq!(out.len(), TS_PACKET_SIZE, "round {round}: shrank to one packet");
            all.extend_from_slice(&out);
        }
        // Second round's CC continues from the first (owned counter).
        assert_eq!((all[3] + 1) & 0x0F, all[188 + 3] & 0x0F);
    }

    #[test]
    fn pcr_on_the_pmt_pid_survives_an_edit() {
        let src = sky_sports_pmt();
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40;
        pkt[2] = 0x40;
        pkt[3] = 0x35;
        pkt[4] = 7;
        pkt[5] = 0x10; // PCR flag
        pkt[6..12].copy_from_slice(&[1, 2, 3, 4, 0x7E, 5]);
        pkt[12] = 0; // pointer
        pkt[13..13 + src.len()].copy_from_slice(&src);
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &[pkt], |u| {
            let new = rebuild_pmt_section(
                &u.sections()[0],
                &PmtEdit { es: &[EsEdit::Audio { pid: 0xC9, target: AudioTarget::Aac }], ..Default::default() },
            )
            .unwrap();
            u.replace_section(0, new);
        });
        assert_eq!(&out[..12], &pkt[..12], "header + AF with PCR kept");
        assert!(verify_psi_crc(&out, 13));
    }

    #[test]
    fn restamp_reaches_a_two_packet_pmt_and_spares_other_sections() {
        let (sec, _) = two_packet_pmt(7, 3);
        let pkts = packetize_sections(0x40, &[&sec], 5);
        let out = restamp_pmt_unit(&pkts, 11);
        assert_eq!(out.len(), 2);
        let mut asm = crate::engine::ts_parse::SectionAssembler::new();
        let got: Vec<Vec<u8>> = out.iter().flat_map(|p| asm.push_packet(p).map(|s| s.to_vec()).collect::<Vec<_>>()).collect();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0][5] >> 1) & 0x1F, 11);
        assert!(crc_ok(&got[0]));
        // VH1: the 0xC0 section ahead of the PMT is untouched.
        let vh1 = vh1_pmt_packet();
        let out = restamp_pmt_unit(&[vh1], 4);
        assert_eq!(&out[0][..VH1_PMT_OFFSET], &vh1[..VH1_PMT_OFFSET]);
        assert_eq!((out[0][VH1_PMT_OFFSET + 5] >> 1) & 0x1F, 4);
        assert!(verify_psi_crc(&out[0], VH1_PMT_OFFSET));
    }

    /// An AF-only packet on the PMT PID (a PCR) arriving between the
    /// packets of a held two-packet unit. It repeats the CC of the payload
    /// packet before it, so it must go out right behind that packet: the
    /// stage used to write it at once, ahead of the held unit — a CC error
    /// at the receiver on every repetition and the PCR moved in front of
    /// the PMT bytes.
    #[test]
    fn af_only_packets_keep_their_place_behind_a_held_unit() {
        let (sec, target) = two_packet_pmt(7, 1);
        let pkts = packetize_sections(0x40, &[&sec], 3);
        let mut pcr = [0xFFu8; TS_PACKET_SIZE];
        pcr[0] = TS_SYNC_BYTE;
        pcr[1] = 0x00;
        pcr[2] = 0x40;
        pcr[3] = 0x20 | (pkts[0][3] & 0x0F); // AF only, CC of the packet before it
        pcr[4] = 183;
        pcr[5] = 0x10; // PCR flag
        pcr[6..12].copy_from_slice(&[1, 2, 3, 4, 0x7E, 5]);
        let src = [pkts[0], pcr, pkts[1]];

        // Unchanged unit: byte-identical, in source order.
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &src, |_| {});
        assert_eq!(out, src.concat(), "source order");

        // Edited unit (same packet count, source CCs kept): the AF-only
        // packet still follows the first packet and repeats its CC.
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &src, |u| {
            let new = rebuild_pmt_section(
                &u.sections()[0],
                &PmtEdit { es: &[EsEdit::Audio { pid: target, target: AudioTarget::Aac }], ..Default::default() },
            )
            .unwrap();
            u.replace_section(0, new);
        });
        let p: Vec<&[u8]> = out.chunks(TS_PACKET_SIZE).collect();
        assert_eq!(p.len(), 3);
        assert_eq!(p[1], &pcr[..], "the PCR packet in second place, unchanged");
        assert_eq!(p[0][3] & 0x0F, p[1][3] & 0x0F, "AF-only repeats the payload CC before it");
        assert_eq!((p[1][3] + 1) & 0x0F, p[2][3] & 0x0F);

        // Unit dropped (damaged continuation): the PCR packet still goes out.
        let mut bad = pkts[1];
        bad[6] ^= 0x01;
        let mut stage = PsiUnitStage::new("test");
        let out = run_stage(&mut stage, &[pkts[0], pcr, bad], |_| {});
        assert_eq!(out, pcr.to_vec());
    }

    /// The AC-3 growth fallback must also cover a section the additions
    /// push past the 1021-byte PMT limit: `rebuild_pmt_section` refuses the
    /// grown section outright, and the fallback used to hang off its
    /// success, so the source PMT went out untouched — the old stream_type
    /// over a re-encoded ES.
    #[test]
    fn growth_fallback_covers_a_section_that_would_overflow() {
        // A PMT with section_length 1016, whose AAC ES the AC-3 additions
        // (6 + 3 bytes) would push to 1025 > 1021.
        let fill = vec![0x0A, 0x04, b'e', b'n', b'g', 0x00, 0x52, 0x01, 0x07];
        let mut es: Vec<(u8, u16, &[u8])> = vec![(0x0F, 0x101, &[0x0A, 0x04, b'e', b'n', b'g', 0x00])];
        for i in 0..70u16 {
            es.push((0x06, 0x200 + i, fill.as_slice()));
        }
        let mut sec = pmt_section(1, 0, 0x100, &[], &es);
        let pad = 1016 - (sec.len() - 3);
        // Top up with a private program-level descriptor to hit 1016 exactly.
        let mut pinfo = vec![0xFE, (pad - 2) as u8];
        pinfo.extend(std::iter::repeat_n(0u8, pad - 2));
        sec = pmt_section(1, 0, 0x100, &pinfo, &es);
        assert_eq!(sec.len(), 3 + 1016);
        let edit = [EsEdit::Audio { pid: 0x101, target: AudioTarget::Ac3 { flavour: TsFlavour::Dvb } }];
        assert!(
            rebuild_pmt_section(&sec, &PmtEdit { es: &edit, ..Default::default() }).is_none(),
            "the full edit overflows"
        );
        let pkts = packetize_sections(0x40, &[&sec], 0);
        let mut stage = PsiUnitStage::new("test");
        let mut rebuilt = None;
        run_stage(&mut stage, &pkts, |u| {
            rebuilt = rebuild_pmt_section_fitting(u, 0, &PmtEdit { es: &edit, ..Default::default() });
        });
        let rebuilt = rebuilt.expect("fallback taken");
        assert!(crc_ok(&rebuilt));
        let v = parse_pmt(&rebuilt).unwrap();
        assert_eq!(v.es[0].stream_type, 0x81, "self-identifying ATSC AC-3, no additions");
        assert_eq!(es_descs(&v, 0x101), vec![(0x0A, b"eng\0".to_vec())]);
    }

    #[test]
    fn orphan_continuation_and_duplicates_are_dropped() {
        let big = pmt_section(7, 0, 0x100, &[0u8; 500], &[(0x03, 0x101, &[])]);
        let pkts = packetize_sections(0x40, &[&big], 3);
        let mut stage = PsiUnitStage::new("test");
        // Orphan continuation first, then a duplicated PUSI (restarts the
        // unit) and a duplicated continuation (ignored).
        let out = run_stage(
            &mut stage,
            &[pkts[1], pkts[0], pkts[0], pkts[1], pkts[1], pkts[2]],
            |_| {},
        );
        assert_eq!(out, [pkts[0], pkts[1], pkts[2]].concat());
    }
}
