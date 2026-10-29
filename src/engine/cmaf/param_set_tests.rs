// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The passthrough track and the parameter sets it carries, driven the way
//! `handle_video` drives them: each access unit through
//! [`passthrough_track_ready`] and then the segmenter, one at a time, in
//! decode order (bilbycast-edge#204).
//!
//! The slices here carry real slice headers as far as the PPS they name —
//! `first_mb_in_slice` / `slice_type` / `pic_parameter_set_id` for H.264,
//! `first_slice_segment_in_pic_flag` / `no_output_of_prior_pics_flag` /
//! `slice_pic_parameter_set_id` for HEVC — so a test can read back, from what
//! was published, which PPS each sample needs and whether the init it decodes
//! against holds it.

use super::codecs::BitReader;
use super::param_sets::tests::{Bits, h264_pps, h264_sps, hevc_pps, hevc_sps, hevc_vps, hx};
use super::param_sets::ParamSets;
use super::*;

fn config() -> CmafOutputConfig {
    serde_json::from_value(serde_json::json!({
        "id": "o",
        "name": "o",
        "ingest_url": "https://origin.invalid/live",
        "segment_duration_secs": 1.0,
    }))
    .unwrap()
}

/// 25 fps.
const FRAME_90K: u64 = 3_600;

fn h264_slice(idr: bool, pps_id: u32) -> Vec<u8> {
    let (header, slice_type) = if idr { (0x65, 7) } else { (0x41, 5) };
    Bits::new(&[header]).ue(0).ue(slice_type).ue(pps_id).put(0x5A, 8).done()
}

fn hevc_slice(nal_type: u8, pps_id: u32) -> Vec<u8> {
    let mut b = Bits::new(&[nal_type << 1, 0x01]);
    b.put(1, 1); // first_slice_segment_in_pic_flag
    if (16..=23).contains(&nal_type) {
        b.put(0, 1); // no_output_of_prior_pics_flag
    }
    b.ue(pps_id).put(0x5A, 8).done()
}

/// The PPS a slice names, read from its header; `None` for anything else.
fn slice_pps(codec: VideoCodec, nal: &[u8]) -> Option<u32> {
    match codec {
        VideoCodec::H264 => {
            if !matches!(nal[0] & 0x1F, 1 | 5) {
                return None;
            }
            let mut r = BitReader::new(&nal[1..]);
            r.ue()?;
            r.ue()?;
            r.ue()
        }
        VideoCodec::H265 => {
            let t = (nal[0] >> 1) & 0x3F;
            if t > 21 {
                return None;
            }
            let mut r = BitReader::new(&nal[2..]);
            r.bit()?;
            if (16..=23).contains(&t) {
                r.bit()?;
            }
            r.ue()
        }
    }
}

/// The PPS ids a track's init carries — read back from the `avcC` / `hvcC`
/// that `build_init_segment` writes, not from the track's own list.
fn init_pps_ids(track: &VideoTrack) -> Vec<u32> {
    let init = fmp4::build_init_segment(track, None);
    let (fourcc, codec) = match track.codec {
        VideoCodec::H264 => (b"avcC", VideoCodec::H264),
        VideoCodec::H265 => (b"hvcC", VideoCodec::H265),
    };
    let pos = init.windows(4).position(|w| w == fourcc).expect("config record");
    let size = u32::from_be_bytes(init[pos - 4..pos].try_into().unwrap()) as usize;
    let body = &init[pos + 4..pos - 4 + size];
    let read = |at: &mut usize| {
        let len = u16::from_be_bytes([body[*at], body[*at + 1]]) as usize;
        let nal = body[*at + 2..*at + 2 + len].to_vec();
        *at += 2 + len;
        nal
    };
    let mut pps = Vec::new();
    match codec {
        VideoCodec::H264 => {
            let mut at = 6;
            for _ in 0..(body[5] & 0x1F) {
                read(&mut at);
            }
            let n = body[at];
            at += 1;
            for _ in 0..n {
                pps.push(read(&mut at));
            }
        }
        VideoCodec::H265 => {
            let mut at = 23;
            for _ in 0..body[22] {
                let nal_type = body[at] & 0x3F;
                let n = u16::from_be_bytes([body[at + 1], body[at + 2]]);
                at += 3;
                for _ in 0..n {
                    let nal = read(&mut at);
                    if nal_type == 34 {
                        pps.push(nal);
                    }
                }
            }
        }
    }
    let skip = if codec == VideoCodec::H264 { 1 } else { 2 };
    pps.iter().map(|n| BitReader::new(&n[skip..]).ue().unwrap()).collect()
}

