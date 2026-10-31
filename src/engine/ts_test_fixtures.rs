// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared PSI test fixtures (test builds only).
//!
//! The VH1 packets are real bytes from `testbed/test_ts/VH1.ts` (an ATSC /
//! DigiCipher capture): every PMT-PID packet carries a 24-byte short-form
//! 0xC0 section at the pointer target and the PMT right behind it at packet
//! offset 29. Every PMT parser that assumed "the pointer target is the PMT"
//! found nothing on this stream.

use super::ts_parse::{mpeg2_crc32, TS_PACKET_SIZE, TS_SYNC_BYTE};

fn hex_packet(hex: &str) -> [u8; TS_PACKET_SIZE] {
    let hex: String = hex.split_whitespace().collect();
    assert_eq!(hex.len(), TS_PACKET_SIZE * 2, "fixture must be one TS packet");
    let mut pkt = [0u8; TS_PACKET_SIZE];
    for (i, b) in pkt.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex");
    }
    pkt
}

/// VH1.ts packet #134 — PMT PID 0x0031, program 2010 (0x07DA), version 11.
/// Layout: `pointer 0 | 0xC0 section (24 B, SSI=0) | PMT at offset 29`.
/// PMT: PCR_PID 0x0E0F; program_info = CA (GI, 0x4749) + registration
/// "CUEI"; ES 0x02/0x0E0F (MPEG-2 video), 0x81/0x0E10 (AC-3, ISO 639
/// "eng"), 0x86/0x0E11 (SCTE-35), 0xC0/0x0E12..0x0E14 (private).
pub fn vh1_pmt_packet() -> [u8; TS_PACKET_SIZE] {
    hex_packet(concat!(
        "4740311500c000150007da00b600000000000001000000000042aa829b02b05607da",
        "d70000ee0ff00c09044749e10105044355454902ee0ff00081ee10f0060a04656e67",
        "0086ee11f000c0ee12f008050445545631a100c0ee13f009050445545631a20100c0",
        "ee14f008bf06496e766964699c7a7f3effffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffff",
    ))
}

/// VH1.ts packet #97 — the PAT: program 2010 → PMT PID 0x0031.
pub fn vh1_pat_packet() -> [u8; TS_PACKET_SIZE] {
    hex_packet(concat!(
        "4740001a0000b00d0000c1000007dae031266bf6a6ffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffff",
    ))
}

/// Offset of the PMT section inside [`vh1_pmt_packet`].
pub const VH1_PMT_OFFSET: usize = 29;
pub const VH1_PROGRAM: u16 = 2010;

/// Build a complete long-form PMT section (table_id .. CRC_32).
pub fn pmt_section(
    program_number: u16,
    version: u8,
    pcr_pid: u16,
    program_info: &[u8],
    es: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&program_number.to_be_bytes());
    body.push(0xC1 | ((version & 0x1F) << 1));
    body.push(0x00);
    body.push(0x00);
    body.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    body.push(pcr_pid as u8);
    body.push(0xF0 | ((program_info.len() >> 8) as u8 & 0x0F));
    body.push(program_info.len() as u8);
    body.extend_from_slice(program_info);
    for (st, pid, info) in es {
        body.push(*st);
        body.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        body.push(*pid as u8);
        body.push(0xF0 | ((info.len() >> 8) as u8 & 0x0F));
        body.push(info.len() as u8);
        body.extend_from_slice(info);
    }
    let section_length = body.len() + 4;
    let mut sec = vec![0x02, 0xB0 | ((section_length >> 8) as u8 & 0x0F), section_length as u8];
    sec.extend_from_slice(&body);
    let crc = mpeg2_crc32(&sec);
    sec.extend_from_slice(&crc.to_be_bytes());
    sec
}

