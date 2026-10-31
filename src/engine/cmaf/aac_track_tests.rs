// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The CMAF output's AAC track as a player reads it — the `esds`
//! AudioSpecificConfig and the raw access units in the samples — for a
//! PCE-signalled source (edge#210 item 3) and for a re-encode (the encoder's
//! ADTS header used to stay in every sample, and an HE-AAC track to announce
//! the output rate under AAC-LC).

use super::*;
use crate::engine::aac_config::AacConfig;
use crate::engine::ts_test_fixtures::{aac_program_ts, he_aac_adts, pce_signalled_adts, sine_aac_lc_frames};
use crate::manager::events::Event;

fn config(audio_encode: Option<serde_json::Value>) -> CmafOutputConfig {
    let mut cfg = serde_json::json!({
        "id": "o",
        "name": "o",
        "ingest_url": "https://origin.invalid/live",
        "segment_duration_secs": 1.0,
    });
    if let Some(ae) = audio_encode {
        cfg["audio_encode"] = ae;
    }
    serde_json::from_value(cfg).unwrap()
}

/// `ts` demuxed and every AAC frame handed to [`handle_audio_frame`] as the
/// output loop hands it, with the re-encoder the output would build for
/// `config`. The state, whether the track existed after each frame, and the
/// events raised.
fn run(ts: &[u8], config: &CmafOutputConfig) -> (CmafState, Vec<bool>, Vec<Event>) {
    let mut state = CmafState::new();
    if let Some(ae) = &config.audio_encode {
        let reenc = AudioReencoder::new(ae, config.transcode.as_ref(), &CancellationToken::new(), "o", "f")
            .expect("the re-encoder");
        state.audio_reencoder = Some(reenc);
    }
    let (events, mut rx) = crate::manager::events::event_channel();
    let mut demux = TsDemuxer::new(None);
    let mut built = Vec::new();
    for pkt in ts.chunks(188) {
        for f in demux.demux(pkt) {
            if let DemuxedFrame::Aac { data, pts } = f {
                handle_audio_frame(&mut state, &demux, config, &data, pts, &events, "f");
                built.push(state.audio_seg.is_some());
            }
        }
    }
    let raised = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    (state, built, raised)
}

/// The track's AudioSpecificConfig and its buffered samples' bodies.
fn track_and_samples(state: &mut CmafState) -> (Vec<u8>, u16, Vec<Vec<u8>>) {
    let seg = state.audio_seg.as_mut().expect("an audio track");
    let asc = seg.track.aac_asc().expect("an AAC track").to_vec();
    let channels = seg.track.channels;
    let (_, _, samples) = seg.take_pending_samples(u64::MAX).expect("samples");
    (asc, channels, samples.into_iter().map(|s| s.data).collect())
}

/// Every sample decodes in fdk set up from `asc` alone, to `channels` at
/// 48 kHz, `samples` a channel from the first that reaches that format —
/// among the first 8, as an HE-AAC decoder may start on its core — and none
/// opens on an ADTS sync. The number of samples.
#[cfg(feature = "fdk-aac")]
fn assert_decodes(asc: &[u8], bodies: &[Vec<u8>], channels: usize, samples: usize) -> usize {
    let mut dec = aac_audio::AacDecoder::open_raw(asc).expect("a decoder from the esds alone");
    let mut steady = None;
    for (i, b) in bodies.iter().enumerate() {
        assert!(
            !(b.len() >= 2 && b[0] == 0xFF && b[1] & 0xF0 == 0xF0),
            "sample {i}: an ADTS header inside the sample: {:02x?}",
            &b[..b.len().min(7)]
        );
        let pcm = dec.decode_frame(b).unwrap_or_else(|e| panic!("sample {i} does not decode: {e}"));
        let rate = dec.stream_info().map_or(0, |si| si.sample_rate);
        if steady.is_none() && (rate, pcm.planar.len()) == (48_000, channels) {
            steady = Some(i);
        }
        if steady.is_some() {
            assert_eq!((rate, pcm.planar.len(), pcm.frame_size), (48_000, channels, samples), "sample {i}");
        }
    }
    let first = steady.expect("no sample decodes to the format");
    assert!(first < 8, "the format only from sample {first}");
    bodies.len()
}

fn errors_raised(raised: &[Event]) -> Vec<String> {
    raised.iter().map(|e| e.message.clone()).collect()
}

/// edge#210 item 3, CMAF passthrough: a PCE-signalled source (channel
/// configuration 0, the PCE in frame 5 and every 12th after it) builds no
/// track until its first PCE, then one whose `esds` carries it, and every
/// frame from there goes into the samples as it came, PCE or not — all
/// decodable from the `esds` alone. It used to build the two-byte
/// configuration from the first frame: an init no decoder opens.
#[cfg(feature = "fdk-aac")]
#[test]
fn a_pce_signalled_source_waits_for_its_pce_and_carries_it_in_the_init() {
    let sine = sine_aac_lc_frames();
    let looped: Vec<&[u8]> = sine.iter().cycle().take(40).copied().collect();
    let adts = pce_signalled_adts(&looped, 5, 12);
    let (mut state, built, raised) = run(&aac_program_ts(&adts), &config(None));
    assert_eq!(built.iter().position(|&b| b), Some(5), "the track from the first PCE");
    assert!(raised.is_empty(), "{:?}", errors_raised(&raised));
    let (asc, channels, bodies) = track_and_samples(&mut state);
    let mut raw = Vec::new();
    let mut off = 0;
    while off + 7 <= adts.len() {
        let len = (usize::from(adts[off + 3] & 0x03) << 11) | (usize::from(adts[off + 4]) << 3) | (usize::from(adts[off + 5]) >> 5);
        raw.push(adts[off + 7..off + len].to_vec());
        off += len;
    }
    let want = AacConfig::from_adts(1, 3, 0, &raw[5]).expect("frame 5's PCE");
    assert_eq!(asc, want.audio_specific_config(), "the PCE in the esds");
    assert_eq!(channels, 2);
    assert_eq!(bodies, raw[5..], "every frame from the first PCE, as it came");
    assert_decodes(&asc, &bodies, 2, 1_024);
}

