// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RTMP input — accepts incoming publish connections and remuxes to MPEG-TS.
//!
//! This module runs an RTMP server that accepts one publisher at a time (OBS,
//! ffmpeg, Wirecast, etc.). The received H.264 video and AAC audio are remuxed
//! into MPEG-TS packets and pushed into the broadcast channel as `RtpPacket`
//! structs, identical to how SRT and RTP inputs work.
//!
//! ## Data flow
//!
//! ```text
//! [OBS/ffmpeg] --(RTMP/TCP)--> [RTMP server] --(FLV tags)--> [TS muxer] --(TS packets)--> [broadcast channel]
//! ```
//!
//! ## Configuration
//!
//! ```json
//! {
//!   "type": "rtmp",
//!   "listen_addr": "0.0.0.0:1935",
//!   "app": "live",
//!   "stream_key": "my_secret_key"
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtmpInputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::InputTranscoder;
use super::packet::RtpPacket;
use super::rtmp::server::{RtmpMediaMessage, RtmpServerConfig, run_rtmp_server};
use super::rtmp::ts_mux::TsMuxer;

/// Spawn the RTMP input task.
///
/// Returns a `JoinHandle` for the task. The task runs until `cancel` is triggered
/// or the RTMP server encounters a fatal error.
pub fn spawn_rtmp_input(
    config: RtmpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    force_idr: Arc<std::sync::atomic::AtomicBool>,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("RTMP input starting on {} (app='{}')", config.listen_addr, config.app);

        let (media_tx, media_rx) = mpsc::channel::<RtmpMediaMessage>(1024);
        let is_publishing = Arc::new(AtomicBool::new(false));

        let server_config = RtmpServerConfig {
            listen_addr: config.listen_addr.clone(),
            expected_app: config.app.clone(),
            expected_stream_key: config.stream_key.clone(),
        };

        // Spawn the RTMP server
        let cancel_server = cancel.clone();
        let is_pub = is_publishing.clone();
        let server_event_sender = event_sender.clone();
        let server_flow_id = flow_id.clone();
        let server_listen_addr = config.listen_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = run_rtmp_server(server_config, media_tx, is_pub, cancel_server).await {
                tracing::error!("RTMP server error: {e:#}");
                use crate::manager::events::{BindProto, BindScope};
                let scope = BindScope::flow(&server_flow_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    server_event_sender.emit_port_conflict(
                        "RTMP server",
                        &server_listen_addr,
                        BindProto::Tcp,
                        scope,
                        &e,
                    );
                } else {
                    server_event_sender.emit_flow_with_details(
                        EventSeverity::Critical, category::RTMP,
                        format!("RTMP server error: {e}"),
                        &server_flow_id,
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        });

        // Process media messages from the RTMP server
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("RTMP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("RTMP input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
            &event_sender,
        );
        let pid_overrides_for_mux = config.pid_overrides.clone();
        // Synthetic-TS input: TsMuxer already applies pid_overrides[1].
        // Pass `pid_overrides: None` to the post-process so the override
        // rewriter doesn't double-rewrite. program_filter is a no-op
        // (TsMuxer always emits program 1) but harmless; pid_map can
        // still mechanically remap on top.
        let passthrough_clock = config.passthrough_clock.unwrap_or(false);
        let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: None,
            pid_map: config.pid_map.as_ref(),
            passthrough_clock,
            av_sync_pacer: av_sync_pacer.as_ref(),
            av_skew: Some(&av_skew_for_post),
        });
        // The muxer-mode rewriter's input-scoped Warnings — above all
        // `clock_rewrite_no_pcr`: an audio-only publish's PMT names a
        // video PID as PCR_PID that never carries a PCR.
        if let Some(p) = post.as_mut() {
            p.set_event_sender(&event_sender, &input_id);
        }
        if let Some(ref _p) = post {
            tracing::info!(
                "RTMP input: ingress post-process active (program_filter={} pid_map={} passthrough_clock={})",
                config.program_number.is_some(),
                config.pid_map.is_some(),
                passthrough_clock,
            );
        }
        // Per-input ingress publisher (fixed-delay + de-jitter modes).
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            // RTMP is TCP + a locally-synthesised TS clock — no media-rate
            // ingress de-jitter.
            crate::engine::ingress_publisher::IngressBuffering {
                delay_ms: config.ingress_delay_ms,
                ..Default::default()
            },
            broadcast_tx,
            &input_id,
            cancel.clone(),
            stats.clone(),
        );
        process_media(media_rx, publisher, stats, cancel, event_sender, flow_id, &mut transcoder, &mut post, pid_overrides_for_mux).await;
    })
}

