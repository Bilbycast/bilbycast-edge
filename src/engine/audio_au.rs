// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Audio access-unit framing, shared by every path that decodes TS audio:
//! the TS audio replacer, the demuxer (`ts_demux`) behind the display, SDI,
//! CMAF, RTMP, WebRTC and ST 2110-30 outputs, the HLS in-process remux, and
//! the meters (`audio_decode::PidAudioDecoder`).
//!
//! [`AuCutter`] rebuilds the elementary stream of one audio PID from its TS
//! packets and cuts whole access units (AUs) out of it **as soon as each
//! one's last byte has arrived**, independently of how the source muxer
//! packed AUs into PES packets:
//!
//! - An AU may straddle two PES packets (legal whenever
//!   `data_alignment_indicator` is 0, and done by some broadcast muxers:
//!   Sky Sports Arena splits one AAC frame across a PES boundary three times
//!   a loop). Parsing each PES in isolation lost that AU *and* — because the
//!   next PES then began with its tail — every AU of the next PES.
//! - The PTS in a PES header belongs to the first AU that **commences** in
//!   that PES (ISO/IEC 13818-1 §2.4.3.7). Each PES start is recorded as a
//!   [`CutAu::pes_start`] mark on the first AU cut at or after it.
//! - Decoding per AU instead of per PES removes up to one PES of latency
//!   (seven AAC frames, 149 ms, on Sky) from every re-encoded stream.
//!
//! **Validation.** A header found where the previous AU ended, with the same
//! stream parameters ([`AuHeader::key`]), is trusted. Anywhere else — the
//! first AU, after a resync, after a continuity-counter break — a candidate
//! is only cut once its successor is a consistent header too, or it ends
//! exactly on a PES boundary; otherwise it is a false sync inside a payload
//! and the cutter moves on a byte. A candidate a scan found (not where an AU
//! ended, not at a PES start) with other parameters than the stream's last
//! AU is dropped at once, without waiting for the length it claims. An AU
//! into which a new PES begins with a header of its own — of the same
//! parameters, or of others with a consistent successor (a playlist item or
//! a splice with another configuration) — was cut short upstream (the
//! truncated last PES of a file at a loop wrap) and is discarded, never
//! glued to the next PES's bytes. AC-3 and E-AC-3 frames share a key: an
//! AC-3 core followed by E-AC-3 dependent substreams (Annex E) is one
//! stream.
//!
//! **E-AC-3 dependent substreams** (7.1 = a 5.1 independent frame plus a
//! dependent frame with the other two channels) are cut *into* the
//! independent frame before them: one AU carries the whole time slot.
//! libavcodec merges a dependent frame only when it follows its independent
//! frame in the same packet, and ignores one sent on its own — every decode
//! path here sends one AU per `send_packet`, so 7.1 decoded as its 5.1 core.
//! An independent frame is therefore held until the bytes after it show no
//! dependent frame follows (the next header, or the end of its PES); a
//! dependent frame with no independent frame before it (the first after a
//! join) goes out as it came.

use std::collections::VecDeque;

/// Elementary-stream framing of a source audio codec the replacer decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuFormat {
    /// AAC in ADTS (stream_type 0x0F).
    Adts,
    /// AAC in LATM / LOAS (0x11).
    Loas,
    /// MPEG-1 audio layer I / II (0x03 / 0x04).
    Mpa,
    /// AC-3 (0x80 / 0x81 / 0xC1) and E-AC-3 (0x87 / 0xC2).
    Ac3,
}

impl AuFormat {
    /// The framing of a (DVB-resolved) source stream_type, `None` for one
    /// the replacer does not decode.
    pub fn for_stream_type(stream_type: u8) -> Option<Self> {
        match stream_type {
            0x0F => Some(Self::Adts),
            0x11 => Some(Self::Loas),
            0x03 | 0x04 => Some(Self::Mpa),
            0x80 | 0x81 | 0xC1 | 0x87 | 0xC2 => Some(Self::Ac3),
            _ => None,
        }
    }
}

/// What one AU header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuHeader {
    /// Whole AU length in bytes, header included.
    pub len: usize,
    /// Stream parameters that stay constant from one AU to the next (ADTS
    /// profile + sampling frequency + channel configuration, MPEG audio layer
    /// + rate, the AC-3 / E-AC-3 rate). A "successor" with another key is
    /// not the next AU, and a candidate the chain does not vouch for whose
    /// key differs from the stream's last AU is a false sync unless a PES
    /// begins with it (see [`AuCutter::next`]).
    pub key: u32,
    /// Nominal duration in samples at [`Self::sample_rate`]; 0 when the
    /// header does not carry it (LOAS, an E-AC-3 dependent substream).
    pub samples: u32,
    /// Sample rate the header declares (the AAC core rate for SBR streams).
    pub sample_rate: u32,
}

/// Outcome of parsing a header at the start of a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    /// Too few bytes to decide.
    NeedMore,
    /// Not a header of this format.
    Invalid,
    Valid(AuHeader),
}

/// Parse the AU header at the start of `buf`.
pub fn parse_header(fmt: AuFormat, buf: &[u8]) -> Head {
    match fmt {
        AuFormat::Adts => adts_header(buf),
        AuFormat::Loas => loas_header(buf),
        AuFormat::Mpa => mpa_header(buf),
        AuFormat::Ac3 => ac3_header(buf),
    }
}

/// ADTS (ISO/IEC 14496-3 §1.A.2.2): 12-bit sync, layer `00`.
fn adts_header(buf: &[u8]) -> Head {
    if let Some(&b0) = buf.first()
        && b0 != 0xFF
    {
        return Head::Invalid;
    }
    if let Some(&b1) = buf.get(1)
        && (b1 & 0xF6) != 0xF0
    {
        return Head::Invalid;
    }
    if buf.len() < 7 {
        return Head::NeedMore;
    }
    let profile = buf[2] >> 6;
    let sfi = (buf[2] >> 2) & 0x0F;
    let Some(sample_rate) = super::audio_decode::sample_rate_from_index(sfi) else {
        return Head::Invalid;
    };
    let channel_config = ((buf[2] & 0x01) << 2) | (buf[3] >> 6);
    let len = (((buf[3] & 0x03) as usize) << 11) | ((buf[4] as usize) << 3) | ((buf[5] as usize) >> 5);
    let header_len = if buf[1] & 0x01 != 0 { 7 } else { 9 };
    if len <= header_len {
        return Head::Invalid;
    }
    let raw_data_blocks = (buf[6] & 0x03) as u32 + 1;
    Head::Valid(AuHeader {
        len,
        key: ((profile as u32) << 8) | ((sfi as u32) << 4) | channel_config as u32,
        samples: 1024 * raw_data_blocks,
        sample_rate,
    })
}