/// Build a complete PAT section.
pub fn pat_section(programs: &[(u16, u16)], version: u8) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    let mut sec = vec![
        0x00,
        0xB0 | ((section_length >> 8) as u8 & 0x0F),
        section_length as u8,
        0x00,
        0x01,
        0xC1 | ((version & 0x1F) << 1),
        0x00,
        0x00,
    ];
    for (pn, pid) in programs {
        sec.extend_from_slice(&pn.to_be_bytes());
        sec.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        sec.push(*pid as u8);
    }
    let crc = mpeg2_crc32(&sec);
    sec.extend_from_slice(&crc.to_be_bytes());
    sec
}

/// Lay `sections` back to back on `pid` starting with pointer_field 0,
/// setting PUSI (with a pointer to the first new section) on every packet
/// in which a section starts, and 0xFF-stuffing the tail. CC counts up
/// from `cc_start`. Mirrors how a conforming muxer packs one PSI unit.
pub fn packetize_sections(pid: u16, sections: &[&[u8]], cc_start: u8) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut data = Vec::new();
    let mut starts = Vec::new();
    for s in sections {
        starts.push(data.len());
        data.extend_from_slice(s);
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut cc = cc_start & 0x0F;
    while pos < data.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[2] = pid as u8;
        pkt[3] = 0x10 | cc;
        let next_start = starts.iter().copied().find(|&s| s >= pos);
        let cap = TS_PACKET_SIZE - 4;
        match next_start {
            Some(s) if s + 1 < pos + cap => {
                pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
                pkt[4] = (s - pos) as u8;
                let n = (cap - 1).min(data.len() - pos);
                pkt[5..5 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            Some(s) if s + 1 == pos + cap => {
                pkt[1] = (pid >> 8) as u8 & 0x1F;
                let n = cap - 1;
                pkt[4..4 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
            _ => {
                pkt[1] = (pid >> 8) as u8 & 0x1F;
                let n = cap.min(data.len() - pos);
                pkt[4..4 + n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
            }
        }
        out.push(pkt);
        cc = (cc + 1) & 0x0F;
    }
    out
}

/// One-packet PAT for `programs` on PID 0 with CC `cc`.
pub fn pat_packet(programs: &[(u16, u16)], version: u8, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let sec = pat_section(programs, version);
    packetize_sections(0, &[&sec], cc)[0]
}

/// A PMT that spans two packets: 12 audio ES, each carrying an ISO 639
/// language descriptor and a stream_identifier, with an MP2 target at the
/// end so it lands in the SECOND packet. Returns `(section, target_pid)`.
pub fn two_packet_pmt(program_number: u16, version: u8) -> (Vec<u8>, u16) {
    let descs: Vec<Vec<u8>> = (0..12u8)
        .map(|i| vec![0x0A, 0x04, b'e', b'n', b'a' + i, 0x00, 0x52, 0x01, i])
        .collect();
    let mut es: Vec<(u8, u16, &[u8])> = Vec::new();
    es.push((0x1B, 0x0100, &[]));
    for (i, d) in descs.iter().enumerate() {
        // 11 teletext-shaped private ES first, the MP2 target last.
        let st = if i == 11 { 0x03 } else { 0x06 };
        es.push((st, 0x0200 + i as u16, d.as_slice()));
    }
    let sec = pmt_section(program_number, version, 0x0100, &[], &es);
    assert!(sec.len() > TS_PACKET_SIZE - 5, "fixture must span two packets");
    (sec, 0x020B)
}

/// Write a 5-byte PES timestamp (`marker` = 0x2 PTS-only, 0x3 PTS with DTS
/// following, 0x1 DTS).
fn put_ts(dst: &mut [u8], marker: u8, v: u64) {
    let v = v & 0x1_FFFF_FFFF;
    dst[0] = (marker << 4) | (((v >> 29) as u8) & 0x0E) | 0x01;
    dst[1] = (v >> 22) as u8;
    dst[2] = (((v >> 14) as u8) & 0xFE) | 0x01;
    dst[3] = (v >> 7) as u8;
    dst[4] = (((v << 1) as u8) & 0xFE) | 0x01;
}

/// A PUSI packet starting a PES on `pid` (payload only, CC `cc`) with
/// `stream_id`, a PTS and an optional DTS; the rest of the packet is 0xAA
/// ES bytes.
pub fn pes_start_packet(
    pid: u16,
    cc: u8,
    stream_id: u8,
    pts: u64,
    dts: Option<u64>,
) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xAAu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = pid as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4..8].copy_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    pkt[8] = 0x00;
    pkt[9] = 0x00;
    pkt[10] = 0x80;
    match dts {
        Some(d) => {
            pkt[11] = 0xC0;
            pkt[12] = 10;
            put_ts(&mut pkt[13..18], 0x3, pts);
            put_ts(&mut pkt[18..23], 0x1, d);
        }
        None => {
            pkt[11] = 0x80;
            pkt[12] = 5;
            put_ts(&mut pkt[13..18], 0x2, pts);
        }
    }
    pkt
}

/// A payload-only continuation packet on `pid` (0xAA bytes), CC `cc`.
pub fn payload_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xAAu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = (pid >> 8) as u8 & 0x1F;
    pkt[2] = pid as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt
}

/// The TS packets of one PES on `pid` carrying `es` with `pts`
/// (`stream_id`, PES_packet_length set, data_alignment_indicator 0), the
/// last one padded with adaptation-field stuffing; CC counted on from `cc`.
pub fn pes_packets(pid: u16, stream_id: u8, es: &[u8], pts: u64, cc: &mut u8) -> Vec<u8> {
    let mut pes = vec![0x00, 0x00, 0x01, stream_id];
    pes.extend_from_slice(&((8 + es.len()) as u16).to_be_bytes());
    pes.extend_from_slice(&[0x80, 0x80, 0x05]);
    let mut ts = [0u8; 5];
    put_ts(&mut ts, 0x2, pts);
    pes.extend_from_slice(&ts);
    pes.extend_from_slice(es);
    let mut out = Vec::new();
    for (i, chunk) in pes.chunks(184).enumerate() {
        let mut pkt = vec![
            TS_SYNC_BYTE,
            if i == 0 { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F),
            pid as u8,
        ];
        let stuffing = 184 - chunk.len();
        if stuffing == 0 {
            pkt.push(0x10 | (*cc & 0x0F));
        } else {
            pkt.push(0x30 | (*cc & 0x0F));
            pkt.push((stuffing - 1) as u8);
            if stuffing > 1 {
                pkt.push(0x00);
                pkt.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        pkt.extend_from_slice(chunk);
        *cc = (*cc + 1) & 0x0F;
        out.extend_from_slice(&pkt);
    }
    out
}

/// A one-program TS (PAT, PMT with H.264 on 0x100 and AAC ADTS on 0x101)
/// carrying the ADTS frames of `adts`, one PES per frame — enough for a
/// demuxer to lock the audio PID and cache its AAC config.
pub fn aac_program_ts(adts: &[u8]) -> Vec<u8> {
    let mut frames = Vec::new();
    let (mut off, mut pts) = (0usize, 90_000u64);
    while off + 7 <= adts.len() {
        let len = (((adts[off + 3] as usize) & 0x03) << 11)
            | ((adts[off + 4] as usize) << 3)
            | ((adts[off + 5] as usize) >> 5);
        if len < 7 || off + len > adts.len() {
            break;
        }
        frames.push((pts, &adts[off..off + len]));
        off += len;
        pts += 1920;
    }
    aac_program_ts_timed(&frames)
}

/// [`aac_program_ts`] with each ADTS frame at its own PTS: one PES per
/// `(pts, frame)`.
pub fn aac_program_ts_timed(frames: &[(u64, &[u8])]) -> Vec<u8> {
    let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
    let mut ts = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
    ts.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
    let mut cc = 0u8;
    for &(pts, frame) in frames {
        ts.extend(pes_packets(0x101, 0xC0, frame, pts, &mut cc));
    }
    ts
}

/// The whole ADTS frames of `adts`, split.
pub fn split_adts(adts: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut off = 0;
    while off + 7 <= adts.len() {
        let len = (usize::from(adts[off + 3] & 0x03) << 11) | (usize::from(adts[off + 4]) << 3) | (usize::from(adts[off + 5]) >> 5);
        if len < 7 || off + len > adts.len() {
            break;
        }
        frames.push(&adts[off..off + len]);
        off += len;
    }
    frames
}

/// The sine fixture's 25 frames twice (AAC-LC 48 kHz stereo, a 1 kHz tone
/// on the left), then `frames` AAC-LC frames at 44.1 kHz stereo (a 300 Hz
/// tone on the left, [`aac_lc_adts`]), each at its own PTS from 90 000: a
/// source whose AAC configuration changes mid-stream, as a switch to another
/// feed does. The PTS of every frame and the index of the first at 44.1 kHz.
#[cfg(feature = "fdk-aac")]
pub fn rate_switch_frames(frames: usize) -> (Vec<(u64, Vec<u8>)>, usize) {
    let sine = sine_aac_lc_frames();
    let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut pts = 90_000u64;
    for f in sine.iter().chain(sine.iter()) {
        out.push((pts, f.to_vec()));
        pts += 1_920;
    }
    let first = out.len();
    let other = aac_lc_adts(44_100, 2, frames);
    for (j, f) in split_adts(&other).into_iter().enumerate() {
        out.push((pts + j as u64 * 1024 * 90_000 / 44_100, f.to_vec()));
    }
    (out, first)
}

/// Goertzel power of `hz` in `samples` at `rate` — over the whole buffer, so
/// a long one tells close frequencies apart.
pub fn tone_power(samples: &[f32], hz: f64, rate: u32) -> f64 {
    let c = 2.0 * (2.0 * std::f64::consts::PI * hz / f64::from(rate)).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = f64::from(x) + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - c * s1 * s2
}

/// One second of a 1 kHz tone (and 440 Hz on the right) as HE-AAC ADTS
/// frames — v1 (SBR) or v2 (SBR + PS) at 48 kHz stereo — from the fdk
/// encoder. ADTS signals SBR implicitly: every header carries the AAC-LC
/// profile and the 24 kHz core rate, and v2's mono core.
#[cfg(feature = "fdk-aac")]
pub fn he_aac_adts(v2: bool) -> Vec<u8> {
    let mut enc = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
        profile: if v2 { aac_codec::AacProfile::HeAacV2 } else { aac_codec::AacProfile::HeAacV1 },
        sample_rate: 48_000,
        channels: 2,
        bitrate: if v2 { 32_000 } else { 64_000 },
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::Implicit,
        transport: aac_codec::TransportType::Adts,
    })
    .expect("fdk HE-AAC encoder");
    let n = enc.frame_size() as usize;
    let mut out = Vec::new();
    for k in 0..48_000 / n {
        let tone = |f: f32| -> Vec<f32> {
            (0..n)
                .map(|i| 0.3 * (2.0 * std::f32::consts::PI * f * (k * n + i) as f32 / 48_000.0).sin())
                .collect()
        };
        out.extend(enc.encode_frame(&[tone(1_000.0), tone(440.0)]).expect("encode").bytes);
    }
    out
}

/// A live programme for the transcode tests: MP2 48 kHz stereo on PID
/// 0x101 and its PCR on PID 0x100, which the PMT names a video PID and
/// which carries nothing else, the audio 100 ms ahead of the PCR. One chunk
/// per 40 ms of the source ([`Mp2Programme::chunk`]).
#[cfg(feature = "media-codecs")]
pub struct Mp2Programme {
    frames: Vec<Vec<u8>>,
    mux: crate::engine::rtmp::ts_mux::TsMuxer,
    next: usize,
}

#[cfg(feature = "media-codecs")]
impl Mp2Programme {
    pub const PCR_PID: u16 = 0x100;
    pub const AUDIO_PID: u16 = 0x101;
    /// The PCR of chunk 0, 27 MHz.
    pub const T0_27MHZ: u64 = 900_000_000;
    /// One MP2 frame (1 152 samples at 48 kHz), 90 kHz.
    pub const FRAME_90K: u64 = 2_160;
    /// One chunk of the source, 27 MHz.
    pub const CHUNK_27MHZ: u64 = 40 * 27_000;

    /// `seconds` of a tone, encoded.
    pub fn new(seconds: usize) -> Self {
        let mut enc = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
            codec: video_codec::AudioCodecType::Mp2,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 192,
        })
        .expect("MP2 encoder");
        let fs = enc.frame_size();
        let mut frames = Vec::new();
        for k in 0..seconds * 48_000 / fs {
            let pcm: Vec<f32> = (0..fs).map(|i| ((k * fs + i) as f32 * 0.0625).sin() * 0.25).collect();
            for f in enc.encode_frame(&[pcm.clone(), pcm]).expect("encode") {
                frames.push(f.data.to_vec());
            }
        }
        let mut mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
        mux.set_pids(None, Some(Self::PCR_PID), Some(Self::AUDIO_PID), Some(Self::PCR_PID));
        mux.set_has_audio(true);
        mux.set_audio_stream(0x03, None);
        Self { frames, mux, next: 0 }
    }

    /// The PTS of audio frame `n`, 90 kHz.
    pub fn frame_pts(n: usize) -> u64 {
        Self::T0_27MHZ / 300 + 9_000 + n as u64 * Self::FRAME_90K
    }

    /// The source's `k`th 40 ms: its PCR, then every frame due within
    /// 100 ms of it. Chunks go in increasing `k`; a `k` skipped is a gap in
    /// the source, its frames lost. `None` once the frames run out.
    pub fn chunk(&mut self, k: u64) -> Option<Vec<u8>> {
        let t = Self::T0_27MHZ + k * Self::CHUNK_27MHZ;
        while self.next < self.frames.len() && Self::frame_pts(self.next) < t / 300 {
            self.next += 1;
        }
        if self.next >= self.frames.len() {
            return None;
        }
        let mut c = super::ts_parse::pcr_only_packet(Self::PCR_PID, 0, t, false).to_vec();
        while self.next < self.frames.len() && Self::frame_pts(self.next) <= t / 300 + 9_000 {
            for p in self.mux.mux_audio_pre_adts(&self.frames[self.next], Self::frame_pts(self.next)) {
                c.extend_from_slice(&p);
            }
            self.next += 1;
        }
        Some(c)
    }
}

