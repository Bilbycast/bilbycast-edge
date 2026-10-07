// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opus-in-MPEG-TS passthrough for the WebRTC outputs.
//!
//! A source that already carries Opus — a WHIP input, ffmpeg's
//! `-c:a libopus -f mpegts`, another edge's Opus-in-TS — goes out on the
//! WebRTC audio track as the packets it carries, with no decode and no
//! re-encode. Each PES is split into its access units by the Opus-in-TS
//! walker the decoders use ([`split_opus_frames`]) and each packet is
//! stamped on the 48 kHz RTP clock Opus always runs at (RFC 7587 §4.1): the
//! PES's PTS for its first packet, and for each one after it the samples of
//! the packets before it, read from their TOC bytes (RFC 6716 §3.1). A PES
//! carries several packets (ffmpeg packs up to 120 ms), so stamping them
//! all with the PES's PTS would hand the receiver one instant for all of
//! them.
//!
//! The PES PTS is the time of its first packet's first sample before any
//! `start_trim` — what ffmpeg's `mpegts` muxer writes: the untrimmed packets
//! of its first PES, whose first carries the encoder pre-skip as
//! `start_trim`, end exactly where the next PES's PTS begins. RTP cannot
//! signal a trim, so the trims are not applied.
//!
//! RTP carries one Opus stream, mono or stereo. Multistream Opus — more
//! than two channels, or dual mono — is a concatenation of streams no
//! WebRTC receiver decodes as such; [`carries`] says which layouts go.

use crate::engine::audio_decode::split_opus_frames;

/// The 33-bit MPEG-TS PTS range.
const PTS_WRAP: u64 = 1 << 33;

/// Whether a source whose PMT signals Opus `channel_config_code`
/// (`ts_parse::opus_channel_config_code`) is a single mono or stereo Opus
/// stream — what an RTP Opus track carries. A source that signals none (an
/// edge's own muxer, writing only the registration descriptor) is taken as
/// one: Opus-in-TS without the extension descriptor has no way to describe
/// a multistream layout.
pub fn carries(channel_config_code: Option<u8>) -> bool {
    matches!(channel_config_code, None | Some(1) | Some(2))
}

/// The operator-facing name of the layout a `channel_config_code` signals.
pub fn describe(channel_config_code: u8) -> String {
    match channel_config_code {
        0 => "dual-mono Opus".to_string(),
        1 => "mono Opus".to_string(),
        2 => "stereo Opus".to_string(),
        n @ 3..=8 => format!("{n}-channel Opus"),
        n => format!("Opus with channel_config_code 0x{n:02X}"),
    }
}

/// The duration of an Opus packet in 48 kHz samples, from its TOC byte
/// (RFC 6716 §3.1) and, for a code-3 packet, its frame-count byte
/// (§3.2.5). `None` for a packet that has no duration: empty, a code-3
/// packet without its count byte or with no frames, or longer than the
/// 120 ms a packet may hold.
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame: u32 = match config {
        // SILK-only: 10 / 20 / 40 / 60 ms.
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        // Hybrid: 10 / 20 ms.
        12..=15 => [480, 960][usize::from(config & 1)],
        // CELT-only: 2.5 / 5 / 10 / 20 ms.
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frames: u32 = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    let samples = frame * frames;
    (frames > 0 && samples <= 5_760).then_some(samples)
}

/// The 48 kHz RTP timeline of one WebRTC session's Opus passthrough.
///
/// The 33-bit PTS is unwrapped first — converted straight to 48 kHz, its
/// wrap every 26.5 hours would step the RTP timestamp, since 2^33 ticks are
/// 2^33 × 8 / 15 samples, not a whole multiple of 2^32. A step backwards (an
/// input switch to an earlier timeline) is followed as it is, as the video's
/// RTP time follows it.
#[derive(Default)]
pub struct OpusTimeline {
    /// The last PTS seen (33 bits) and where it sits unwrapped, in 90 kHz
    /// ticks — offset by one wrap, so a step back from the first stays
    /// above zero.
    anchor: Option<(u64, u64)>,
    /// The 48 kHz time just past the last packet placed: where a PES
    /// without a PTS continues.
    next: Option<u64>,
}