/// What a run of access units left behind.
#[derive(Default)]
struct Run {
    /// Every closed segment: its generation and the PPS each sample names.
    segments: Vec<(u32, Vec<u32>)>,
    /// The PPS ids of each generation's init, read back from the init the
    /// track writes as the generation begins.
    inits: Vec<(u32, Vec<u32>)>,
}

/// One output, fed access units `(nalus, is_keyframe)` at 25 fps.
struct Out {
    state: CmafState,
    config: CmafOutputConfig,
    codec: VideoCodec,
    frame: u64,
    run: Run,
}

impl Out {
    fn new(codec: VideoCodec) -> Self {
        Self { state: CmafState::new(), config: config(), codec, frame: 0, run: Run::default() }
    }

    fn feed(&mut self, nalus: Vec<Vec<u8>>, is_keyframe: bool) {
        let pts = 900_000 + self.frame * FRAME_90K;
        self.frame += 1;
        let ready =
            passthrough_track_ready(&mut self.state, self.codec, &nalus, is_keyframe, &self.config);
        if !ready {
            return;
        }
        let seg = self.state.video_seg.as_mut().unwrap();
        let before = seg.generation();
        let out = seg.push(&nalus, Some(pts), 0, is_keyframe);
        if let (Some(done), Some((_, _, samples))) =
            (out.completed_video, out.completed_video_samples)
        {
            let names: Vec<u32> = samples
                .iter()
                .flat_map(|s| length_prefixed(&s.data))
                .filter_map(|n| slice_pps(self.codec, &n))
                .collect();
            self.run.segments.push((done.generation, names));
        }
        let seg = self.state.video_seg.as_ref().unwrap();
        if self.run.inits.is_empty() || seg.generation() != before {
            self.run.inits.push((seg.generation(), init_pps_ids(&seg.track)));
        }
    }

    fn generation(&self) -> u32 {
        self.state.video_seg.as_ref().map_or(0, VideoSegmenter::generation)
    }

    fn track(&self) -> &VideoTrack {
        &self.state.video_seg.as_ref().unwrap().track
    }

    /// Every sample of every closed segment names a PPS its generation's
    /// init carries.
    fn assert_every_sample_decodable(&self) {
        assert!(!self.run.segments.is_empty(), "nothing closed");
        for (generation, names) in &self.run.segments {
            let init = &self.run.inits.iter().rev().find(|(g, _)| g == generation).unwrap().1;
            for id in names {
                assert!(
                    init.contains(id),
                    "generation {generation}: a slice names PPS {id}, init has {init:?}"
                );
            }
        }
    }
}

fn length_prefixed(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        out.push(data[at + 4..at + 4 + len].to_vec());
        at += 4 + len;
    }
    out
}

fn h770() -> (Vec<u8>, Vec<u8>, [Vec<u8>; 3]) {
    let vps = hx("40010c01ffff02200000030060000003000003007b0c190240");
    let sps = hx(concat!(
        "42010102200000030060000003000003007ba003c080221f6d8d0c199246f843fff91421ff",
        "fc8a10fffffffffffffffe45087fffffffffffffff22843fffffffffffffffc8a10fffffff",
        "fffffffff22843fffffffffffffffe10ffffffffffffffff5780a1010101eff00000030010",
        "000003032080",
    ));
    let pps = [
        hx("4401c02524f87640"),
        hx("44015009693e1d90"),
        hx(concat!(
            "44017009693e1f061838fff22830c1c7ff914187fc1c7ffffffffffffc8a0c3fe0e3ffffff",
            "ffffffe45061ff838fffffffffffff914187fe0e3ffffffffffffe45061ff838fffffffffffff",
            "c187fe0e3fffffffffffff9",
        )),
    ];
    (vps, sps, pps)
}