/// A PCE-signalled source that never carries its PCE builds no track and
/// says so once, after five seconds of its audio: Warning
/// `aac_channel_layout_missing`, output-scoped.
#[test]
fn a_pce_signalled_source_without_its_pce_is_held_back_and_reported() {
    let sine = sine_aac_lc_frames();
    let looped: Vec<&[u8]> = sine.iter().cycle().take(300).copied().collect();
    let adts = pce_signalled_adts(&looped, usize::MAX, 1);
    let (state, built, raised) = run(&aac_program_ts(&adts), &config(None));
    assert!(state.audio_seg.is_none() && !built.iter().any(|&b| b), "no track");
    let codes: Vec<&serde_json::Value> = raised.iter().filter_map(|e| e.details.as_ref()).map(|d| &d["error_code"]).collect();
    assert_eq!(codes, ["aac_channel_layout_missing"], "said once: {:?}", errors_raised(&raised));
    assert_eq!(raised[0].output_id.as_deref(), Some("o"));
}

/// The same source re-encoded (`audio_encode`): its decoder is built from
/// the configuration with the PCE once one has arrived, and the frames
/// before it are held back quietly. It used to fail to open a decoder for
/// channel configuration 0 and raise an `audio_encode` Warning on every
/// frame, carrying no audio.
#[cfg(feature = "fdk-aac")]
#[test]
fn a_pce_signalled_source_is_re_encoded() {
    let sine = sine_aac_lc_frames();
    let looped: Vec<&[u8]> = sine.iter().cycle().take(60).copied().collect();
    let adts = pce_signalled_adts(&looped, 5, 12);
    let (mut state, _, raised) = run(&aac_program_ts(&adts), &config(Some(serde_json::json!({ "codec": "aac_lc" }))));
    assert!(raised.is_empty(), "{:?}", errors_raised(&raised));
    let (asc, channels, bodies) = track_and_samples(&mut state);
    assert_eq!((asc, channels), (vec![0x11, 0x90], 2), "AAC-LC 48 kHz stereo");
    let n = assert_decodes(&[0x11, 0x90], &bodies, 2, 1_024);
    assert!(n >= 45, "{n} samples");
}

/// A re-encode's track is what the encoder's first frame says it emits, and
/// each sample is the bare access unit: AAC-LC at 48 kHz stereo; HE-AAC v1
/// at its 24 kHz core, SBR implicit; v2 at its mono core, PS implicit — as a
/// passed-through HE-AAC source's track says. Each sample used to keep the
/// encoder's ADTS header, and every target announce 48 kHz AAC-LC at the
/// output's channel count — for HE-AAC, a core twice its real rate.
#[cfg(feature = "fdk-aac")]
#[test]
fn a_re_encode_carries_raw_access_units_under_the_encoders_own_configuration() {
    let sine = sine_aac_lc_frames();
    let looped: Vec<&[u8]> = sine.iter().cycle().take(100).copied().collect();
    let adts: Vec<u8> = looped.concat();
    for (codec, asc, samples, at_least) in
        [("aac_lc", [0x11, 0x90], 1_024, 90), ("he_aac_v1", [0x13, 0x10], 2_048, 45), ("he_aac_v2", [0x13, 0x08], 2_048, 45)]
    {
        let (mut state, _, raised) = run(&aac_program_ts(&adts), &config(Some(serde_json::json!({ "codec": codec }))));
        assert!(raised.is_empty(), "{codec}: {:?}", errors_raised(&raised));
        let (got, channels, bodies) = track_and_samples(&mut state);
        assert_eq!(got, asc, "{codec}: the esds");
        assert_eq!(channels, u16::from(asc[1] >> 3 & 0x0F), "{codec}: the core's channels");
        let n = assert_decodes(&got, &bodies, 2, samples);
        assert!(n >= at_least, "{codec}: {n} samples");
    }
}

/// An HE-AAC v1 re-encode of an HE-AAC v2 source: the same, through the
/// decoder's SBR / PS output.
#[cfg(feature = "fdk-aac")]
#[test]
fn an_he_aac_source_re_encoded_carries_raw_access_units() {
    let (mut state, _, raised) =
        run(&aac_program_ts(&he_aac_adts(true)), &config(Some(serde_json::json!({ "codec": "he_aac_v1" }))));
    assert!(raised.is_empty(), "{:?}", errors_raised(&raised));
    let (asc, _, bodies) = track_and_samples(&mut state);
    assert_eq!(asc, [0x13, 0x10], "HE-AAC v1: AAC-LC at the 24 kHz core, stereo");
    assert_decodes(&asc, &bodies, 2, 2_048);
}
