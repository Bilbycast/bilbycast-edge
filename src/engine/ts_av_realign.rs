//! A/V **emission** realignment for the input transcode path.
//!
//! When the video ES is transcoded (decode → HW/SW encode), the video
//! pipeline buffers many frames — Intel VAAPI-decode → QSV-encode runs
//! ~10–20 frames deep — so the video PES for content-moment *T* leaves the
//! transcoder hundreds of milliseconds **after** the audio for *T*. The
//! audio path (MP2/AC-3 decode → re-encode) is shallow, so on the wire the
//! audio runs **ahead of its video** by the video pipeline depth (measured
//! ~0.5 s on Intel HW transcode of the Sky-Witness 1080i25 testbed file).
//!
//! The PTS values are correct, so a PTS-respecting software receiver (VLC)
//! re-aligns by buffering. But two problems remain for broadcast: (1) the
//! early audio sits in the receiver's small T-STD audio buffer, which a
//! strict hardware decoder can overflow; (2) the emission order is inverted
//! versus a normal broadcast mux, where video leads audio.
//!
//! `TsAvRealigner` holds audio-PID TS packets and releases them gated on the
//! **video PID's PTS progress**, so audio and video for the same content
//! leave together (with a small configurable audio lead matching a normal
//! mux). It does **not** touch PTS or CC — packets are emitted byte-identical
//! and in their original per-PID order, only **later** — so the audio stays
//! bit-exact and continuity-correct, just no longer early. Video, PCR, PSI,
//! and data PIDs pass through untouched and immediately.
//!
//! Safety: if the video PID stops advancing (encoder stall / video absent),
//! held audio is force-released after a bound so audio is never stalled or
//! dropped — degrading gracefully to the pre-fix (audio-early) behaviour
//! rather than going silent.

use std::collections::{HashSet, VecDeque};

use crate::engine::ts_parse::{
    extract_pcr, extract_pes_pts, parse_pat_programs, pts_delta_ms, ts_pid, ts_pusi,
};

const TS_PACKET: usize = 188;
const SYNC_BYTE: u8 = 0x47;

/// Target audio lead over video, in 90 kHz units. Audio for PTS `P` is
/// released once the video PID's latest PTS reaches `P − LEAD`, so the
/// audio leads its video by ~`LEAD` on the wire — a healthy broadcast mux
/// relationship (audio decoder is faster, so it's served slightly ahead)
/// instead of the ~0.5 s *early* the raw transcode produces. 150 ms.
const LEAD_90K: i64 = 150 * 90;

/// Hard cap on held audio packets before force-release (safety net so a
/// stalled / absent video PID can never grow the queue unbounded). ~6 s of
/// 192 kbps MP2-in-TS.
const MAX_HELD_PKTS: usize = 512;

/// Force-release the oldest held audio if it falls more than this far behind
/// the newest audio PTS seen — i.e. the video PID is stalled/absent. 3 s.
const MAX_HOLD_MS: i64 = 3_000;

struct HeldPkt {
    /// PTS (90 kHz) of the PES this packet belongs to — the gating key.
    pts_90k: u64,
    bytes: [u8; TS_PACKET],
}

/// Holds audio packets and releases them gated on video PTS progress.
/// Stateful across `process` calls (the hold queue persists).
pub struct TsAvRealigner {
    enabled: bool,
    pmt_pid: Option<u16>,
    /// The PCR-carrying PID — the transcoder forces PCR onto the video PID,
    /// so this *is* the video PID. Tracked rather than parsed from the PMT
    /// stream_type so it works regardless of codec.
    video_pid: Option<u16>,
    audio_pids: HashSet<u16>,
    latest_video_pts: u64,
    have_video_pts: bool,
    /// Newest audio PES PTS seen — drives the stall safety bound.
    latest_audio_pts: u64,
    have_audio_pts: bool,
    /// In-progress PES PTS per audio PID (set at PUSI, inherited by the
    /// continuation packets of the same PES so each held packet is tagged
    /// with its PES's PTS).
    cur_pes_pts: std::collections::HashMap<u16, u64>,
    pending: VecDeque<HeldPkt>,
}