/// The reported capture, as it is: every IRAP carries VPS0, SPS0, PPS0, PPS1,
/// PPS2 in that order (real bytes from 770_H_8000_2.ts), and its slices name
/// PPS0 and PPS1, never PPS2. The init used to carry PPS2 alone — the last
/// one in — and every frame failed with "PPS id out of range".
#[test]
fn a_multi_pps_hevc_source_builds_one_init_carrying_every_pps() {
    let (vps, sps, pps) = h770();
    let mut out = Out::new(VideoCodec::H265);
    for gop in 0..5 {
        for i in 0..25u32 {
            if i == 0 {
                let mut au = vec![vps.clone(), sps.clone()];
                au.extend(pps.iter().cloned());
                au.push(hevc_slice(19, 0));
                out.feed(au, true);
            } else {
                out.feed(vec![hevc_slice(1, (i + gop) % 2)], false);
            }
        }
    }
    let mut last = vec![vps.clone(), sps.clone()];
    last.extend(pps.iter().cloned());
    last.push(hevc_slice(19, 0));
    out.feed(last, true);
    assert_eq!(init_pps_ids(out.track()), vec![0, 1, 2]);
    let carried: Vec<&Vec<u8>> = out.track().param_sets.pps.iter().map(|p| &p.nal).collect();
    assert_eq!(carried, pps.iter().collect::<Vec<_>>());
    assert_eq!(out.generation(), 0, "a source that never changes never rotates");
    assert_eq!(out.run.segments.len(), 5);
    let named: std::collections::BTreeSet<u32> =
        out.run.segments.iter().flat_map(|(_, n)| n.iter().copied()).collect();
    assert_eq!(named, [0, 1].into(), "fixture: the slices name PPS0 and PPS1");
    out.assert_every_sample_decodable();
    assert_eq!(out.track().height, 544, "dimensions from the SPS");
}

/// The same access units through the real path in front of it: muxed to TS
/// and demuxed, as the output receives them, several frames to a `demux`
/// call. On the code before this fix the init carried one PPS — PPS2, the
/// last at each IRAP and the one no slice names (a check written against
/// 95f1e75 failed exactly so).
#[test]
fn the_770h_access_units_through_the_demuxer() {
    use crate::engine::rtmp::ts_mux::TsMuxer;
    let (vps, sps, pps) = h770();
    let mut mux = TsMuxer::new();
    mux.set_video_stream_type(0x24);
    let mut demuxer = TsDemuxer::new(None);
    let mut out = Out::new(VideoCodec::H265);
    let mut batch = Vec::new();
    for frame in 0..60u64 {
        let key = frame % 25 == 0;
        let nals = if key {
            let mut au = vec![vps.clone(), sps.clone()];
            au.extend(pps.iter().cloned());
            au.push(hevc_slice(19, 0));
            au
        } else {
            vec![hevc_slice(1, (frame % 2) as u32)]
        };
        let mut annex_b = Vec::new();
        for n in &nals {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(n);
        }
        let pts = 900_000 + frame * FRAME_90K;
        batch.extend(mux.mux_video(&annex_b, pts, pts, key).iter().flat_map(|p| p.iter().copied()));
        // Three frames to a datagram, so a batch holds more than one IRAP's
        // worth of look-ahead when it straddles one.
        if frame % 3 == 2 {
            for f in demuxer.demux(&std::mem::take(&mut batch)) {
                if let DemuxedFrame::H265 { nalus, is_keyframe, .. } = f {
                    out.feed(nalus, is_keyframe);
                }
            }
        }
    }
    assert_eq!(init_pps_ids(out.track()), vec![0, 1, 2]);
    assert_eq!(out.generation(), 0);
    out.assert_every_sample_decodable();
}