/// LOAS AudioSyncStream (ISO/IEC 14496-3 §1.7.3): 11-bit sync `0x2B7`,
/// 13-bit length of the AudioMuxElement that follows.
fn loas_header(buf: &[u8]) -> Head {
    if let Some(&b0) = buf.first()
        && b0 != 0x56
    {
        return Head::Invalid;
    }
    if let Some(&b1) = buf.get(1)
        && (b1 & 0xE0) != 0xE0
    {
        return Head::Invalid;
    }
    if buf.len() < 3 {
        return Head::NeedMore;
    }
    let payload = (((buf[1] & 0x1F) as usize) << 8) | buf[2] as usize;
    if payload == 0 {
        return Head::Invalid;
    }
    Head::Valid(AuHeader { len: 3 + payload, key: 0, samples: 0, sample_rate: 0 })
}

/// MPEG-1 audio layer I / II header (ISO/IEC 11172-3 §2.4.2.3). Only
/// MPEG-1 (`ID` = 1, version bits `11`) is accepted — the same set
/// `audio_decode::split_mp2_frames` splits for the other decode paths.
pub(crate) fn mpa_header(buf: &[u8]) -> Head {
    /// Layer II bitrates (kbps); index 0 (free format) and 15 are rejected.
    const L2_KBPS: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    /// Layer I bitrates (kbps).
    const L1_KBPS: [u32; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const RATES: [u32; 3] = [44_100, 48_000, 32_000];
    if let Some(&b0) = buf.first()
        && b0 != 0xFF
    {
        return Head::Invalid;
    }
    if let Some(&b1) = buf.get(1)
        && ((b1 & 0xF0) != 0xF0 || (b1 >> 3) & 0x03 != 0b11 || !matches!((b1 >> 1) & 0x03, 0b10 | 0b11))
    {
        return Head::Invalid;
    }
    if buf.len() < 4 {
        return Head::NeedMore;
    }
    let layer = (buf[1] >> 1) & 0x03;
    let bitrate_idx = (buf[2] >> 4) as usize;
    let sr_idx = ((buf[2] >> 2) & 0x03) as usize;
    let padding = ((buf[2] >> 1) & 0x01) as u32;
    if bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return Head::Invalid;
    }
    let sample_rate = RATES[sr_idx];
    let (len, samples) = if layer == 0b10 {
        (144 * L2_KBPS[bitrate_idx] * 1000 / sample_rate + padding, 1152)
    } else {
        ((12 * L1_KBPS[bitrate_idx] * 1000 / sample_rate + padding) * 4, 384)
    };
    if len < 4 {
        return Head::Invalid;
    }
    Head::Valid(AuHeader {
        len: len as usize,
        key: ((layer as u32) << 4) | sr_idx as u32,
        samples,
        sample_rate,
    })
}

/// AC-3 (ATSC A/52 §5.3.1) / E-AC-3 (Annex E) syncframe header.
fn ac3_header(buf: &[u8]) -> Head {
    if let Some(&b0) = buf.first()
        && b0 != 0x0B
    {
        return Head::Invalid;
    }
    if let Some(&b1) = buf.get(1)
        && b1 != 0x77
    {
        return Head::Invalid;
    }
    if buf.len() < super::audio_decode::AC3_MIN_HEADER_BYTES {
        return Head::NeedMore;
    }
    let Some(len) = super::audio_decode::ac3_frame_size(buf) else {
        return Head::Invalid;
    };
    if len < super::audio_decode::AC3_MIN_HEADER_BYTES {
        return Head::Invalid;
    }
    const RATES: [u32; 3] = [48_000, 44_100, 32_000];
    // The key is the sample-rate code alone: AC-3 and E-AC-3 syncframes
    // interleave legally in one stream (Annex E: an AC-3 core as
    // independent substream 0, E-AC-3 dependent substreams extending it to
    // 7.1), so the bitstream family must not make a frame's successor look
    // like another stream's.
    let fscod = buf[4] >> 6;
    let bsid = buf[5] >> 3;
    if bsid <= 10 {
        // `ac3_frame_size` rejected fscod 3.
        return Head::Valid(AuHeader {
            len,
            key: fscod as u32,
            samples: 1536,
            sample_rate: RATES[fscod as usize],
        });
    }
    // E-AC-3: strmtyp 3 is reserved; a dependent substream (1) belongs to
    // the time slot of the independent frame before it.
    let strmtyp = buf[2] >> 6;
    if strmtyp == 3 {
        return Head::Invalid;
    }
    let (key, sample_rate, blocks) = if fscod == 3 {
        let fscod2 = (buf[4] >> 4) & 0x03;
        if fscod2 == 3 {
            return Head::Invalid;
        }
        (3 + fscod2 as u32, [24_000, 22_050, 16_000][fscod2 as usize], 6)
    } else {
        (fscod as u32, RATES[fscod as usize], [1, 2, 3, 6][((buf[4] >> 4) & 0x03) as usize])
    };
    Head::Valid(AuHeader {
        len,
        key,
        samples: if strmtyp == 1 { 0 } else { 256 * blocks },
        sample_rate,
    })
}

/// Decode the 5-byte PTS / DTS field of a PES header (ISO/IEC 13818-1
/// §2.4.3.7).
pub(crate) fn parse_pes_timestamp(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30 | (b1 << 22) | ((b2 >> 1) & 0x7F) << 15 | (b3 << 7) | ((b4 >> 1) & 0x7F)
}