impl Default for TsAvRealigner {
    fn default() -> Self {
        Self::new()
    }
}

impl TsAvRealigner {
    pub fn new() -> Self {
        Self {
            enabled: true,
            pmt_pid: None,
            video_pid: None,
            audio_pids: HashSet::with_capacity(4),
            latest_video_pts: 0,
            have_video_pts: false,
            latest_audio_pts: 0,
            have_audio_pts: false,
            cur_pes_pts: std::collections::HashMap::with_capacity(4),
            pending: VecDeque::with_capacity(MAX_HELD_PKTS),
        }
    }

    /// Process one TS chunk: video/PCR/PSI/data pass through immediately,
    /// audio is held and released gated on video PTS. Appends to `output`.
    pub fn process(&mut self, input: &[u8], output: &mut Vec<u8>) {
        if !self.enabled {
            output.extend_from_slice(input);
            return;
        }
        let mut i = 0;
        while i + TS_PACKET <= input.len() {
            let pkt = &input[i..i + TS_PACKET];
            i += TS_PACKET;
            if pkt[0] != SYNC_BYTE {
                // Not 188-aligned — pass the byte through and resync on the
                // next sync. Shouldn't happen on transcoder output.
                output.extend_from_slice(pkt);
                continue;
            }
            let pid = ts_pid(pkt);

            // ── PID discovery ──
            if pid == 0 && ts_pusi(pkt) {
                if let Some((_, pmt)) = parse_pat_programs(pkt).into_iter().next() {
                    self.pmt_pid = Some(pmt);
                }
            }
            if Some(pid) == self.pmt_pid {
                if let Ok(arr) = <&[u8; TS_PACKET]>::try_from(pkt) {
                    crate::engine::input_media_player::refresh_audio_pids_from_pmt(
                        arr,
                        &mut self.audio_pids,
                    );
                }
            }
            // The PCR PID is the video PID (transcoder forces PCR→video).
            if extract_pcr(pkt).is_some() {
                self.video_pid = Some(pid);
            }

            let is_video = Some(pid) == self.video_pid;
            let is_audio = self.audio_pids.contains(&pid);

            if is_video {
                if ts_pusi(pkt) {
                    if let Some(pts) = extract_pes_pts(pkt) {
                        self.latest_video_pts = pts;
                        self.have_video_pts = true;
                    }
                }
                output.extend_from_slice(pkt);
                // Drain INTERLEAVED right after each video frame — NOT batched
                // at chunk end. The HW encoder emits video in bursts (several
                // frames per process() call); batching the audio release to the
                // end of a burst bunches it after the burst's last PCR, so the
                // older audio in the batch lands behind that PCR ⇒ LATE at the
                // receiver (a strict T-STD decoder drops it). Draining per video
                // frame bounds each released batch to ~one frame and positions
                // each audio PES right after the video frame that gates it, so
                // wire_emit paces it at the correct wall-clock and audio stays
                // ~LEAD ahead of PCR. (Verified: sync-test went from −133 ms
                // audio-vs-PCR / "ever late" to a healthy positive lead.)
                self.drain(output, false);
            } else if is_audio {
                if ts_pusi(pkt) {
                    if let Some(pts) = extract_pes_pts(pkt) {
                        self.cur_pes_pts.insert(pid, pts);
                        self.latest_audio_pts = pts;
                        self.have_audio_pts = true;
                    }
                }
                let tag = self.cur_pes_pts.get(&pid).copied().unwrap_or(0);
                let mut b = [0u8; TS_PACKET];
                b.copy_from_slice(pkt);
                self.pending.push_back(HeldPkt { pts_90k: tag, bytes: b });
            } else {
                // PAT / PMT / other data / null — emit immediately.
                output.extend_from_slice(pkt);
            }
        }
        self.drain(output, false);
    }