/// The same for H.264: an SPS and three PPS at every IDR, slices naming two.
#[test]
fn a_multi_pps_h264_source_builds_one_init_carrying_every_pps() {
    let mut out = Out::new(VideoCodec::H264);
    for gop in 0..4u32 {
        for i in 0..25u32 {
            if i == 0 {
                let au = vec![
                    h264_sps(0, 1),
                    h264_pps(0, 1),
                    h264_pps(1, 1),
                    h264_pps(2, 1),
                    h264_slice(true, 1),
                ];
                out.feed(au, true);
            } else {
                out.feed(vec![h264_slice(false, (i + gop) % 2)], false);
            }
        }
    }
    let last = vec![h264_sps(0, 1), h264_pps(0, 1), h264_pps(1, 1), h264_pps(2, 1), h264_slice(true, 0)];
    out.feed(last, true);
    assert_eq!(init_pps_ids(out.track()), vec![0, 1, 2]);
    assert_eq!(out.generation(), 0);
    out.assert_every_sample_decodable();
}

/// A random-access point that leaves out a PPS the track carries is not a
/// change: the track keeps it, and nothing rotates.
#[test]
fn a_rap_dropping_an_unused_pps_does_not_rotate() {
    let (vps, sps, pps) = h770();
    let mut out = Out::new(VideoCodec::H265);
    for gop in 0..4 {
        let mut au = vec![vps.clone(), sps.clone(), pps[0].clone(), pps[1].clone()];
        if gop == 0 {
            au.push(pps[2].clone());
        }
        au.push(hevc_slice(19, 0));
        out.feed(au, true);
        for i in 1..25u32 {
            out.feed(vec![hevc_slice(1, i % 2)], false);
        }
    }
    out.feed(vec![hevc_slice(19, 0)], true);
    assert_eq!(out.generation(), 0);
    assert_eq!(init_pps_ids(out.track()), vec![0, 1, 2]);
    out.assert_every_sample_decodable();
}

/// A source switch on one PID with no PMT change in between: the new
/// source's first IDR carries its own sets, which replace the old ones of
/// each kind — one rotation, and the new init holds the new source's sets
/// only, not the old source's extra PPS.
#[test]
fn a_source_switch_rotates_once_and_drops_the_old_extra_pps() {
    let mut out = Out::new(VideoCodec::H264);
    let a = [h264_sps(0, 0xA1), h264_pps(0, 0xA1), h264_pps(1, 0xA1)];
    let b = [h264_sps(0, 0xB2), h264_pps(0, 0xB2)];
    for _ in 0..3 {
        out.feed(vec![a[0].clone(), a[1].clone(), a[2].clone(), h264_slice(true, 0)], true);
        for i in 1..25 {
            out.feed(vec![h264_slice(false, i % 2)], false);
        }
    }
    assert_eq!(out.generation(), 0);
    for _ in 0..3 {
        out.feed(vec![b[0].clone(), b[1].clone(), h264_slice(true, 0)], true);
        for _ in 1..25 {
            out.feed(vec![h264_slice(false, 0)], false);
        }
    }
    out.feed(vec![b[0].clone(), b[1].clone(), h264_slice(true, 0)], true);
    assert_eq!(out.generation(), 1, "exactly one rotation");
    let sets = &out.track().param_sets;
    assert_eq!(sets.sps.iter().map(|s| &s.nal).collect::<Vec<_>>(), vec![&b[0]]);
    assert_eq!(sets.pps.iter().map(|s| &s.nal).collect::<Vec<_>>(), vec![&b[1]]);
    out.assert_every_sample_decodable();
}

/// A stream discontinuity — the PMT version bump every operator switch
/// makes — through the output's own frame handler.
async fn discontinuity(out: &mut Out) {
    let mut demuxer = TsDemuxer::new(None);
    let stats = OutputStatsAccumulator::new("o".into(), "o".into(), "cmaf".into());
    let (events, _rx) = crate::manager::events::event_channel();
    let config = out.config.clone();
    handle_frame(
        DemuxedFrame::Discontinuity,
        &mut out.state,
        &mut demuxer,
        &config,
        "",
        "",
        "",
        "",
        "",
        false,
        false,
        &stats,
        &events,
        "flow",
        0,
    )
    .await;
}