/// One access unit cut from the stream.
#[derive(Debug)]
pub struct CutAu {
    /// The AU's bytes, header included.
    pub data: Vec<u8>,
    /// A PES began at or before this AU (and after the previous one): this is
    /// the first AU that commences in that PES.
    pub pes_start: bool,
    /// That PES's PTS; `None` when the PES carried none or `pes_start` is
    /// false.
    pub pts: Option<u64>,
    pub header: AuHeader,
}

/// Where each PES began in the byte stream, and its PTS.
#[derive(Clone, Copy, Debug)]
struct Mark {
    offset: u64,
    pts: Option<u64>,
}

/// What [`AuCutter::pes_inside`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inside {
    /// No PES starts inside with a header of its own.
    None,
    /// One does, `rel` bytes in, with this key.
    Header { rel: usize, key: u32 },
    /// A PES starts inside, but the bytes to judge it have not arrived.
    Wait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PesState {
    /// No PES open (before the first PUSI, or after a malformed header):
    /// payload is ignored until the next PUSI.
    Idle,
    /// Collecting a PES header that may span TS packets.
    Header,
    /// Collecting ES bytes.
    Payload,
}

/// Bound on ES bytes held uncut: a stream that never yields a valid AU is
/// discarded from the front instead of growing without bound. Far above the
/// largest AU (an 8 KB ADTS frame; E-AC-3 at most 4 KB).
const MAX_BUFFERED: usize = 64 * 1024;

/// Continuous-ES access-unit cutter for one audio PID. See the module doc.
#[derive(Debug)]
pub struct AuCutter {
    fmt: AuFormat,
    /// ES bytes not yet cut.
    buf: Vec<u8>,
    /// Absolute ES offset of `buf[0]`.
    base: u64,
    /// PES starts at or after `base` (plus at most one before it whose
    /// bytes were discarded), oldest first.
    marks: VecDeque<Mark>,
    state: PesState,
    /// The PES header being assembled.
    header: Vec<u8>,
    /// ES bytes the open PES still owns, when PES_packet_length said.
    pes_left: Option<usize>,
    /// Absolute offset where the open PES's ES ends, when known.
    pes_end: Option<u64>,
    /// `Some(key)` while the head of `buf` is exactly where the last cut AU
    /// (of that key) ended, with no byte lost in between.
    chained: Option<u32>,
    /// Key of the last AU cut, kept across resyncs (only a new cutter
    /// forgets it): the stream's parameters, which a candidate found by
    /// scanning must share unless a PES begins with it.
    last_key: Option<u32>,
    /// Bytes discarded (resync, truncated AUs, overflow) since the last
    /// [`Self::take_discarded`].
    discarded: u64,
    /// Continuity counter and payload of the last packet taken by
    /// [`Self::push_packet`].
    cc: Option<u8>,
    last_payload: Vec<u8>,
    /// An AC-3 / E-AC-3 independent frame waiting for the dependent
    /// substream frames that may follow it (see the module doc).
    held: Option<CutAu>,
}

impl AuCutter {
    pub fn new(fmt: AuFormat) -> Self {
        Self {
            fmt,
            buf: Vec::with_capacity(16 * 1024),
            base: 0,
            marks: VecDeque::new(),
            state: PesState::Idle,
            header: Vec::with_capacity(32),
            pes_left: None,
            pes_end: None,
            chained: None,
            last_key: None,
            discarded: 0,
            cc: None,
            last_payload: Vec::with_capacity(184),
            held: None,
        }
    }

    /// Feed one whole TS packet of the audio PID: its payload, after a
    /// continuity check. A counter that skips is a lost packet
    /// ([`Self::mark_discontinuity`]); a packet with the counter and payload
    /// of the one before is a duplicate (ISO/IEC 13818-1 §2.4.3.3) and is
    /// dropped. `false` for a packet that brought nothing (no payload, a
    /// duplicate).
    pub fn push_packet(&mut self, pkt: &[u8]) -> bool {
        use super::ts_parse::{ts_cc, ts_has_payload, ts_payload_offset, ts_pusi, TS_PACKET_SIZE};
        if !ts_has_payload(pkt) {
            return false;
        }
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE || payload_start >= pkt.len() {
            return false;
        }
        let payload = &pkt[payload_start..];
        let cc = ts_cc(pkt);
        if let Some(last) = self.cc {
            if cc == last && payload == self.last_payload.as_slice() {
                return false;
            }
            if cc != (last + 1) & 0x0F {
                self.mark_discontinuity();
            }
        }
        self.cc = Some(cc);
        self.last_payload.clear();
        self.last_payload.extend_from_slice(payload);
        self.push(ts_pusi(pkt), payload);
        true
    }

    /// ES bytes held that no AU has been cut from yet, or that an AU not
    /// handed out yet carries: 0 when everything that arrived is out (a PES
    /// ended on an AU boundary).
    pub fn buffered(&self) -> usize {
        self.buf.len() + self.held.as_ref().map_or(0, |h| h.data.len())
    }

    pub fn format(&self) -> AuFormat {
        self.fmt
    }

    /// Bytes discarded since the last call (a resync, an AU cut short
    /// upstream, overflow), for the caller's error accounting.
    pub fn take_discarded(&mut self) -> u64 {
        std::mem::take(&mut self.discarded)
    }

    /// Bytes are missing from the stream (a TS continuity-counter break):
    /// the AU in flight may be damaged, so the next one is validated by its
    /// successor before it is trusted.
    pub fn mark_discontinuity(&mut self) {
        self.chained = None;
        self.pes_left = None;
        self.pes_end = None;
    }

    /// Feed the payload of one TS packet of the audio PID.
    pub fn push(&mut self, pusi: bool, payload: &[u8]) {
        if pusi {
            // A new PES: whatever the previous one still owed never came.
            self.state = PesState::Header;
            self.header.clear();
            self.pes_left = None;
            self.pes_end = None;
            self.feed_header(payload);
            return;
        }
        match self.state {
            PesState::Idle => {}
            PesState::Header => self.feed_header(payload),
            PesState::Payload => self.append_es(payload),
        }
    }