/// What a TS stream carries on [`Mp2Programme`]'s PIDs, in order: each PCR
/// (27 MHz) and each audio PES's PTS (90 kHz).
#[cfg(feature = "media-codecs")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgrammeEvent {
    Pcr(u64),
    AudioPts(u64),
}

/// The [`ProgrammeEvent`]s of `ts`, in stream order.
#[cfg(feature = "media-codecs")]
pub fn programme_events(ts: &[u8]) -> Vec<ProgrammeEvent> {
    use super::ts_parse::{extract_pcr, extract_pes_pts, ts_pid, ts_pusi};
    ts.chunks_exact(TS_PACKET_SIZE)
        .filter(|p| p[0] == TS_SYNC_BYTE)
        .flat_map(|p| {
            let pcr = (ts_pid(p) == Mp2Programme::PCR_PID).then(|| extract_pcr(p)).flatten();
            let pts = (ts_pid(p) == Mp2Programme::AUDIO_PID && ts_pusi(p))
                .then(|| extract_pes_pts(p))
                .flatten();
            pcr.map(ProgrammeEvent::Pcr).into_iter().chain(pts.map(ProgrammeEvent::AudioPts))
        })
        .collect()
}

/// What makes a stream on [`Mp2Programme`]'s PIDs one a receiver can
/// follow from its first byte: every PCR after `after_27mhz` (nothing of
/// an earlier stream in it), each 30–50 ms after the one before (no step
/// back, no jump), and every audio PES after the first PCR 0–1 s ahead of
/// the PCR before it. Panics naming the first that is not.
#[cfg(feature = "media-codecs")]
pub fn assert_one_followable_stream(events: &[ProgrammeEvent], after_27mhz: u64) {
    let mut last_pcr: Option<u64> = None;
    let mut pcrs = 0;
    let mut audio = 0;
    for (i, e) in events.iter().enumerate() {
        match *e {
            ProgrammeEvent::Pcr(v) => {
                assert!(v > after_27mhz, "event {i}: PCR {v} is from before {after_27mhz}: {events:?}");
                if let Some(prev) = last_pcr {
                    let step = v as i64 - prev as i64;
                    assert!(
                        (30 * 27_000..=50 * 27_000).contains(&step),
                        "event {i}: the PCR steps {:.1} ms: {events:?}",
                        step as f64 / 27_000.0
                    );
                }
                last_pcr = Some(v);
                pcrs += 1;
            }
            ProgrammeEvent::AudioPts(pts) => {
                let Some(pcr) = last_pcr else { continue };
                let lead = pts as i64 - (pcr / 300) as i64;
                assert!(
                    (0..=90_000).contains(&lead),
                    "event {i}: audio PTS {pts} leads its PCR by {:.1} ms: {events:?}",
                    lead as f64 / 90.0
                );
                audio += 1;
            }
        }
    }
    assert!(pcrs >= 10 && audio >= 10, "{pcrs} PCRs and {audio} audio PES: too little to judge");

/// The 25 frames of the AAC-LC 48 kHz stereo sine fixture.
pub fn sine_aac_lc_frames() -> Vec<&'static [u8]> {
    const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
    let mut out = Vec::new();
    let mut off = 0;
    while off + 7 <= ADTS.len() {
        let len = (usize::from(ADTS[off + 3] & 0x03) << 11) | (usize::from(ADTS[off + 4]) << 3) | (usize::from(ADTS[off + 5]) >> 5);
        out.push(&ADTS[off..off + len]);
        off += len;
    }
    out
}