/// The same switch where the new source sends its sets ahead of its IDR,
/// in an access unit of their own, and the IDR carries none — which a
/// random-access point alone cannot clean up: merged by id, the old source's
/// PPS1 would stay in the table and the new init. The demuxer's discontinuity
/// (the PMT version bump every operator switch makes) starts the table again.
#[tokio::test]
async fn a_discontinuity_starts_the_table_again() {
    let mut out = Out::new(VideoCodec::H264);
    let a = [h264_sps(0, 0xA1), h264_pps(0, 0xA1), h264_pps(1, 0xA1)];
    let b = [h264_sps(0, 0xB2), h264_pps(0, 0xB2)];
    for _ in 0..2 {
        out.feed(vec![a[0].clone(), a[1].clone(), a[2].clone(), h264_slice(true, 0)], true);
        for i in 1..25 {
            out.feed(vec![h264_slice(false, i % 2)], false);
        }
    }

    // The switch, through the output's own frame handler.
    discontinuity(&mut out).await;
    assert!(out.state.source_params.sets(VideoCodec::H264).is_none(), "the table starts again");

    // Until the new source's sets arrive nothing changes, keyframe or not.
    out.feed(vec![h264_slice(true, 0)], true);
    out.feed(vec![h264_slice(false, 0)], false);
    assert_eq!(out.generation(), 0);
    assert!(!out.state.video_seg.as_ref().unwrap().rotation_pending());

    // The sets ahead of the IDR, then the IDR carrying none: one rotation.
    out.feed(vec![b[0].clone(), b[1].clone()], false);
    assert!(!out.state.video_seg.as_ref().unwrap().rotation_pending(), "not mid-GOP");
    out.feed(vec![h264_slice(true, 0)], true);
    for _ in 1..25 {
        out.feed(vec![h264_slice(false, 0)], false);
    }
    out.feed(vec![h264_slice(true, 0)], true);
    assert_eq!(out.generation(), 1, "exactly one rotation");
    let sets = &out.track().param_sets;
    assert_eq!(sets.sps.iter().map(|s| &s.nal).collect::<Vec<_>>(), vec![&b[0]]);
    let pps: Vec<&Vec<u8>> = sets.pps.iter().map(|s| &s.nal).collect();
    assert_eq!(pps, vec![&b[1]], "no PPS1 of the old source");
}

/// What the reset does not cover. On a switch that keeps the video PID and
/// codec, the old source's last picture is still in the demuxer's PES
/// assembler when the PMT bump is seen, and comes out after the
/// discontinuity — completed by the new source's first PES — so its
/// parameter sets land in the fresh table. A new source whose random-access
/// point re-sends each kind replaces them; one that sends its sets only
/// outside a random-access point merges them in beside the old picture's,
/// and the old source's extra PPS reaches the next init, as it would have
/// with no reset (docs/cmaf.md, Every parameter set, by id).
#[tokio::test]
async fn the_old_sources_last_picture_after_a_discontinuity() {
    let a = [h264_sps(0, 0xA1), h264_pps(0, 0xA1), h264_pps(1, 0xA1)];
    let b = [h264_sps(0, 0xB2), h264_pps(0, 0xB2)];
    for sets_at_the_idr in [true, false] {
        let mut out = Out::new(VideoCodec::H264);
        for _ in 0..2 {
            out.feed(vec![a[0].clone(), a[1].clone(), a[2].clone(), h264_slice(true, 0)], true);
            for i in 1..25 {
                out.feed(vec![h264_slice(false, i % 2)], false);
            }
        }
        discontinuity(&mut out).await;
        // The old source's last picture: it re-sent its sets mid-GOP.
        out.feed(vec![a[0].clone(), a[1].clone(), a[2].clone(), h264_slice(false, 1)], false);
        let table = out.state.source_params.sets(VideoCodec::H264).unwrap();
        assert_eq!(table.pps.len(), 2, "the old picture's sets are in the fresh table");
        if sets_at_the_idr {
            out.feed(vec![b[0].clone(), b[1].clone(), h264_slice(true, 0)], true);
        } else {
            out.feed(vec![b[0].clone(), b[1].clone()], false);
            out.feed(vec![h264_slice(true, 0)], true);
        }
        for _ in 1..25 {
            out.feed(vec![h264_slice(false, 0)], false);
        }
        out.feed(vec![h264_slice(true, 0)], true);
        assert_eq!(out.generation(), 1, "exactly one rotation");
        let sets = &out.track().param_sets;
        assert_eq!(sets.sps.iter().map(|s| &s.nal).collect::<Vec<_>>(), vec![&b[0]]);
        let pps: Vec<&Vec<u8>> = sets.pps.iter().map(|s| &s.nal).collect();
        if sets_at_the_idr {
            assert_eq!(pps, vec![&b[1]], "the IDR's PPS replaced the old picture's");
        } else {
            assert_eq!(pps, vec![&b[1], &a[2]], "the old source's PPS1 stays beside the new PPS0");
        }
        out.assert_every_sample_decodable();
    }
}