impl OpusTimeline {
    /// Split one Opus-in-TS PES payload into its packets, each with its
    /// 48 kHz RTP time. A PES without a PTS (`pts` `None`) continues where
    /// the previous one ended, and is dropped when no PES has been placed
    /// yet. A packet whose duration cannot be read ends the PES there:
    /// nothing after it could be placed.
    pub fn place<'a>(&mut self, pes: &'a [u8], pts: Option<u64>) -> Vec<(&'a [u8], u64)> {
        let start = match pts {
            Some(pts) => {
                let pts = pts & (PTS_WRAP - 1);
                let unwrapped = match self.anchor {
                    None => pts + PTS_WRAP,
                    Some((last, last_unwrapped)) => {
                        let step = pts.wrapping_sub(last) & (PTS_WRAP - 1);
                        let step = if step >= PTS_WRAP / 2 {
                            step as i64 - PTS_WRAP as i64
                        } else {
                            step as i64
                        };
                        last_unwrapped.wrapping_add_signed(step)
                    }
                };
                self.anchor = Some((pts, unwrapped));
                // 48 000 / 90 000 = 8 / 15.
                (u128::from(unwrapped) * 8 / 15) as u64
            }
            None => match self.next {
                Some(next) => next,
                None => return Vec::new(),
            },
        };
        let mut placed = Vec::new();
        let mut at = start;
        for packet in split_opus_frames(pes) {
            let Some(samples) = packet_samples(packet) else {
                break;
            };
            placed.push((packet, at));
            at = at.wrapping_add(u64::from(samples));
        }
        self.next = Some(at);
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};

    /// 0.4 s of a 1 kHz tone as ffmpeg muxes it:
    /// `ffmpeg -f lavfi -i sine=frequency=1000:sample_rate=48000:duration=0.4
    ///  -ac 2 -c:a libopus -b:a 48k -f mpegts` — five PES of 20 ms packets,
    /// the first packet with `start_trim` 312, the last with `end_trim` 648,
    /// the PMT signalling stereo (channel_config_code 2).
    const STEREO_TS: &[u8] = include_bytes!("../testdata/sine1k_opus_48k_stereo.ts");
    /// The same tone as 5.1 (`-af pan=5.1|… -mapping_family 1`, 0.1 s): a
    /// multistream, channel_config_code 6.
    const FIVE_ONE_TS: &[u8] = include_bytes!("../testdata/sine1k_opus_48k_5_1.ts");

    /// The Opus PES of `ts`, in order. The demuxer hands a PES over when
    /// the next one starts, so the stream goes in twice: its last PES (the
    /// end-trimmed packet) comes out too, followed by the repeat's.
    fn opus_pes(ts: &[u8]) -> (TsDemuxer, Vec<(Vec<u8>, Option<u64>)>) {
        let mut demux = TsDemuxer::new(None);
        let pes = demux
            .demux(&[ts, ts].concat())
            .into_iter()
            .filter_map(|f| match f {
                DemuxedFrame::Opus {
                    data,
                    pts,
                    pts_known,
                } => Some((data, pts_known.then_some(pts))),
                _ => None,
            })
            .collect();
        (demux, pes)
    }

    #[test]
    fn packet_samples_reads_the_toc() {
        // ffmpeg's libopus packets: hybrid FB, SILK WB, CELT SWB / FB, 20 ms.
        for toc in [0x7C, 0x4C, 0xDC, 0xFC] {
            assert_eq!(packet_samples(&[toc, 0x00]), Some(960), "toc {toc:#04x}");
        }
        assert_eq!(packet_samples(&[0x18]), Some(2_880), "SILK NB 60 ms");
        assert_eq!(packet_samples(&[0x80]), Some(120), "CELT NB 2.5 ms");
        assert_eq!(
            packet_samples(&[0x7D, 0, 0]),
            Some(1_920),
            "code 1: two frames"
        );
        assert_eq!(
            packet_samples(&[0x7E, 1, 0, 0]),
            Some(1_920),
            "code 2: two frames"
        );
        assert_eq!(
            packet_samples(&[0x7F, 0x03]),
            Some(2_880),
            "code 3: three frames"
        );
        assert_eq!(
            packet_samples(&[0x7F, 0x86]),
            Some(5_760),
            "code 3 VBR + padding, six"
        );
        assert_eq!(
            packet_samples(&[0x1B, 0x02]),
            Some(5_760),
            "two 60 ms frames: 120 ms"
        );
        assert_eq!(packet_samples(&[]), None);
        assert_eq!(packet_samples(&[0x7F]), None, "code 3 without its count");
        assert_eq!(packet_samples(&[0x7F, 0x00]), None, "code 3, no frames");
        assert_eq!(packet_samples(&[0x7F, 0x07]), None, "140 ms");
        assert_eq!(packet_samples(&[0x1B, 0x03]), None, "180 ms");
    }

    #[test]
    fn layouts_rtp_carries() {
        for code in [None, Some(1), Some(2)] {
            assert!(carries(code), "{code:?}");
        }
        for code in [0, 3, 6, 8, 0x82, 0xFF] {
            assert!(!carries(Some(code)), "{code:#04x}");
        }
        assert_eq!(describe(0), "dual-mono Opus");
        assert_eq!(describe(6), "6-channel Opus");
        assert_eq!(describe(0x82), "Opus with channel_config_code 0x82");
    }

    /// ffmpeg's stream goes out as the packets it carries, each its own
    /// size (the trimmed first and last too), on one gapless 48 kHz
    /// timeline: 960 samples apart, across every PES boundary, the first
    /// where the first PES's PTS says.
    #[test]
    fn an_ffmpeg_opus_stream_is_placed_packet_by_packet() {
        let (demux, pes) = opus_pes(STEREO_TS);
        assert_eq!(demux.opus_channel_config(), Some(2));
        assert!(carries(demux.opus_channel_config()));
        assert_eq!(pes[0].1, Some(126_000));
        assert_eq!(pes[5].1, Some(126_000), "the repeat");

        let mut timeline = OpusTimeline::default();
        let placed: Vec<(Vec<u8>, u64)> = pes[..5]
            .iter()
            .flat_map(|(data, pts)| {
                timeline
                    .place(data, *pts)
                    .into_iter()
                    .map(|(p, t)| (p.to_vec(), t))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(placed.len(), 21);
        let sizes: Vec<usize> = placed.iter().map(|(p, _)| p.len()).collect();
        assert_eq!(&sizes[..5], &[106, 93, 91, 90, 85]);
        assert_eq!(sizes[20], 232);
        assert!(placed.iter().all(|(p, _)| packet_samples(p) == Some(960)));
        let first = (u128::from(126_000 + PTS_WRAP) * 8 / 15) as u64;
        for (k, (_, t)) in placed.iter().enumerate() {
            assert_eq!(*t, first + 960 * k as u64, "packet {k}");
        }
    }

    /// A 5.1 stream is signalled as such; its packets are a multistream the
    /// passthrough does not carry.
    #[test]
    fn a_five_one_stream_is_not_carried() {
        let (demux, pes) = opus_pes(FIVE_ONE_TS);
        assert!(!pes.is_empty());
        assert_eq!(demux.opus_channel_config(), Some(6));
        assert!(!carries(demux.opus_channel_config()));
    }

    fn au(packet: &[u8]) -> Vec<u8> {
        let mut out = vec![0x7F, 0xE0, packet.len() as u8];
        out.extend_from_slice(packet);
        out
    }

    /// The 33-bit wrap leaves the RTP time running on; a PES without a PTS
    /// continues the previous one; a step back is followed.
    #[test]
    fn the_timeline_unwraps_continues_and_follows_a_step() {
        let pes: Vec<u8> = [au(&[0xFC, 1]), au(&[0xFC, 2])].concat();
        let mut timeline = OpusTimeline::default();
        let before = timeline.place(&pes, Some(PTS_WRAP - 1_800));
        assert_eq!(before.len(), 2);
        let wrapped = timeline.place(&pes, Some(1_800));
        assert_eq!(wrapped[0].1, before[1].1 + 960, "across the wrap");
        assert_eq!(wrapped[1].1 - wrapped[0].1, 960);
        let no_pts = timeline.place(&pes, None);
        assert_eq!(no_pts[0].1, wrapped[1].1 + 960);
        let back = timeline.place(&pes, Some(PTS_WRAP + 1_800 - 90_000));
        assert_eq!(back[0].1, wrapped[0].1 - 48_000, "one second back");
        // A PES without a PTS before any with one has nowhere to go.
        assert!(OpusTimeline::default().place(&pes, None).is_empty());
    }

    /// A packet with no readable duration ends its PES; malformed control
    /// headers place nothing and never panic.
    #[test]
    fn malformed_input_places_what_it_can() {
        let pes: Vec<u8> = [au(&[0xFC, 1]), au(&[0x7F]), au(&[0xFC, 3])].concat();
        let placed = OpusTimeline::default().place(&pes, Some(0));
        assert_eq!(
            placed.len(),
            1,
            "stops at the code-3 packet without a count"
        );
        let mut timeline = OpusTimeline::default();
        for junk in [
            &[][..],
            &[0x7F],
            &[0x7F, 0xE0],
            &[0x7F, 0xE0, 0x09, 0xFC],
            &[0xFF; 40],
        ] {
            assert!(timeline.place(junk, Some(0)).is_empty());
        }
        for pts in [0, u64::MAX, PTS_WRAP, PTS_WRAP / 2] {
            let _ = timeline.place(&au(&[0xFC, 1]), Some(pts));
        }
    }
}