    fn feed_header(&mut self, bytes: &[u8]) {
        self.header.extend_from_slice(bytes);
        if self.header.len() < 9 {
            return;
        }
        if self.header[..3] != [0x00, 0x00, 0x01] {
            self.state = PesState::Idle;
            self.header.clear();
            return;
        }
        let header_data_len = self.header[8] as usize;
        let es_start = 9 + header_data_len;
        if self.header.len() < es_start {
            return;
        }
        let pes_len = u16::from_be_bytes([self.header[4], self.header[5]]) as usize;
        let pts = ((self.header[7] >> 6) >= 2 && header_data_len >= 5)
            .then(|| parse_pes_timestamp(&self.header[9..14]));
        let offset = self.base + self.buf.len() as u64;
        self.pes_left = (pes_len > 0).then(|| pes_len.saturating_sub(3 + header_data_len));
        self.pes_end = self.pes_left.map(|n| offset + n as u64);
        self.marks.push_back(Mark { offset, pts });
        self.state = PesState::Payload;
        let rest = self.header.split_off(es_start);
        self.header.clear();
        self.append_es(&rest);
    }

    fn append_es(&mut self, bytes: &[u8]) {
        let take = match self.pes_left.as_mut() {
            Some(left) => {
                let n = bytes.len().min(*left);
                *left -= n;
                n
            }
            None => bytes.len(),
        };
        self.buf.extend_from_slice(&bytes[..take]);
        if self.buf.len() > MAX_BUFFERED {
            let excess = self.buf.len() - MAX_BUFFERED / 2;
            self.discard(excess);
        }
    }

    /// Drop `n` bytes from the front. The head is no longer where an AU
    /// ended.
    fn discard(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        self.buf.drain(..n);
        self.base += n as u64;
        self.discarded += n as u64;
        self.chained = None;
        // Keep at most one mark before the head: the next AU cut belongs to
        // the latest PES that began before it.
        while self.marks.len() >= 2 && self.marks[1].offset <= self.base {
            self.marks.pop_front();
        }
    }

    /// Skip to the next position a header of this format could start at
    /// (possibly one that needs more bytes to judge).
    fn resync(&mut self) {
        let skip = (1..self.buf.len())
            .find(|&i| parse_header(self.fmt, &self.buf[i..]) != Head::Invalid)
            .unwrap_or(self.buf.len());
        self.discard(skip);
    }

    /// Whether a PES begins inside the AU at `start..end` (of key `key`)
    /// with an AU header of its own. One of the AU's own key is taken at its
    /// word; one of another key (the next PES is another stream: a playlist
    /// item or a splice with another configuration) only once its own
    /// successor agrees or it ends on a PES boundary, since the continuation
    /// of an AU straddling into that PES could look like a header by chance.
    /// Every PES start inside is looked at, not only the first: the first
    /// can open with such a continuation.
    fn pes_inside(&self, start: u64, end: u64, key: u32, at_end: bool) -> Inside {
        for m in self.marks.iter().filter(|m| m.offset > start && m.offset < end) {
            let rel = (m.offset - start) as usize;
            let Some(bytes) = self.buf.get(rel..) else {
                break;
            };
            match parse_header(self.fmt, bytes) {
                Head::Valid(next) if next.key == key => {
                    return Inside::Header { rel, key };
                }
                Head::Valid(next) => match self.confirmed(rel, next, at_end) {
                    Some(true) => return Inside::Header { rel, key: next.key },
                    Some(false) => {}
                    None => return Inside::Wait,
                },
                Head::NeedMore if !at_end => return Inside::Wait,
                _ => {}
            }
        }
        Inside::None
    }

    /// Whether the header `h` found `rel` bytes into the buffer has a
    /// consistent successor: a header of its key right after it, or a PES
    /// boundary there. `None` until the bytes to judge have arrived.
    fn confirmed(&self, rel: usize, h: AuHeader, at_end: bool) -> Option<bool> {
        let after = rel + h.len;
        let abs = self.base + after as u64;
        if self.pes_end == Some(abs) || self.marks.iter().any(|m| m.offset == abs) {
            return Some(true);
        }
        match self.buf.get(after..).map(|b| parse_header(self.fmt, b)) {
            Some(Head::Valid(n)) => Some(n.key == h.key),
            Some(Head::Invalid) => Some(false),
            Some(Head::NeedMore) | None => at_end.then_some(true),
        }
    }

    /// The next complete, validated AU, if one is ready. `at_end` (a
    /// shutdown flush) accepts an AU whose successor has not arrived and
    /// discards an incomplete tail. An AC-3 / E-AC-3 AU carries its
    /// dependent substream frames (see the module doc).
    pub fn next(&mut self, at_end: bool) -> Option<CutAu> {
        if self.fmt != AuFormat::Ac3 {
            return self.next_frame(at_end);
        }
        loop {
            match self.next_frame(at_end) {
                // An E-AC-3 dependent substream frame: part of the time slot
                // of the independent frame before it.
                Some(au) if au.header.samples == 0 => match self.held.as_mut() {
                    Some(h) => {
                        h.data.extend_from_slice(&au.data);
                        h.header.len = h.data.len();
                        if !h.pes_start && au.pes_start {
                            h.pes_start = true;
                            h.pts = au.pts;
                        }
                    }
                    None => return Some(au),
                },
                Some(au) => {
                    if let Some(prev) = self.held.replace(au) {
                        return Some(prev);
                    }
                }
                None => {
                    return if at_end || self.held_is_whole() { self.held.take() } else { None };
                }
            }
        }
    }

    /// Whether no dependent frame can follow the held one any more: the
    /// bytes after it open with anything but a dependent frame's header, or
    /// its PES ended with it.
    fn held_is_whole(&self) -> bool {
        if self.held.is_none() {
            return false;
        }
        match parse_header(self.fmt, &self.buf) {
            Head::Valid(h) => h.samples != 0,
            Head::Invalid => true,
            Head::NeedMore => self.buf.is_empty() && self.pes_end == Some(self.base),
        }
    }