/// A PPS re-sent with new content between IDRs takes effect at the next
/// IDR that carries it, as it always has: nothing is cut mid-GOP.
#[test]
fn a_changed_pps_rotates_at_the_next_idr_not_mid_gop() {
    let mut out = Out::new(VideoCodec::H264);
    out.feed(vec![h264_sps(0, 1), h264_pps(0, 1), h264_slice(true, 0)], true);
    for _ in 1..10 {
        out.feed(vec![h264_slice(false, 0)], false);
    }
    out.feed(vec![h264_pps(0, 2), h264_slice(false, 0)], false);
    for _ in 11..25 {
        out.feed(vec![h264_slice(false, 0)], false);
    }
    assert_eq!(out.generation(), 0);
    assert!(!out.state.video_seg.as_ref().unwrap().rotation_pending(), "nothing mid-GOP");
    out.feed(vec![h264_sps(0, 1), h264_pps(0, 2), h264_slice(true, 0)], true);
    assert_eq!(out.generation(), 1, "rotated at the IDR");
    assert_eq!(out.run.segments.last().unwrap().0, 0, "the segment it closed is the old generation");
    assert_eq!(out.track().param_sets.pps[0].nal, h264_pps(0, 2));
}

/// A source whose PPS0 churns (sky-sports-arena: 104 distinct PPS0 contents
/// in its first 80 MiB, 23 of them on its 44 IDRs, re-sent between IDRs too). A random-access
/// point defines the PPS it carries, so the track follows the IDRs' own
/// PPS0 — a rotation exactly where an IDR's differs from the track's — and
/// never one for a re-send between them.
#[test]
fn same_id_churn_rotates_only_on_the_raps_own_change() {
    let mut out = Out::new(VideoCodec::H264);
    let rap_tags = [1u8, 1, 2, 2, 3, 1];
    let mut want_generation = 0;
    let mut track_tag = rap_tags[0];
    for (gop, &tag) in rap_tags.iter().enumerate() {
        if tag != track_tag {
            want_generation += 1;
            track_tag = tag;
        }
        out.feed(vec![h264_sps(0, 1), h264_pps(0, tag), h264_slice(true, 0)], true);
        assert_eq!(out.generation(), want_generation, "gop {gop}");
        assert_eq!(out.track().param_sets.pps[0].nal, h264_pps(0, tag), "gop {gop}");
        for i in 1..25u8 {
            // Re-sends of other contents between the IDRs.
            let au = if i % 6 == 0 {
                vec![h264_pps(0, 0x40 + i), h264_slice(false, 0)]
            } else {
                vec![h264_slice(false, 0)]
            };
            out.feed(au, false);
        }
        assert_eq!(out.generation(), want_generation, "gop {gop}: not between IDRs");
    }
    assert_eq!(want_generation, 3);
}