/// Process media messages from the RTMP server, mux into TS, and push to broadcast.
async fn process_media(
    mut media_rx: mpsc::Receiver<RtmpMediaMessage>,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
    pid_overrides: Option<crate::config::models::TsPidOverridesMap>,
) {
    let mut muxer = TsMuxer::new();
    if let Some(po) = pid_overrides.as_ref()
        && let Some(entry) = po.get(&1) {
                muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
    let mut seq_num: u16 = 0;
    let mut has_sent_sps_pps = false;
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut audio_sample_rate_idx: u8 = 4; // default 44.1kHz
    let mut audio_channels: u8 = 2; // default stereo

    // Audio PTS derivation state. RTMP carries millisecond-resolution
    // timestamps but real AAC frames are 1024 samples ≈ 21.33 ms (48 kHz)
    // or 23.22 ms (44.1 kHz). Multiplying ms*90 produces PES PTS deltas
    // that oscillate between 23 ms and 24 ms (or 21 ms / 22 ms) — close
    // on average but never exactly the AAC frame duration. Downstream
    // tools that decode the AAC and re-derive frame timing from a
    // sample counter (ffmpeg's `-f null` PCM pipeline, every audio
    // muxer with strict DTS checks) flag the resulting irregular spacing
    // as "non monotonically increasing dts" — see Bug #11 in the
    // 2026-04-09 test report.
    //
    // The fix is to anchor a sample counter to the FIRST audio frame's
    // RTMP timestamp and emit every subsequent PES PTS as exactly
    // `anchor + frames * 1024 * 90000 / sample_rate`. This produces a
    // strictly monotonic, evenly-spaced sequence whose deltas exactly
    // match the AAC nominal frame duration, satisfying every downstream
    // muxer regardless of how it computes DTS.
    let mut audio_anchor_pts_90khz: Option<u64> = None;
    let mut audio_frames_emitted: u64 = 0;
    // Whether this publish carries video — what the PMT lists and which PID
    // carries the PCR. See `PublishLayout`.
    let mut layout = PublishLayout::default();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTMP input shutting down");
                return;
            }
            msg = media_rx.recv() => {
                match msg {
                    Some(RtmpMediaMessage::Video { data, timestamp_ms }) => {
                        if data.len() < 5 {
                            continue;
                        }

                        let frame_type = (data[0] >> 4) & 0x0F;
                        let codec_id = data[0] & 0x0F;
                        let avc_packet_type = data[1];

                        // Only handle AVC (H.264)
                        if codec_id != 7 {
                            continue;
                        }
                        // Video after all: a publish taken as audio-only
                        // moves back (PMT version bump, PCR on video).
                        apply_layout(&mut muxer, layout.on_video(), &flow_id);

                        match avc_packet_type {
                            0 => {
                                // AVC sequence header (SPS/PPS)
                                if let Some((s, p)) = parse_avc_decoder_config(&data[5..]) {
                                    sps = Some(s);
                                    pps = Some(p);
                                    muxer.set_has_audio(true); // Assume audio until proven otherwise
                                    tracing::info!("RTMP: received AVC sequence header (SPS+PPS)");
                                    event_sender.emit_flow(
                                        EventSeverity::Info,
                                        category::RTMP,
                                        "RTMP publisher connected",
                                        &flow_id,
                                    );
                                }
                            }
                            1 => {
                                // AVC NALU data
                                let is_keyframe = frame_type == 1;
                                let composition_time = (data[2] as i32) << 16 | (data[3] as i32) << 8 | data[4] as i32;
                                // Sign extend 24-bit
                                let composition_time = if composition_time & 0x800000 != 0 {
                                    composition_time | !0xFFFFFF
                                } else {
                                    composition_time
                                };

                                let dts_ms = timestamp_ms as i64;
                                let pts_ms = dts_ms + composition_time as i64;

                                let dts_90khz = TIMELINE_START_90K + (dts_ms.max(0) * 90) as u64;
                                let pts_90khz = TIMELINE_START_90K + (pts_ms.max(0) * 90) as u64;

                                // Convert length-prefixed NALUs to Annex B
                                let annex_b = length_prefixed_to_annex_b(&data[5..], &sps, &pps, is_keyframe && !has_sent_sps_pps);

                                if is_keyframe && sps.is_some() {
                                    has_sent_sps_pps = true;
                                }

                                let ts_packets = muxer.mux_video(&annex_b, pts_90khz, dts_90khz, is_keyframe);

                                // Bundle all TS packets from this frame into one RtpPacket
                                if !ts_packets.is_empty() {
                                    let total_len: usize = ts_packets.iter().map(|c| c.len()).sum();
                                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                                    for chunk in &ts_packets {
                                        combined.extend_from_slice(chunk);
                                    }
                                    let recv_time = wall_clock_micros();
                                    let packet = RtpPacket {
                                        data: combined.freeze(),
                                        sequence_number: seq_num,
                                        rtp_timestamp: dts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                        upstream_seq: None,
                                        upstream_leg_id: None,
                                        sender_timestamp_us: None,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
                                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, &publisher, packet);
                                    } else {
                                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(RtmpMediaMessage::Audio { data, timestamp_ms }) => {
                        if data.len() < 2 {
                            continue;
                        }

                        let sound_format = (data[0] >> 4) & 0x0F;
                        let aac_packet_type = data[1];

                        // Only handle AAC
                        if sound_format != 10 {
                            continue;
                        }

                        match aac_packet_type {
                            0 => {
                                // AAC sequence header (AudioSpecificConfig)
                                if data.len() >= 4 {
                                    let asc = &data[2..];
                                    // Parse AudioSpecificConfig (2 bytes min)
                                    // Bits: [audioObjectType:5][frequencyIndex:4][channelConfiguration:4]...
                                    let freq_idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
                                    let ch_cfg = (asc[1] >> 3) & 0x0F;
                                    audio_sample_rate_idx = freq_idx;
                                    audio_channels = ch_cfg;
                                    muxer.set_has_audio(true);
                                    tracing::info!("RTMP: received AAC sequence header (freq_idx={freq_idx}, channels={ch_cfg})");
                                }
                            }
                            1 => {
                                // Raw AAC frame
                                let raw_aac = &data[2..];

                                // Sample-counter-derived PTS — see
                                // `audio_anchor_pts_90khz` doc above for the
                                // rationale (Bug #11). The first frame
                                // anchors to its RTMP-supplied wall time;
                                // subsequent frames are spaced exactly one
                                // AAC frame duration apart at 90 kHz.
                                let sample_rate_hz = aac_sample_rate_hz(audio_sample_rate_idx);
                                let anchor = *audio_anchor_pts_90khz
                                    .get_or_insert_with(|| TIMELINE_START_90K + (timestamp_ms as u64) * 90);
                                let pts_90khz = anchor
                                    + audio_frames_emitted * 1024 * 90_000 / sample_rate_hz as u64;
                                audio_frames_emitted += 1;
                                if let Some(has_video) = layout.on_audio(pts_90khz) {
                                    apply_layout(&mut muxer, has_video, &flow_id);
                                }

                                let ts_packets = muxer.mux_audio(raw_aac, pts_90khz, audio_sample_rate_idx, audio_channels);

                                if !ts_packets.is_empty() {
                                    let total_len: usize = ts_packets.iter().map(|c| c.len()).sum();
                                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                                    for chunk in &ts_packets {
                                        combined.extend_from_slice(chunk);
                                    }
                                    let recv_time = wall_clock_micros();
                                    let packet = RtpPacket {
                                        data: combined.freeze(),
                                        sequence_number: seq_num,
                                        rtp_timestamp: pts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                        upstream_seq: None,
                                        upstream_leg_id: None,
                                        sender_timestamp_us: None,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
                                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, &publisher, packet);
                                    } else {
                                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(RtmpMediaMessage::Metadata { declares_video }) => {
                        tracing::debug!(?declares_video, "RTMP: received metadata");
                        if let Some(has_video) = layout.on_metadata(declares_video) {
                            apply_layout(&mut muxer, has_video, &flow_id);
                        }
                    }
                    Some(RtmpMediaMessage::Disconnected) => {
                        tracing::info!("RTMP publisher disconnected, waiting for reconnection");
                        event_sender.emit_flow(
                            EventSeverity::Warning,
                            category::RTMP,
                            "RTMP publisher disconnected",
                            &flow_id,
                        );
                        has_sent_sps_pps = false;
                        // Reset audio anchor so the next publisher restarts
                        // the sample counter from its own RTMP wall time.
                        audio_anchor_pts_90khz = None;
                        audio_frames_emitted = 0;
                        // The next publisher states its own layout. The
                        // muxer keeps this one's until it does.
                        layout = PublishLayout::default();
                    }
                    None => {
                        // Channel closed
                        tracing::info!("RTMP input channel closed");
                        return;
                    }
                }
            }
        }
    }
}

/// Where a publish's timeline starts (90 kHz): its RTMP timestamps, which
/// start at 0, are carried from the muxer's PCR lead, so the first PCR —
/// that far behind the first timestamp — is 0 rather than just below the
/// 33-bit wrap.
const TIMELINE_START_90K: u64 = super::rtmp::ts_mux::PCR_LEAD_90K;

/// Audio media time an RTMP publish may carry without a single H.264 tag
/// before it is taken as audio-only (1 s). Publishers send the AVC sequence
/// header straight after `onMetaData`, ahead of any audio of note.
const AUDIO_ONLY_AFTER_90K: u64 = 90_000;

/// Whether an RTMP publish carries video: what the TS muxer's PMT lists,
/// and whether its PCR rides the video or the audio PID.
///
/// The muxer assumed video, so an audio-only publish (a radio encoder, an
/// AAC-only push) went out with a PMT naming an absent video PID as PCR_PID
/// and no PCR at all — every output of the flow unclocked, and the ingress
/// PTS rewriter dropping its first 2 s of audio waiting for one. RTMP
/// carries no FLV file header (`TypeFlagsAudio` / `TypeFlagsVideo` exist
/// only in `.flv` files), so the answer comes from the publisher's
/// `onMetaData` and from the tags:
/// - `onMetaData` describing audio and no video → audio-only at once;
/// - an H.264 tag → video for the rest of the publish, including after
///   audio-only was decided (a late video tag: the muxer bumps the PMT
///   version and PCR moves to the video PID);
/// - audio for [`AUDIO_ONLY_AFTER_90K`] with no H.264 tag — no metadata, or
///   metadata naming a video codec this input does not remux — →
///   audio-only, and only a video tag reverses that.
///
/// Until one of those the muxer keeps what it had: video on a fresh input,
/// the previous publisher's layout after a reconnect.
#[derive(Debug, Default)]
struct PublishLayout {
    /// An H.264 tag arrived in this publish.
    video_seen: bool,
    /// Audio-only was decided from the tags (the audio ran on alone).
    audio_only_by_tags: bool,
    /// PTS of this publish's first audio frame.
    first_audio_90k: Option<u64>,
}

impl PublishLayout {
    /// `onMetaData` said `declares_video` (`None`: nothing). Returns the
    /// video presence to apply. The tags outrank it: a publish that has
    /// sent video, or whose audio already ran on alone, keeps its layout.
    fn on_metadata(&mut self, declares_video: Option<bool>) -> Option<bool> {
        if self.video_seen || self.audio_only_by_tags {
            return None;
        }
        declares_video
    }

    /// An H.264 tag arrived: video.
    fn on_video(&mut self) -> bool {
        self.video_seen = true;
        self.audio_only_by_tags = false;
        true
    }

    /// An audio frame stamped `pts_90k`: audio-only once the audio has run
    /// [`AUDIO_ONLY_AFTER_90K`] with no H.264 tag.
    fn on_audio(&mut self, pts_90k: u64) -> Option<bool> {
        if self.video_seen {
            return None;
        }
        let first = *self.first_audio_90k.get_or_insert(pts_90k);
        if pts_90k.saturating_sub(first) >= AUDIO_ONLY_AFTER_90K {
            self.audio_only_by_tags = true;
        }
        self.audio_only_by_tags.then_some(false)
    }
}

/// Apply a publish's video presence to the muxer, logging a real change.
fn apply_layout(muxer: &mut TsMuxer, has_video: bool, flow_id: &str) {
    if muxer.change_has_video(has_video) {
        tracing::info!(
            flow_id,
            has_video,
            "RTMP: publish {} — the PMT names the {} PID as PCR_PID",
            if has_video { "carries video" } else { "is audio-only" },
            if has_video { "video" } else { "audio" },
        );
    }
}

/// Map an MPEG-4 AAC `samplingFrequencyIndex` (per ISO 14496-3 §1.6.3.4
/// Table 1.18) to the actual sample rate in Hz. Used to derive the
/// per-frame PTS increment for the audio sample-counter clock (Bug #11).
///
/// Returns 44100 for unknown indices, matching the legacy
/// `audio_sample_rate_idx` default in [`process_media`]. The escape
/// value (idx 15, "explicit frequency in next 24 bits") is unsupported
/// in the AAC sequence headers we receive over RTMP and falls through
/// to the default.
fn aac_sample_rate_hz(idx: u8) -> u32 {
    match idx {
        0 => 96_000,
        1 => 88_200,
        2 => 64_000,
        3 => 48_000,
        4 => 44_100,
        5 => 32_000,
        6 => 24_000,
        7 => 22_050,
        8 => 16_000,
        9 => 12_000,
        10 => 11_025,
        11 => 8_000,
        12 => 7_350,
        _ => 44_100,
    }
}

/// Wall-clock microseconds since the Unix epoch.
///
/// Used as the `recv_time_us` field on `RtpPacket`s emitted by the RTMP
/// input. Downstream consumers (notably the HLS output) compare this
/// against the segment-start time to decide when to cut a new segment, so
/// it must advance with real wall-clock time. The previous implementation
/// used `Instant::now().elapsed()` which always returns ~0 µs (the elapsed
/// time since the freshly-constructed Instant), so HLS segment cutting
/// from RTMP-sourced flows never crossed the duration threshold and
/// produced zero segments — see the 2026-04-09 Bug B fix in QUALITY_REPORT.md.
fn wall_clock_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Parse AVCDecoderConfigurationRecord to extract SPS and PPS.
fn parse_avc_decoder_config(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.len() < 8 {
        return None;
    }

    // Skip: configurationVersion(1) + AVCProfileIndication(1) + profile_compat(1)
    //       + AVCLevelIndication(1) + lengthSizeMinusOne(1)
    let mut pos = 5;

    // numOfSequenceParameterSets (lower 5 bits)
    if pos >= data.len() {
        return None;
    }
    let num_sps = (data[pos] & 0x1F) as usize;
    pos += 1;

    let mut sps_data = None;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + sps_len > data.len() {
            return None;
        }
        sps_data = Some(data[pos..pos + sps_len].to_vec());
        pos += sps_len;
    }

    // numOfPictureParameterSets
    if pos >= data.len() {
        return None;
    }
    let num_pps = data[pos] as usize;
    pos += 1;

    let mut pps_data = None;
    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            return None;
        }
        let pps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + pps_len > data.len() {
            return None;
        }
        pps_data = Some(data[pos..pos + pps_len].to_vec());
        pos += pps_len;
    }

    match (sps_data, pps_data) {
        (Some(s), Some(p)) => Some((s, p)),
        _ => None,
    }
}

/// Convert length-prefixed NALUs to Annex B format (start codes).
/// Optionally prepend SPS/PPS for keyframes.
fn length_prefixed_to_annex_b(
    data: &[u8],
    sps: &Option<Vec<u8>>,
    pps: &Option<Vec<u8>>,
    prepend_sps_pps: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 128);

    // Prepend SPS/PPS with start codes before keyframes
    if prepend_sps_pps {
        if let Some(s) = sps {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(s);
        }
        if let Some(p) = pps {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(p);
        }
    }

    // Convert each length-prefixed NALU to Annex B
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nalu_len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + nalu_len > data.len() {
            break;
        }

        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_parse::{extract_pcr, ts_pid, ts_pusi};

    /// What `process_media` published for a sequence of RTMP messages: every
    /// PMT as `(version, PCR_PID, ES PIDs)` in order, and the PIDs whose
    /// packets carried a PCR, in order of the first PCR on each.
    struct Published {
        pmts: Vec<(u8, u16, Vec<u16>)>,
        pcr_pids: Vec<u16>,
        video_packets: usize,
    }

    async fn publish(msgs: Vec<RtmpMediaMessage>) -> Published {
        let (tx, rx) = mpsc::channel(8192);
        for m in msgs {
            tx.send(m).await.unwrap();
        }
        drop(tx);
        let (btx, mut brx) = broadcast::channel(8192);
        let stats = Arc::new(FlowStatsAccumulator::new("f".into(), "flow".into(), "rtmp".into()));
        let cancel = CancellationToken::new();
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            Default::default(),
            btx,
            "in",
            cancel.clone(),
            stats.clone(),
        );
        let (events, _events_rx) = crate::manager::events::event_channel();
        process_media(rx, publisher, stats, cancel, events, "flow".into(), &mut None, &mut None, None)
            .await;
        let mut out = Published { pmts: Vec::new(), pcr_pids: Vec::new(), video_packets: 0 };
        while let Ok(p) = brx.try_recv() {
            for pkt in p.data.chunks(188) {
                let pid = ts_pid(pkt);
                if pid == 0x0100 {
                    out.video_packets += 1;
                }
                if extract_pcr(pkt).is_some() && !out.pcr_pids.contains(&pid) {
                    out.pcr_pids.push(pid);
                }
                if pid == 0x1000 && ts_pusi(pkt) {
                    let sec = &pkt[5..];
                    let len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
                    let mut es = Vec::new();
                    let mut pos = 12;
                    while pos + 5 <= 3 + len - 4 {
                        es.push((((sec[pos + 1] & 0x1F) as u16) << 8) | sec[pos + 2] as u16);
                        pos += 5 + ((((sec[pos + 3] & 0x0F) as usize) << 8) | sec[pos + 4] as usize);
                    }
                    let pmt = ((sec[5] >> 1) & 0x1F, (((sec[8] & 0x1F) as u16) << 8) | sec[9] as u16, es);
                    if out.pmts.last() != Some(&pmt) {
                        out.pmts.push(pmt);
                    }
                }
            }
        }
        out
    }

    fn aac_config() -> RtmpMediaMessage {
        // AAC-LC, 48 kHz, stereo.
        RtmpMediaMessage::Audio { data: bytes::Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]), timestamp_ms: 0 }
    }

    fn aac_frames(n: u32, first_ms: u32) -> Vec<RtmpMediaMessage> {
        (0..n)
            .map(|i| {
                let mut data = vec![0xAF, 0x01];
                data.extend(std::iter::repeat_n(0x21, 300));
                RtmpMediaMessage::Audio { data: data.into(), timestamp_ms: first_ms + i * 21 }
            })
            .collect()
    }

    fn avc_config() -> RtmpMediaMessage {
        let mut data = vec![0x17, 0x00, 0, 0, 0, 1, 0x42, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x04];
        data.extend_from_slice(&[0x67, 0x42, 0x00, 0x1E]);
        data.extend_from_slice(&[0x01, 0x00, 0x04, 0x68, 0xCE, 0x3C, 0x80]);
        RtmpMediaMessage::Video { data: data.into(), timestamp_ms: 0 }
    }

    fn avc_idr(timestamp_ms: u32) -> RtmpMediaMessage {
        let mut nalu = vec![0x65, 0x88, 0x84];
        nalu.extend(std::iter::repeat_n(0x11, 400));
        let mut data = vec![0x17, 0x01, 0, 0, 0];
        data.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        data.extend_from_slice(&nalu);
        RtmpMediaMessage::Video { data: data.into(), timestamp_ms }
    }

    /// An audio-only publish (ffmpeg `-f flv` with no video writes no video
    /// property into `onMetaData`): the PMT lists the audio alone and names
    /// it as PCR_PID, and the audio carries the PCR. It used to name the
    /// absent video PID and carry no PCR at all.
    #[tokio::test]
    async fn an_audio_only_publish_carries_its_pcr_on_the_audio() {
        let mut msgs = vec![RtmpMediaMessage::Metadata { declares_video: Some(false) }, aac_config()];
        msgs.extend(aac_frames(10, 0));
        let out = publish(msgs).await;
        assert_eq!(out.pmts, vec![(0, 0x0101, vec![0x0101])]);
        assert_eq!(out.pcr_pids, vec![0x0101]);
    }

    /// With no `onMetaData` the publish is taken as audio-only once its
    /// audio has run a second alone (PMT version 1); a video tag after that
    /// moves it back (version 2, PCR on the video).
    #[tokio::test]
    async fn a_publish_without_metadata_follows_its_tags() {
        let mut msgs = vec![aac_config()];
        msgs.extend(aac_frames(60, 0));
        msgs.push(avc_config());
        msgs.push(avc_idr(1300));
        msgs.extend(aac_frames(2, 1300));
        let out = publish(msgs).await;
        assert_eq!(
            out.pmts,
            vec![
                (0, 0x0100, vec![0x0100, 0x0101]),
                (1, 0x0101, vec![0x0101]),
                (2, 0x0100, vec![0x0100, 0x0101]),
            ],
        );
        assert_eq!(out.pcr_pids, vec![0x0101, 0x0100]);
        assert!(out.video_packets > 0);
    }

    /// An A/V publish is unchanged: one PMT, PCR on the video.
    #[tokio::test]
    async fn an_av_publish_keeps_its_pcr_on_the_video() {
        let mut msgs =
            vec![RtmpMediaMessage::Metadata { declares_video: Some(true) }, avc_config(), aac_config()];
        msgs.push(avc_idr(0));
        msgs.extend(aac_frames(60, 0));
        msgs.push(avc_idr(1300));
        let out = publish(msgs).await;
        assert_eq!(out.pmts, vec![(0, 0x0100, vec![0x0100, 0x0101])]);
        assert_eq!(out.pcr_pids, vec![0x0100]);
    }

    /// Metadata cannot overrule the tags: once video has been sent, an
    /// `onMetaData` without it changes nothing, and once the audio ran on
    /// alone a later one declaring video does not either.
    #[test]
    fn the_tags_outrank_the_metadata() {
        let mut l = PublishLayout::default();
        assert_eq!(l.on_metadata(Some(false)), Some(false));
        assert!(l.on_video());
        assert_eq!(l.on_metadata(Some(false)), None);
        assert_eq!(l.on_audio(10 * AUDIO_ONLY_AFTER_90K), None, "video was seen");

        let mut l = PublishLayout::default();
        assert_eq!(l.on_audio(1_000), None);
        assert_eq!(l.on_audio(1_000 + AUDIO_ONLY_AFTER_90K - 1), None);
        assert_eq!(l.on_audio(1_000 + AUDIO_ONLY_AFTER_90K), Some(false));
        assert_eq!(l.on_metadata(Some(true)), None, "the audio ran on alone");
        assert!(l.on_video());
        assert_eq!(l.on_audio(1_000 + 3 * AUDIO_ONLY_AFTER_90K), None);
    }

    /// Regression for Bug B (2026-04-09): RTMP packets must carry a
    /// monotonically advancing wall-clock `recv_time_us` so the HLS output
    /// can compare consecutive packets and decide when to cut a segment.
    /// The previous implementation called `Instant::now().elapsed()` which
    /// returns the duration since the *just-constructed* Instant — i.e.
    /// approximately zero microseconds, every single call. With every
    /// packet stamped at ~0 µs the HLS segment-boundary check
    /// `elapsed_us >= segment_duration_us` never fired and zero segments
    /// were uploaded.
    #[test]
    fn wall_clock_micros_advances_between_calls() {
        let t1 = wall_clock_micros();
        // Sleep enough to be visibly larger than any plausible per-call
        // jitter in CI.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t2 = wall_clock_micros();
        assert!(
            t2 > t1 + 10_000,
            "wall_clock_micros() did not advance: t1={t1} t2={t2}"
        );
        // And the absolute value must look like a real Unix timestamp,
        // not a tiny number of microseconds since process start. Anything
        // after 2026-01-01 is fine.
        let jan_2026_us: u64 = 1_767_225_600_000_000;
        assert!(
            t1 > jan_2026_us,
            "wall_clock_micros() returned a non-Unix value: {t1}"
        );
    }
}