    /// Release held audio whose PES PTS has been reached by the video PID
    /// (plus the safety force-release paths). When `flush_all`, drain
    /// everything (graceful shutdown / teardown).
    fn drain(&mut self, output: &mut Vec<u8>, flush_all: bool) {
        while let Some(front) = self.pending.front() {
            let reached = self.have_video_pts
                && pts_delta_ms(self.latest_video_pts, front.pts_90k) >= -(LEAD_90K / 90);
            let stalled_too_long = self.have_audio_pts
                && pts_delta_ms(self.latest_audio_pts, front.pts_90k) > MAX_HOLD_MS;
            let too_many = self.pending.len() > MAX_HELD_PKTS;
            if flush_all || reached || stalled_too_long || too_many {
                let p = self.pending.pop_front().unwrap();
                output.extend_from_slice(&p.bytes);
            } else {
                break;
            }
        }
    }

    /// Drain all held audio — call on flush / shutdown so no audio is lost.
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        self.drain(output, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal TS packet on `pid` with optional PTS (PUSI+PES) and
    // optional PCR. Not a full mux — just enough for the realigner's parsers.
    fn pkt(pid: u16, pusi: bool, pts: Option<u64>, pcr: Option<u64>) -> [u8; TS_PACKET] {
        let mut p = [0xFFu8; TS_PACKET];
        p[0] = SYNC_BYTE;
        p[1] = ((pusi as u8) << 6) | ((pid >> 8) as u8 & 0x1F);
        p[2] = (pid & 0xFF) as u8;
        let mut payload_start = 4;
        if let Some(pcrv) = pcr {
            // adaptation field with PCR
            p[3] = 0b0011_0000; // AF + payload, cc 0
            p[4] = 7; // af length
            p[5] = 0x10; // PCR_flag
            let base = pcrv / 300;
            let ext = pcrv % 300;
            p[6] = (base >> 25) as u8;
            p[7] = (base >> 17) as u8;
            p[8] = (base >> 9) as u8;
            p[9] = (base >> 1) as u8;
            p[10] = (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8 & 1);
            p[11] = (ext & 0xFF) as u8;
            payload_start = 12;
        } else {
            p[3] = 0b0001_0000; // payload only
        }
        if let Some(ptsv) = pts {
            // PES header with PTS (pts_dts_flags = 0b10)
            let b = payload_start;
            p[b] = 0x00;
            p[b + 1] = 0x00;
            p[b + 2] = 0x01;
            p[b + 3] = 0xC0; // audio-ish stream_id (not used by realigner)
            p[b + 4] = 0;
            p[b + 5] = 0;
            p[b + 6] = 0x80;
            p[b + 7] = 0x80; // PTS present
            p[b + 8] = 5;
            let v = ptsv;
            p[b + 9] = 0x21 | (((v >> 30) & 0x07) << 1) as u8;
            p[b + 10] = ((v >> 22) & 0xFF) as u8;
            p[b + 11] = 0x01 | (((v >> 15) & 0x7F) << 1) as u8;
            p[b + 12] = ((v >> 7) & 0xFF) as u8;
            p[b + 13] = 0x01 | (((v & 0x7F) << 1) as u8);
        }
        p
    }

    fn count_pid(buf: &[u8], pid: u16) -> usize {
        buf.chunks(TS_PACKET).filter(|c| c.len() == TS_PACKET && ts_pid(c) == pid).count()
    }

    #[test]
    fn disabled_is_passthrough() {
        let mut r = TsAvRealigner::new();
        r.enabled = false;
        let input = pkt(0x100, true, Some(9000), None);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input.to_vec());
    }