/// The track is built from the parameter sets sent up to the picture being
/// handled — never from a later picture's. The demuxer's cache was read after
/// the whole datagram had been demuxed, so the first IDR's track could take
/// a PPS from a picture behind it in the same batch. Here the IDR (PPS0
/// first content) and the P picture after it (PPS0 re-sent with other
/// content) reach the output in one datagram, through the real muxer and
/// demuxer: on the code before this fix the track took the P picture's PPS0.
#[test]
fn parameter_sets_are_read_per_frame_not_per_batch() {
    use crate::engine::rtmp::ts_mux::TsMuxer;
    let aus = [
        (vec![h264_sps(0, 1), h264_pps(0, 1), h264_slice(true, 0)], true),
        (vec![h264_pps(0, 2), h264_slice(false, 0)], false),
        (vec![h264_slice(false, 0)], false),
        (vec![h264_slice(false, 0)], false),
    ];
    let mut mux = TsMuxer::new();
    let mut batch = Vec::new();
    for (frame, (nals, key)) in aus.iter().enumerate() {
        let mut annex_b = Vec::new();
        for n in nals {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(n);
        }
        let pts = 900_000 + frame as u64 * FRAME_90K;
        batch.extend(mux.mux_video(&annex_b, pts, pts, *key).iter().flat_map(|p| p.iter().copied()));
    }
    let mut demuxer = TsDemuxer::new(None);
    let frames: Vec<_> = demuxer
        .demux(&batch)
        .into_iter()
        .filter_map(|f| match f {
            DemuxedFrame::H264 { nalus, is_keyframe, .. } => Some((nalus, is_keyframe)),
            _ => None,
        })
        .collect();
    assert!(frames.len() >= 2, "fixture: the IDR and the P picture in one batch");
    let mut out = Out::new(VideoCodec::H264);
    for (i, (nalus, is_keyframe)) in frames.into_iter().enumerate() {
        out.feed(nalus, is_keyframe);
        if i == 0 {
            assert_eq!(out.track().param_sets.pps[0].nal, h264_pps(0, 1), "the IDR's own PPS0");
        }
    }
    assert_eq!(out.track().param_sets.pps[0].nal, h264_pps(0, 1), "not the P picture's");
    assert!(!out.state.video_seg.as_ref().unwrap().rotation_pending(), "nothing mid-GOP");
}

/// With more than one SPS, the one the track's dimensions, configuration
/// header and manifest codec string are read from is the lowest id — not
/// whichever arrived last.
#[test]
fn the_primary_sps_is_the_lowest_id() {
    let mut out = Out::new(VideoCodec::H264);
    let high = Bits::new(&[0x67, 0x4D, 0x40, 0x1F]).ue(1).put(0x11, 8).done();
    let low = Bits::new(&[0x67, 0x64, 0x00, 0x28]).ue(0).put(0x22, 8).done();
    out.feed(vec![high.clone(), low.clone(), h264_pps(0, 1), h264_slice(true, 0)], true);
    assert_eq!(out.track().sps(), &low[..]);
    let init = fmp4::build_init_segment(out.track(), None);
    let pos = init.windows(4).position(|w| w == b"avcC").unwrap();
    assert_eq!(&init[pos + 4..pos + 8], &[1, 0x64, 0x00, 0x28], "avcC header from SPS0");
    assert_eq!(init[pos + 9], 0xE2, "both SPS carried");
}

/// The first track waits for a whole set: an SPS and PPS for H.264, and a
/// VPS too for HEVC.
#[test]
fn the_track_waits_for_a_complete_set() {
    let mut out = Out::new(VideoCodec::H265);
    out.feed(vec![hevc_sps(0, 1), hevc_pps(0, 1), hevc_slice(19, 0)], true);
    assert!(out.state.video_seg.is_none(), "no VPS yet");
    out.feed(vec![hevc_vps(0, 1), hevc_sps(0, 1), hevc_pps(0, 1), hevc_slice(19, 0)], true);
    assert!(out.state.video_seg.is_some());
    assert_eq!(out.track().param_sets, ParamSets {
        vps: vec![param_sets::ParamSet { id: 0, nal: hevc_vps(0, 1) }],
        sps: vec![param_sets::ParamSet { id: 0, nal: hevc_sps(0, 1) }],
        pps: vec![param_sets::ParamSet { id: 0, nal: hevc_pps(0, 1) }],
    });
}