/// `frames` (whole ADTS frames of a stereo stream, no CRC — the sine
/// fixture's) re-signalled the way ISDB-T 1seg signals its layout: every
/// header says channel_configuration 0, and only frame `first` and every
/// `every`th after it opens its raw_data_block with a program_config_element
/// (one front channel pair, tag 0 — the element the stereo frames carry).
/// The frames before `first` carry none, like the start of an NHK capture.
pub fn pce_signalled_adts(frames: &[&[u8]], first: usize, every: usize) -> Vec<u8> {
    use crate::engine::aac_config::{PceElement, PceLayout, adts_with_pce, raw_block_pce};
    let mut out = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        let carries = i >= first && (i - first).is_multiple_of(every);
        let pce = if carries {
            let (profile, sr_idx) = (frame[2] >> 6, (frame[2] >> 2) & 0x0F);
            raw_block_pce(
                profile,
                sr_idx,
                &PceLayout { front: &[PceElement { is_cpe: true, tag: 0 }], side: &[], back: &[], lfe: &[] },
            )
        } else {
            Vec::new()
        };
        out.extend(adts_with_pce(frame, &pce).expect("a whole ADTS frame"));
    }
    out
}

/// `frames` AAC-LC frames of `channels` channels at `sample_rate` as ADTS,
/// from the fdk encoder, a distinct tone on each channel — one encoder
/// configuration the sine fixture is not (mono, 5.1, another rate).
#[cfg(feature = "fdk-aac")]
pub fn aac_lc_adts(sample_rate: u32, channels: u8, frames: usize) -> Vec<u8> {
    let mut enc = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
        profile: aac_codec::AacProfile::AacLc,
        sample_rate,
        channels,
        bitrate: 64_000 * u32::from(channels),
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::default(),
        transport: aac_codec::TransportType::Adts,
    })
    .expect("fdk AAC-LC encoder");
    let n = enc.frame_size() as usize;
    let mut out = Vec::new();
    let mut k = 0;
    while split_adts(&out).len() < frames {
        let planar: Vec<Vec<f32>> = (0..channels)
            .map(|ch| {
                let f = 300.0 + 200.0 * f32::from(ch);
                (0..n)
                    .map(|i| 0.3 * (2.0 * std::f32::consts::PI * f * (k * n + i) as f32 / sample_rate as f32).sin())
                    .collect()
            })
            .collect();
        out.extend(enc.encode_frame(&planar).expect("encode").bytes);
        k += 1;
    }
    out
}