    #[test]
    fn audio_held_until_video_catches_up_then_released_in_order() {
        let mut r = TsAvRealigner::new();
        // Teach it the PIDs directly (skip PAT/PMT for the unit test).
        r.video_pid = Some(0x200);
        r.audio_pids.insert(0x201);

        // Audio PES well ahead of video: pts 90000 (1.0s). Video at pts 9000
        // (0.1s) — far behind. Audio must be HELD.
        let a1 = pkt(0x201, true, Some(90_000), None);
        let v1 = pkt(0x200, true, Some(9_000), None);
        let mut out = Vec::new();
        r.process(&v1, &mut out);
        r.process(&a1, &mut out);
        // video emitted, audio held (video pts 9000 << audio 90000 - lead).
        assert_eq!(count_pid(&out, 0x200), 1, "video passes through");
        assert_eq!(count_pid(&out, 0x201), 0, "early audio is held");

        // Advance video to pts 90000 — now audio (90000) is reached.
        let v2 = pkt(0x200, true, Some(90_000), None);
        out.clear();
        r.process(&v2, &mut out);
        assert_eq!(count_pid(&out, 0x200), 1, "video2 passes");
        assert_eq!(count_pid(&out, 0x201), 1, "audio released once video reaches it");
    }

    #[test]
    fn audio_force_released_when_video_absent() {
        let mut r = TsAvRealigner::new();
        r.video_pid = Some(0x200); // never any video packets
        r.audio_pids.insert(0x201);
        let mut out = Vec::new();
        // First audio establishes latest_audio_pts.
        r.process(&pkt(0x201, true, Some(90_000), None), &mut out);
        // Audio > 3s newer than the held one — held one must force-release.
        r.process(&pkt(0x201, true, Some(90_000 + 4 * 90_000), None), &mut out);
        assert!(
            count_pid(&out, 0x201) >= 1,
            "stalled-video safety must release audio, not stall it"
        );
    }

    #[test]
    fn audio_interleaved_within_a_video_burst_not_batched_at_end() {
        // Regression: a multi-frame video burst in ONE process() call must
        // interleave the eligible audio between the video frames (drain per
        // frame), not dump it all after the burst's last frame (which would
        // land older audio behind the last PCR == late).
        let mut r = TsAvRealigner::new();
        r.video_pid = Some(0x200);
        r.audio_pids.insert(0x201);
        // Pre-queue two audio PESes (held — no video yet).
        let mut out = Vec::new();
        r.process(&pkt(0x201, true, Some(90_000), None), &mut out); // audio A @1.0s
        r.process(&pkt(0x201, true, Some(90_000 + 3600), None), &mut out); // audio B @1.04s
        assert_eq!(count_pid(&out, 0x201), 0, "held until video");
        // Now a 2-frame video burst in ONE process() call.
        let mut burst = Vec::new();
        burst.extend_from_slice(&pkt(0x200, true, Some(90_000), None)); // video @1.0s
        burst.extend_from_slice(&pkt(0x200, true, Some(90_000 + 3600), None)); // video @1.04s
        out.clear();
        r.process(&burst, &mut out);
        // Both audio released; and audio A must appear BEFORE video frame B in
        // byte order (interleaved by the per-frame drain), not after it.
        let pids: Vec<u16> = out.chunks(TS_PACKET).map(ts_pid).collect();
        assert_eq!(pids.iter().filter(|&&p| p == 0x201).count(), 2, "both audio out");
        let first_audio = pids.iter().position(|&p| p == 0x201).unwrap();
        let last_video = pids.iter().rposition(|&p| p == 0x200).unwrap();
        assert!(first_audio < last_video,
            "audio must interleave within the burst (got order {:?})", pids);
    }

    #[test]
    fn flush_drains_all_held_audio() {
        let mut r = TsAvRealigner::new();
        r.video_pid = Some(0x200);
        r.audio_pids.insert(0x201);
        let mut out = Vec::new();
        r.process(&pkt(0x201, true, Some(90_000), None), &mut out);
        assert_eq!(count_pid(&out, 0x201), 0, "held with no video");
        r.flush(&mut out);
        assert_eq!(count_pid(&out, 0x201), 1, "flush drains held audio");
    }
}