/// The failure, decoded. x264 codes a stream whose slices name PPS0; each
/// IDR then gets a second PPS, the same one re-coded under id 1 — what a
/// cache holding the last PPS seen ends every IDR on. The init this output
/// writes carries both, and every picture decodes against it; an init holding
/// only the last PPS (what the track used to be built from) decodes none.
#[cfg(feature = "video-encoder-x264")]
mod decode {
    use super::*;
    use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoPreset};

    /// `pps` re-coded with `pic_parameter_set_id` = `id`: its RBSP read
    /// past the id, every remaining bit up to and including the stop bit
    /// written after the new id, re-aligned, and emulation prevention put
    /// back.
    fn recode_pps_id(pps: &[u8], id: u32) -> Vec<u8> {
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for &b in &pps[1..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        let bits: Vec<bool> =
            rbsp.iter().flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1 == 1)).collect();
        assert!(bits[0], "fixture: x264 numbers its PPS 0, coded as the single bit 1");
        let stop = bits.iter().rposition(|&b| b).unwrap();
        let code = u64::from(id) + 1;
        let len = 64 - code.leading_zeros();
        let mut out_bits: Vec<bool> = std::iter::repeat_n(false, len as usize - 1)
            .chain((0..len).rev().map(|i| (code >> i) & 1 == 1))
            .collect();
        out_bits.extend_from_slice(&bits[1..=stop]);
        while !out_bits.len().is_multiple_of(8) {
            out_bits.push(false);
        }
        let mut out = vec![pps[0]];
        let mut zeros = 0;
        for chunk in out_bits.chunks(8) {
            let b = chunk.iter().fold(0u8, |acc, &bit| (acc << 1) | u8::from(bit));
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    fn encode(frames: usize) -> Vec<Vec<Vec<u8>>> {
        let (w, h) = (320usize, 240usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 10,
            max_b_frames: 0,
            tune: String::new(),
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..frames {
            let y: Vec<u8> =
                (0..w * h).map(|k| (((k % w + 3 * i) * 7 + (k / w) * 13) % 200) as u8 + 20).collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        aus.iter().map(|f| crate::engine::ts_demux::split_annex_b_nalus(&f.data)).collect()
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    /// Decode every sample, the init's parameter sets in front of the first.
    fn decoded(param_sets: &[&[u8]], samples: &[Vec<Vec<u8>>]) -> usize {
        let mut dec = video_engine::VideoDecoder::open(video_codec::VideoCodec::H264).unwrap();
        let mut frames = 0;
        let mut take = |dec: &mut video_engine::VideoDecoder| {
            while dec.receive_frame().is_ok() {
                frames += 1;
            }
        };
        for (i, sample) in samples.iter().enumerate() {
            let mut nals: Vec<&[u8]> = if i == 0 { param_sets.to_vec() } else { Vec::new() };
            nals.extend(sample.iter().map(Vec::as_slice));
            let _ = dec.send_packet(&annex_b(&nals));
            take(&mut dec);
        }
        let _ = dec.send_flush();
        take(&mut dec);
        frames
    }

    #[test]
    fn an_init_missing_a_referenced_pps_is_the_failure_this_fixes() {
        let mut aus = encode(30);
        assert_eq!(aus.len(), 30);
        let mut injected = None;
        for au in &mut aus {
            if let Some(at) = au.iter().position(|n| n[0] & 0x1F == 8) {
                let pps1 = recode_pps_id(&au[at], 1);
                assert_eq!(BitReader::new(&pps1[1..]).ue(), Some(1));
                au.insert(at + 1, pps1.clone());
                injected = Some((au[at].clone(), pps1));
            }
        }
        let (pps0, pps1) = injected.expect("fixture: x264 sends its PPS in-band");

        let mut out = Out::new(VideoCodec::H264);
        for au in &aus {
            let idr = au.iter().any(|n| n[0] & 0x1F == 5);
            out.feed(au.clone(), idr);
        }
        let sets = out.track().param_sets.clone();
        let carried: Vec<Vec<u8>> = sets.pps.iter().map(|p| p.nal.clone()).collect();
        assert_eq!(carried, vec![pps0.clone(), pps1.clone()]);
        assert_eq!(out.generation(), 0);

        // What the segments carry: the source's NALs without parameter sets.
        let samples: Vec<Vec<Vec<u8>>> = aus.iter().map(|au| nalu::filter_frame_nalus_h264(au)).collect();
        let sps = &sets.sps[0].nal;
        assert_eq!(
            decoded(&[sps, &pps0, &pps1], &samples),
            30,
            "every picture decodes against the full init"
        );
        assert_eq!(decoded(&[sps, &pps1], &samples), 0, "an init with only the last PPS decodes nothing");
    }
}