    /// The next frame [`Self::next`] is built from: one syncframe for AC-3
    /// / E-AC-3, the whole AU for the other formats.
    fn next_frame(&mut self, at_end: bool) -> Option<CutAu> {
        loop {
            if self.buf.is_empty() {
                return None;
            }
            let h = match parse_header(self.fmt, &self.buf) {
                Head::Valid(h) => h,
                Head::Invalid => {
                    self.resync();
                    continue;
                }
                Head::NeedMore => {
                    if at_end {
                        self.discard(self.buf.len());
                    }
                    return None;
                }
            };
            let start = self.base;
            let end = start + h.len as u64;
            // Where this candidate stands: where the last AU ended, or at
            // the start of a PES — the stream's own alignment — or somewhere
            // a scan for a sync landed.
            let aligned =
                self.chained.is_some() || self.marks.iter().any(|m| m.offset == start);
            // A scanned candidate with other stream parameters than the
            // stream's last AU is a false sync inside a payload: move on now
            // instead of waiting out the length it claims (up to 8 KB of
            // ADTS, half a second of audio that would then leave in a burst,
            // late against the PCR).
            if !aligned && self.last_key.is_some_and(|k| k != h.key) {
                self.discard(1);
                continue;
            }
            // A PES that begins inside this AU with a header of its own.
            match self.pes_inside(start, end, h.key, at_end) {
                Inside::Header { rel, key } if aligned => {
                    // This AU was cut short upstream (a file's truncated
                    // last PES at a loop wrap, a splice): drop it and go on
                    // at that PES, whose header is the stream's own
                    // alignment.
                    self.discard(rel);
                    self.chained = Some(key);
                    continue;
                }
                Inside::Header { .. } => {
                    // A scanned candidate spanning a real AU: a false sync.
                    // Scanning on (rather than jumping to the PES) keeps any
                    // real AU between the two.
                    self.discard(1);
                    continue;
                }
                Inside::Wait => return None,
                Inside::None => {}
            }
            if h.len > self.buf.len() {
                if at_end {
                    self.discard(self.buf.len());
                }
                return None;
            }
            if self.chained != Some(h.key) {
                let on_boundary =
                    self.pes_end == Some(end) || self.marks.iter().any(|m| m.offset == end);
                if !on_boundary {
                    match parse_header(self.fmt, &self.buf[h.len..]) {
                        Head::Valid(next) if next.key == h.key => {}
                        Head::NeedMore if !at_end => return None,
                        Head::NeedMore => {}
                        _ => {
                            // A false sync inside a payload.
                            self.discard(1);
                            continue;
                        }
                    }
                }
            }
            let data: Vec<u8> = self.buf.drain(..h.len).collect();
            self.base = end;
            self.chained = Some(h.key);
            self.last_key = Some(h.key);
            let mut pes_start = false;
            let mut pts = None;
            while let Some(m) = self.marks.front().copied() {
                if m.offset > start {
                    break;
                }
                pes_start = true;
                pts = m.pts;
                self.marks.pop_front();
            }
            return Some(CutAu { data, pes_start, pts, header: h });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ADTS frame of `len` bytes (AAC-LC, 48 kHz, stereo, no CRC) whose
    /// body is `fill`.
    fn adts(len: usize, fill: u8) -> Vec<u8> {
        let mut f = vec![fill; len];
        f[0] = 0xFF;
        f[1] = 0xF1;
        f[2] = (1 << 6) | (3 << 2); // LC, 48 kHz, channel_config high bit 0
        f[3] = (2 << 6) | ((len >> 11) as u8 & 0x03); // stereo
        f[4] = (len >> 3) as u8;
        f[5] = ((len as u8) << 5) | 0x1F;
        f[6] = 0xFC;
        f
    }

    /// A PES carrying `es`, PTS `pts`, PES_packet_length set, data
    /// alignment indicator 0.
    fn pes(es: &[u8], pts: Option<u64>) -> Vec<u8> {
        let hdl = if pts.is_some() { 5 } else { 0 };
        let mut p = vec![0x00, 0x00, 0x01, 0xC0];
        p.extend_from_slice(&((3 + hdl + es.len()) as u16).to_be_bytes());
        p.push(0x80);
        p.push(if pts.is_some() { 0x80 } else { 0x00 });
        p.push(hdl as u8);
        if let Some(t) = pts {
            p.push(0x21 | (((t >> 29) as u8) & 0x0E));
            p.push((t >> 22) as u8);
            p.push((((t >> 14) as u8) & 0xFE) | 1);
            p.push((t >> 7) as u8);
            p.push(((t << 1) as u8) | 1);
        }
        p.extend_from_slice(es);
        p
    }

    /// Feed a PES in 184-byte TS payload slices.
    fn feed(c: &mut AuCutter, pes: &[u8]) -> Vec<CutAu> {
        let mut out = Vec::new();
        for (i, chunk) in pes.chunks(184).enumerate() {
            c.push(i == 0, chunk);
            while let Some(au) = c.next(false) {
                out.push(au);
            }
        }
        out
    }

    #[test]
    fn parses_the_frame_lengths_of_every_format() {
        let a = adts(342, 0x11);
        assert_eq!(
            parse_header(AuFormat::Adts, &a),
            Head::Valid(AuHeader { len: 342, key: (1 << 8) | (3 << 4) | 2, samples: 1024, sample_rate: 48_000 })
        );
        assert_eq!(parse_header(AuFormat::Adts, &a[..5]), Head::NeedMore);
        assert_eq!(parse_header(AuFormat::Adts, &[0x12, 0x34]), Head::Invalid);
        // MP2: 384 kbps / 48 kHz layer II = 1152 bytes, 1152 samples.
        let mp2 = [0xFF, 0xFD, 0xE4, 0x00];
        assert!(matches!(parse_header(AuFormat::Mpa, &mp2), Head::Valid(h) if h.len == 1152 && h.samples == 1152));
        // AC-3 448 kbps 48 kHz: frmsizecod 30 = 896 words.
        let ac3 = [0x0B, 0x77, 0, 0, 30, 8 << 3];
        assert!(matches!(parse_header(AuFormat::Ac3, &ac3), Head::Valid(h) if h.len == 1792 && h.samples == 1536));
        // E-AC-3 dependent substream: no duration of its own.
        let dep = [0x0B, 0x77, 0x40 | 0x01, 0x7F, 0x30, 16 << 3];
        assert!(matches!(parse_header(AuFormat::Ac3, &dep), Head::Valid(h) if h.len == 768 && h.samples == 0));
        let loas = [0x56, 0xE0 | 0x01, 0x00];
        assert!(matches!(parse_header(AuFormat::Loas, &loas), Head::Valid(h) if h.len == 3 + 256));
    }

    /// The real Sky Sports Arena pattern: PES k's last AU (412 bytes) has
    /// only 282 bytes in PES k, its 130-byte tail opens PES k+1. Every AU is
    /// cut whole, and PES k+1's PTS goes to its first *whole* AU.
    #[test]
    fn an_au_straddling_two_pes_is_cut_whole() {
        let sizes = [342usize, 375, 474, 458, 421, 394, 412];
        let frames: Vec<Vec<u8>> = sizes.iter().enumerate().map(|(i, &n)| adts(n, i as u8)).collect();
        let es_k: Vec<u8> = frames.concat();
        let split = es_k.len() - 130;
        let next_frames: Vec<Vec<u8>> = (0..6).map(|i| adts(400, 0x40 + i)).collect();
        let mut es_k1 = es_k[split..].to_vec();
        es_k1.extend_from_slice(&next_frames.concat());
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&es_k[..split], Some(90_000)));
        aus.extend(feed(&mut c, &pes(&es_k1, Some(90_000 + 13_440))));
        // Flush the last AU (its successor never arrives).
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 13, "7 + 6 AUs, none lost");
        for (au, want) in aus.iter().zip(frames.iter().chain(next_frames.iter())) {
            assert_eq!(&au.data, want);
        }
        assert_eq!((aus[0].pes_start, aus[0].pts), (true, Some(90_000)));
        assert!(!aus[6].pes_start, "the straddling AU commenced in PES k");
        assert_eq!((aus[7].pes_start, aus[7].pts), (true, Some(90_000 + 13_440)));
        assert_eq!(c.take_discarded(), 0);
    }

    /// Every AU is available as soon as its last byte (and its successor's
    /// header) has arrived — not when the next PES starts.
    #[test]
    fn aus_are_cut_before_their_pes_ends() {
        let es: Vec<u8> = (0..7).map(|i| adts(300, i)).collect::<Vec<_>>().concat();
        let p = pes(&es, Some(0));
        let mut c = AuCutter::new(AuFormat::Adts);
        c.push(true, &p[..184]);
        assert!(c.next(false).is_none(), "first AU not complete yet");
        c.push(false, &p[184..368]);
        let first = c.next(false).expect("AU 1 complete with its successor header in hand");
        assert_eq!(first.data.len(), 300);
    }

    /// A truncated last PES (a file loop wrap) is not glued to the next
    /// PES, whose first AU starts at its first byte.
    #[test]
    fn a_truncated_au_is_dropped_at_the_next_pes() {
        let a = adts(400, 1);
        let b = adts(400, 2);
        let mut es1 = a.clone();
        es1.extend_from_slice(&b[..182]);
        // PES_packet_length claims the whole of b.
        let mut p1 = pes(&[a.clone(), b.clone()].concat(), Some(0));
        p1.truncate(p1.len() - (400 - 182));
        let next: Vec<Vec<u8>> = (0..3).map(|i| adts(350, 0x60 + i)).collect();
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &p1);
        aus.extend(feed(&mut c, &pes(&next.concat(), Some(50_000))));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 4, "a, then the three AUs of the new PES");
        assert_eq!(aus[0].data, a);
        assert_eq!(&aus[1].data, &next[0]);
        assert_eq!((aus[1].pes_start, aus[1].pts), (true, Some(50_000)));
        assert_eq!(c.take_discarded(), 182);
    }

    /// A damaged carry (bytes lost across the boundary) costs exactly the
    /// damaged AU: the cutter resyncs on the next valid header.
    #[test]
    fn a_damaged_carry_loses_one_au() {
        let frames: Vec<Vec<u8>> = (0..4).map(|i| adts(300, 0x20 + i)).collect();
        let es: Vec<u8> = frames.concat();
        // PES 1 ends 100 bytes into frame 2; PES 2 lost 40 bytes of its tail.
        let cut = 600 + 100;
        let mut es2 = es[cut + 40..].to_vec();
        es2.extend_from_slice(&adts(300, 0x30));
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&es[..cut], Some(0)));
        c.mark_discontinuity();
        aus.extend(feed(&mut c, &pes(&es2, Some(10_000))));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        let got: Vec<u8> = aus.iter().map(|a| a.data[7]).collect();
        assert_eq!(got, vec![0x20, 0x21, 0x23, 0x30], "only frame 2 is lost");
        assert!(c.take_discarded() > 0);
    }

    /// A 0xFFF pattern inside a payload is not taken for a sync after a
    /// resync: a candidate needs a consistent successor.
    #[test]
    fn a_false_sync_needs_a_successor() {
        let mut garbage = vec![0u8; 50];
        garbage[10] = 0xFF;
        garbage[11] = 0xF1;
        garbage[12] = 0x4C;
        garbage[13] = 0x80;
        garbage[14] = 0x02; // len 16
        garbage[15] = 0x00;
        let frames: Vec<Vec<u8>> = (0..3).map(|i| adts(200, 0x50 + i)).collect();
        let mut es = garbage;
        es.extend_from_slice(&frames.concat());
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&es, Some(0)));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 3);
        assert_eq!(aus[0].data, frames[0]);
        assert_eq!((aus[0].pes_start, aus[0].pts), (true, Some(0)));
    }

    /// A PES without a PTS still marks a PES start, but carries no time.
    #[test]
    fn a_pts_less_pes_marks_no_time() {
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&[adts(200, 1), adts(200, 2)].concat(), Some(0)));
        aus.extend(feed(&mut c, &pes(&adts(200, 3), None)));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 3);
        assert_eq!((aus[2].pes_start, aus[2].pts), (true, None));
    }

    /// A PES header split across two TS packets is reassembled.
    #[test]
    fn a_pes_header_may_span_packets() {
        let p = pes(&[adts(200, 1), adts(200, 2)].concat(), Some(12_345));
        let mut c = AuCutter::new(AuFormat::Adts);
        c.push(true, &p[..6]);
        c.push(false, &p[6..]);
        let mut aus: Vec<CutAu> = std::iter::from_fn(|| c.next(false)).collect();
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 2);
        assert_eq!(aus[0].pts, Some(12_345));
    }

    /// An ADTS frame of `len` bytes with the given sampling-frequency index
    /// and channel configuration.
    fn adts_with(len: usize, fill: u8, sfi: u8, channel_config: u8) -> Vec<u8> {
        let mut f = adts(len, fill);
        f[2] = (1 << 6) | (sfi << 2) | ((channel_config >> 2) & 0x01);
        f[3] = ((channel_config & 0x03) << 6) | ((len >> 11) as u8 & 0x03);
        f
    }

    /// Feed `es` as TS payload continuing the open PES (no PUSI).
    fn feed_more(c: &mut AuCutter, es: &[u8]) -> Vec<CutAu> {
        let mut out = Vec::new();
        for chunk in es.chunks(184) {
            c.push(false, chunk);
            while let Some(au) = c.next(false) {
                out.push(au);
            }
        }
        out
    }

    /// A PES whose PES_packet_length is 0 (unbounded, as on video PIDs and
    /// some audio muxers): more ES can follow in non-PUSI packets.
    fn open_pes(es: &[u8], pts: Option<u64>) -> Vec<u8> {
        let mut p = pes(es, pts);
        p[4] = 0;
        p[5] = 0;
        p
    }

    /// A file's truncated last PES followed by a PES of another stream
    /// (a playlist moving from a stereo file to a 5.1 one): the partial AU
    /// is dropped, not glued to the new PES's first AU, and the new PES's
    /// PTS goes to that AU.
    #[test]
    fn a_truncated_au_is_dropped_before_a_pes_of_other_parameters() {
        let a1 = adts_with(300, 0x11, 3, 2);
        let a2 = adts_with(400, 0x22, 3, 2);
        let mut es = a1.clone();
        es.extend_from_slice(&a2[..200]);
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&es, Some(1_000)));
        let b: Vec<Vec<u8>> = (0..3).map(|i| adts_with(500, 0x33 + i, 3, 6)).collect();
        aus.extend(feed(&mut c, &pes(&b[..2].concat(), Some(90_000))));
        aus.extend(feed(&mut c, &pes(&b[2], Some(92_000))));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        let got: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
        assert_eq!(got, vec![&a1[..], &b[0][..], &b[1][..], &b[2][..]]);
        assert_eq!((aus[1].pes_start, aus[1].pts), (true, Some(90_000)));
        assert_eq!(c.take_discarded(), 200, "exactly the partial AU");
    }

    /// An AU whose continuation into the next PES happens to parse as a
    /// header of other parameters is still cut whole: another stream's
    /// header counts only with a consistent successor.
    #[test]
    fn a_straddle_whose_continuation_looks_like_a_foreign_header_is_cut_whole() {
        let frames: Vec<Vec<u8>> = (0..4).map(|i| adts(400, 0x40 + i)).collect();
        let mut es = frames.concat();
        // Frame 1's bytes 250.. open PES 2 and begin like a 44.1 kHz ADTS
        // header of 64 bytes, whose "successor" is more of frame 1.
        es[400 + 250..400 + 257].copy_from_slice(&adts_with(64, 0, 4, 2)[..7]);
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &pes(&es[..650], Some(0)));
        aus.extend(feed(&mut c, &pes(&es[650..], Some(3_840))));
        aus.extend(std::iter::from_fn(|| c.next(true)));
        assert_eq!(aus.len(), 4);
        assert_eq!(aus[1].data, es[400..800]);
        assert_eq!((aus[2].pes_start, aus[2].pts), (true, Some(3_840)));
        assert_eq!(c.take_discarded(), 0);
    }

    /// E-AC-3's backward-compatible 7.1 (Annex E): an AC-3 core frame
    /// (bsid 8) followed by an E-AC-3 dependent substream frame (bsid 16).
    /// Both are cut, every PES; the core is not a "false sync" for having
    /// a successor of the other bitstream family.
    #[test]
    fn an_ac3_core_with_e_ac3_dependent_substreams_is_cut_whole() {
        let core = |fill: u8| {
            let mut f = vec![fill; 1792];
            f[..6].copy_from_slice(&[0x0B, 0x77, 0, 0, 30, 8 << 3]);
            f
        };
        let dep = |fill: u8| {
            let mut f = vec![fill; 768];
            f[..6].copy_from_slice(&[0x0B, 0x77, 0x40 | 0x01, 0x7F, 0x30, 16 << 3]);
            f
        };
        let mut c = AuCutter::new(AuFormat::Ac3);
        let mut aus = Vec::new();
        for k in 0..6u8 {
            let es = [core(0x10 + k), dep(0x80 + k)].concat();
            aus.extend(feed(&mut c, &pes(&es, Some(90_000 + k as u64 * 2_880))));
        }
        aus.extend(std::iter::from_fn(|| c.next(true)));
        let lens: Vec<usize> = aus.iter().map(|a| a.data.len()).collect();
        assert_eq!(lens, [1792 + 768].repeat(6), "every core with its dependent frame");
        assert_eq!(c.take_discarded(), 0);
        assert!(aus.iter().all(|a| a.pes_start && a.header.samples == 1536), "each PES's PTS on its core");
        assert!(aus.iter().all(|a| a.header.len == a.data.len()));
    }

    /// A 7.1 E-AC-3 stream (a 5.1 independent frame and the dependent
    /// frame with the other two channels per time slot): each slot is one
    /// AU, handed out as soon as its PES ends — libavcodec merges a
    /// dependent frame only when it arrives in the same packet as its
    /// independent frame, and ignored the ones cut on their own. Several
    /// slots in one PES are cut one by one, each released once the next
    /// slot's header shows it is complete.
    #[test]
    fn a_7_1_e_ac3_time_slot_is_one_au() {
        const ES: &[u8] = include_bytes!("testdata/eac3_5_1_plus_dependent_48k.ec3");
        let slots: Vec<&[u8]> = ES.chunks(768 + 128).collect();
        assert_eq!(slots.len(), 7);
        // One slot per PES: out the moment its PES is complete.
        let mut c = AuCutter::new(AuFormat::Ac3);
        for (k, slot) in slots.iter().enumerate() {
            let aus = feed(&mut c, &pes(slot, Some(90_000 + k as u64 * 2_880)));
            assert_eq!(aus.len(), 1, "slot {k}");
            assert_eq!(aus[0].data, *slot);
            assert_eq!((aus[0].pes_start, aus[0].pts), (true, Some(90_000 + k as u64 * 2_880)));
            assert_eq!(c.buffered(), 0);
        }
        // All seven in one PES.
        let mut c = AuCutter::new(AuFormat::Ac3);
        let mut aus = feed(&mut c, &pes(ES, Some(90_000)));
        aus.extend(std::iter::from_fn(|| c.next(false)));
        assert_eq!(aus.iter().map(|a| a.data.as_slice()).collect::<Vec<_>>(), slots);
        assert_eq!(aus.iter().filter(|a| a.pes_start).count(), 1);
        // A dependent frame with no independent frame ahead of it (a join
        // mid-slot) goes out as it came.
        let mut c = AuCutter::new(AuFormat::Ac3);
        let aus = feed(&mut c, &pes(&ES[768..], Some(90_000)));
        assert_eq!(aus.iter().map(|a| a.data.len()).collect::<Vec<_>>()[..2], [128, 896]);
    }

    /// After a continuity break, a false sync of other stream parameters
    /// inside the damaged AU claiming 6000 bytes is dropped at once: the
    /// next real AU is cut as soon as it is complete, not 6000 bytes later.
    #[test]
    fn a_foreign_false_sync_after_a_break_is_dropped_at_once() {
        let f: Vec<Vec<u8>> = (0..6).map(|i| adts(300, 0x20 + i)).collect();
        let mut head = [f[0].clone(), f[1].clone()].concat();
        head.extend_from_slice(&f[2][..150]);
        let mut damaged = f[2][190..].to_vec();
        // 44.1 kHz mono, 6000 bytes: not this stream's parameters.
        damaged[10..17].copy_from_slice(&adts_with(6000, 0, 4, 1)[..7]);
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &open_pes(&head, Some(0)));
        c.mark_discontinuity();
        aus.extend(feed_more(&mut c, &damaged));
        aus.extend(feed_more(&mut c, &[f[3].clone(), f[4].clone(), f[5].clone()].concat()));
        let got: Vec<u8> = aus.iter().map(|a| a.data[7]).collect();
        assert_eq!(got, vec![0x20, 0x21, 0x23, 0x24, 0x25], "frame 3 on out without waiting");
    }

    /// A false sync of the stream's own parameters claiming 6000 bytes
    /// spans two PES starts: the first opens with the continuation of a
    /// straddling AU, the second with a real header. Every PES start is
    /// looked at, so the real AUs come out without waiting 6000 bytes.
    #[test]
    fn every_pes_start_inside_a_false_sync_is_looked_at() {
        let f: Vec<Vec<u8>> = (0..9).map(|i| adts(300, 0x20 + i)).collect();
        let mut head = [f[0].clone(), f[1].clone()].concat();
        head.extend_from_slice(&f[2][..150]);
        let mut damaged = f[2][190..].to_vec();
        damaged[10..17].copy_from_slice(&adts(6000, 0)[..7]);
        let mut c = AuCutter::new(AuFormat::Adts);
        let mut aus = feed(&mut c, &open_pes(&head, Some(0)));
        c.mark_discontinuity();
        aus.extend(feed_more(&mut c, &damaged));
        // Frame 3 straddles into PES 2, which carries frames 4 and 5.
        aus.extend(feed_more(&mut c, &f[3][..100]));
        let mut es2 = f[3][100..].to_vec();
        es2.extend_from_slice(&[f[4].clone(), f[5].clone()].concat());
        aus.extend(feed(&mut c, &open_pes(&es2, Some(9_000))));
        aus.extend(feed(&mut c, &open_pes(&[f[6].clone(), f[7].clone(), f[8].clone()].concat(), Some(18_000))));
        let got: Vec<u8> = aus.iter().map(|a| a.data[7]).collect();
        assert_eq!(got, vec![0x20, 0x21, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]);
        assert_eq!((aus[3].pes_start, aus[3].pts), (true, Some(9_000)));
        assert_eq!((aus[5].pes_start, aus[5].pts), (true, Some(18_000)));
    }

    /// Straddles are cut the same way for the libavcodec formats.
    #[test]
    fn mpa_and_ac3_straddles_are_cut_whole() {
        // MP2 384 kbps 48 kHz = 1152-byte frames.
        let mp2 = |fill: u8| {
            let mut f = vec![fill; 1152];
            f[..4].copy_from_slice(&[0xFF, 0xFD, 0xE4, 0x00]);
            f
        };
        let ac3 = |fill: u8| {
            let mut f = vec![fill; 1792];
            f[..6].copy_from_slice(&[0x0B, 0x77, 0, 0, 30, 8 << 3]);
            f
        };
        for (fmt, frames) in [
            (AuFormat::Mpa, (0..4).map(mp2).collect::<Vec<_>>()),
            (AuFormat::Ac3, (0..4).map(ac3).collect::<Vec<_>>()),
        ] {
            let es = frames.concat();
            let cut = frames[0].len() + 500;
            let mut c = AuCutter::new(fmt);
            let mut aus = feed(&mut c, &pes(&es[..cut], Some(0)));
            aus.extend(feed(&mut c, &pes(&es[cut..], Some(5_000))));
            aus.extend(std::iter::from_fn(|| c.next(true)));
            assert_eq!(aus.len(), 4, "{fmt:?}");
            assert_eq!((aus[2].pes_start, aus[2].pts), (true, Some(5_000)), "{fmt:?}");
            assert!(!aus[1].pes_start, "{fmt:?}");
        }
    }
}
