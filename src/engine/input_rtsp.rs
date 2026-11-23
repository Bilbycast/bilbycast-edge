// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RTSP input — pulls H.264/AAC from RTSP sources (IP cameras, media servers).
//!
//! Uses the `retina` pure-Rust RTSP client for signaling and RTP reception.
//! Received H.264 NALUs and AAC frames are muxed into MPEG-TS and published
//! to the flow's broadcast channel, identical to other input types.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{RtspInputConfig, RtspTransport};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::InputTranscoder;
use super::packet::RtpPacket;
use super::rtmp::ts_mux::TsMuxer;

/// Spawn an RTSP input task.
///
/// Connects to the RTSP source, receives H.264 video (and optionally AAC audio),
/// muxes into MPEG-TS, and publishes `RtpPacket` to the broadcast channel.
/// Automatically reconnects on connection loss with configurable delay.
pub fn spawn_rtsp_input(
    config: RtspInputConfig,
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
        tracing::info!("RTSP input started, connecting to {}", shown_url(&config.rtsp_url));
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("RTSP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("RTSP input: transcode setup failed, passthrough: {e}");
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
        // Synthetic-TS input — pid_overrides handled by TsMuxer.
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
        // The muxer-mode rewriter's input-scoped Warnings
        // (`clock_rewrite_no_pcr` when the PCR_PID carries no PCR).
        if let Some(p) = post.as_mut() {
            p.set_event_sender(&event_sender, &input_id);
        }
        if let Some(ref _p) = post {
            tracing::info!(
                "RTSP input: ingress post-process active (passthrough_clock={passthrough_clock})"
            );
        }
        // Per-input ingress publisher (fixed-delay + de-jitter modes).
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            // RTSP (retina, RTP/TCP) is a locally-synthesised TS clock — no
            // media-rate ingress de-jitter.
            crate::engine::ingress_publisher::IngressBuffering {
                delay_ms: config.ingress_delay_ms,
                ..Default::default()
            },
            broadcast_tx,
            &input_id,
            cancel.clone(),
            stats.clone(),
        );
        rtsp_input_loop(config, publisher, stats, cancel, event_sender, flow_id, &mut transcoder, &mut post).await;
    })
}

async fn rtsp_input_loop(
    config: RtspInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    let reconnect_delay = std::time::Duration::from_secs(config.reconnect_delay_secs);
    // Track whether we need to emit a disconnect event. Set to true when
    // connected, cleared after emitting the disconnect event. This prevents
    // spamming the same warning on every reconnection attempt.
    let mut disconnect_event_pending = false;
    // The continuity counters the last session left each PID on: the next
    // session's muxer continues them (see [`session_muxer`]).
    let mut carried_cc = None;

    loop {
        match run_rtsp_session(&config, &publisher, &stats, &cancel, &event_sender, &flow_id, &mut disconnect_event_pending, &mut carried_cc, transcoder, post).await {
            Ok(()) => {
                tracing::info!("RTSP input stopped (cancelled)");
                break;
            }
            Err(e) => {
                tracing::warn!(
                    "RTSP connection lost: {}. Reconnecting in {}s...",
                    e,
                    config.reconnect_delay_secs
                );
                // Emit disconnect event once per connection cycle (not on every retry)
                if disconnect_event_pending {
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning, category::RTSP,
                        format!("RTSP input disconnected: {e}. Reconnecting in {}s", config.reconnect_delay_secs),
                        &flow_id,
                        serde_json::json!({
                            "url": shown_url(&config.rtsp_url),
                            "reconnect_delay_secs": config.reconnect_delay_secs,
                            "error": e.to_string(),
                        }),
                    );
                    disconnect_event_pending = false;
                }
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("RTSP input stopped during reconnect wait");
                        break;
                    }
                    _ = tokio::time::sleep(reconnect_delay) => continue,
                }
            }
        }
    }
}

async fn run_rtsp_session(
    config: &RtspInputConfig,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
    disconnect_event_pending: &mut bool,
    carried_cc: &mut Option<(u8, u8, u8, u8)>,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    use retina::codec::CodecItem;

    let (url, creds) = session_target(config)?;
    // Connect, DESCRIBE, SETUP and PLAY within `timeout_secs`: retina bounds
    // none of them, so a server that accepts the connection and never
    // answers held the input in DESCRIBE for good, with no retry.
    let mut step = "the connection or DESCRIBE";
    let budget = std::time::Duration::from_secs(config.timeout_secs);
    let opened = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        opened = tokio::time::timeout(budget, open_session(config, url.clone(), creds, &mut step)) => opened,
    };
    let Ok(opened) = opened else {
        anyhow::bail!("no answer to {step} within timeout_secs ({}s)", config.timeout_secs);
    };
    let (mut demuxed, layout) = opened?;

    tracing::info!("RTSP: connected and playing from {url}");
    event_sender.emit_flow_with_details(
        EventSeverity::Info, category::RTSP,
        format!("RTSP connected to {url}"), flow_id,
        serde_json::json!({
            "url": url.as_str(),
        }),
    );
    *disconnect_event_pending = true;

    let mut ts_muxer = session_muxer(config, layout, *carried_cc);
    let mut seq_num: u16 = 0;

    // Receive loop
    let ended: anyhow::Result<()> = loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            item = demuxed.next() => {
                match item {
                    Some(Ok(item)) => item,
                    Some(Err(e)) => break Err(e.into()),
                    None => break Err(anyhow::anyhow!("RTSP stream ended")),
                }
            }
        };

        match item {
            CodecItem::VideoFrame(frame) => {
                let is_keyframe = frame.is_random_access_point();
                let pts_90khz = rtsp_pts_90k(frame.timestamp().elapsed(), frame.timestamp().clock_rate().get());
                let data = frame.into_data();

                // FrameFormat::SIMPLE gives Annex B — TsMuxer expects this
                let ts_chunks = ts_muxer.mux_video(&data, pts_90khz, pts_90khz, is_keyframe);

                // Bundle all TS packets from this frame into a single RtpPacket
                // to reduce broadcast channel pressure (individual 188-byte packets
                // cause media analyzer lag and missed PAT/PMT detection)
                if !ts_chunks.is_empty() {
                    let total_len: usize = ts_chunks.iter().map(|c| c.len()).sum();
                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                    for chunk in &ts_chunks {
                        combined.extend_from_slice(chunk);
                    }
                    let pkt = RtpPacket {
                        data: combined.freeze(),
                        sequence_number: seq_num,
                        rtp_timestamp: pts_90khz as u32,
                        recv_time_us: crate::util::time::now_us(),
                        is_raw_ts: true,
                        upstream_seq: None,
                        upstream_leg_id: None,
                        sender_timestamp_us: None,
                    };
                    seq_num = seq_num.wrapping_add(1);
                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                    stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, publisher, pkt);
                    } else {
                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            CodecItem::AudioFrame(frame) => {
                let pts_90khz = rtsp_pts_90k(frame.timestamp().elapsed(), frame.timestamp().clock_rate().get());
                let data = frame.data();

                // retina with `FrameFormat::SIMPLE` (set above) returns
                // each AAC access unit already wrapped in a 7-byte ADTS
                // header that bakes in the correct sample_rate_idx and
                // channel_cfg from the SDP. Use `mux_audio_pre_adts` to
                // wrap it directly in a PES — `mux_audio` would call
                // `build_adts_frame` and double-wrap the bytes, which
                // produces a stream that every downstream AAC decoder
                // rejects with "channel element X.X is not allocated"
                // (Bug #3, 2026-04-09 test report).
                let ts_chunks = ts_muxer.mux_audio_pre_adts(data, pts_90khz);

                if !ts_chunks.is_empty() {
                    let total_len: usize = ts_chunks.iter().map(|c| c.len()).sum();
                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                    for chunk in &ts_chunks {
                        combined.extend_from_slice(chunk);
                    }
                    let pkt = RtpPacket {
                        data: combined.freeze(),
                        sequence_number: seq_num,
                        rtp_timestamp: pts_90khz as u32,
                        recv_time_us: crate::util::time::now_us(),
                        is_raw_ts: true,
                        upstream_seq: None,
                        upstream_leg_id: None,
                        sender_timestamp_us: None,
                    };
                    seq_num = seq_num.wrapping_add(1);
                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                    stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, publisher, pkt);
                    } else {
                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            _ => {} // SenderReport, MessageFrame, etc.
        }
    };
    *carried_cc = Some(ts_muxer.current_cc());
    ended
}

/// Connect, DESCRIBE, then SETUP and PLAY every H.264 / H.265 / AAC stream
/// the server describes. `step` names the request in flight (retina makes
/// the connection and DESCRIBE one call), so a caller that gives up on it
/// can say which one went unanswered.
async fn open_session(
    config: &RtspInputConfig,
    url: url::Url,
    creds: Option<retina::client::Credentials>,
    step: &mut &'static str,
) -> anyhow::Result<(retina::client::Demuxed, SessionLayout)> {
    use retina::client::{PlayOptions, SessionGroup, SetupOptions};
    use retina::codec::FrameFormat;

    let session_opts = retina::client::SessionOptions::default()
        .session_group(Arc::new(SessionGroup::default()))
        .creds(creds);
    let mut session = retina::client::Session::describe(url, session_opts).await?;

    // Select transport
    let transport = match config.transport {
        RtspTransport::Tcp => retina::client::Transport::Tcp(Default::default()),
        RtspTransport::Udp => retina::client::Transport::Udp(Default::default()),
    };

    // Discover streams and setup video/audio with Annex B framing
    let mut layout = SessionLayout::default();
    let mut streams_to_setup: Vec<(usize, &str)> = Vec::new();

    for i in 0..session.streams().len() {
        let stream = &session.streams()[i];
        let media = stream.media();
        let encoding = stream.encoding_name();

        if media == "video" && encoding.eq_ignore_ascii_case("h264") {
            streams_to_setup.push((i, "video"));
        } else if media == "video" && encoding.eq_ignore_ascii_case("h265") {
            streams_to_setup.push((i, "video"));
            layout.is_h265 = true;
        } else if media == "audio"
            && (encoding.eq_ignore_ascii_case("mpeg4-generic")
                || encoding.eq_ignore_ascii_case("aac"))
        {
            streams_to_setup.push((i, "audio"));
        }
    }

    *step = "SETUP";
    for (i, kind) in &streams_to_setup {
        let opts = SetupOptions::default()
            .transport(transport.clone())
            .frame_format(FrameFormat::SIMPLE);
        session.setup(*i, opts).await?;
        if *kind == "video" {
            layout.has_video = true;
        } else if *kind == "audio" {
            layout.has_audio = true;
        }
        tracing::info!("RTSP: setup {} stream {}", kind, i);
    }

    *step = "PLAY";
    let demuxed = session
        .play(PlayOptions::default())
        .await?
        .demuxed()?;
    Ok((demuxed, layout))
}

/// What a session set up, which lays out its TS.
#[derive(Clone, Copy, Debug, Default)]
struct SessionLayout {
    has_video: bool,
    has_audio: bool,
    is_h265: bool,
}

/// The muxer for one session, continuing the continuity counters the
/// previous session of this input left each PID on (`carried_cc`, from
/// [`TsMuxer::current_cc`]).
///
/// Every reconnect builds a fresh muxer, while what follows it — the
/// post-process chain, the flow and every output — carries on: started at
/// CC 0, each PID's first packet broke its sequence with no
/// `discontinuity_indicator`, which the flow's own TR 101 290 counted as a
/// `cc_errors` and a strict receiver reads, one reconnect in sixteen, as a
/// duplicate it drops (ISO/IEC 13818-1 §2.4.3.3). No packet of a PID is
/// lost across a reconnect — each frame is muxed whole — so the sequence
/// carries straight on, as the media player's does across a file splice
/// (`SpliceContinuity`). The new session's timestamps restart, which a
/// PCR's DI announces: the muxer-mode rewriter's on the step itself, or —
/// with `passthrough_clock` — the flow's source-discontinuity watch's, on
/// the PCR after it.
fn session_muxer(
    config: &RtspInputConfig,
    layout: SessionLayout,
    carried_cc: Option<(u8, u8, u8, u8)>,
) -> TsMuxer {
    let mut ts_muxer = TsMuxer::new();
    if let Some(po) = config.pid_overrides.as_ref()
        && let Some(entry) = po.get(&1) {
                ts_muxer.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
    ts_muxer.set_has_video(layout.has_video);
    ts_muxer.set_has_audio(layout.has_audio);
    if layout.is_h265 {
        ts_muxer.set_video_stream_type(super::rtmp::ts_mux::STREAM_TYPE_H265);
    }
    if let Some((pat, pmt, video, audio)) = carried_cc {
        ts_muxer.seed_cc(pat, pmt, video, audio);
    }
    ts_muxer
}

/// The URL a session connects to and the credentials it presents.
///
/// retina refuses a URL that carries credentials ("URL must not contain
/// credentials"), and an input given the form cameras document,
/// `rtsp://user:pass@camera/…`, retried every `reconnect_delay_secs` for
/// good. The userinfo is lifted out of the URL — percent-decoded, as a URL
/// carries it — into the credentials, unless `username` and `password` are
/// both set, which win.
fn session_target(
    config: &RtspInputConfig,
) -> anyhow::Result<(url::Url, Option<retina::client::Credentials>)> {
    let mut url = url::Url::parse(&config.rtsp_url)?;
    let embedded = (!url.username().is_empty() || url.password().is_some()).then(|| {
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        retina::client::Credentials {
            username: decode(url.username()),
            password: decode(url.password().unwrap_or_default()),
        }
    });
    strip_userinfo(&mut url);
    let creds = match (&config.username, &config.password) {
        (Some(username), Some(password)) => Some(retina::client::Credentials {
            username: username.clone(),
            password: password.clone(),
        }),
        _ => embedded,
    };
    Ok((url, creds))
}

fn strip_userinfo(url: &mut url::Url) {
    // Both fail only for a URL that cannot carry userinfo at all.
    let _ = url.set_username("");
    let _ = url.set_password(None);
}

/// `rtsp_url` as a log line, an event, the stats or the health may show it:
/// without its userinfo, which is a camera's password as often as not. A
/// URL that does not parse loses everything from `://` to its last `@`.
pub(crate) fn shown_url(rtsp_url: &str) -> String {
    match url::Url::parse(rtsp_url) {
        Ok(mut url) => {
            strip_userinfo(&mut url);
            url.into()
        }
        Err(_) => match (rtsp_url.find("://"), rtsp_url.rfind('@')) {
            (Some(scheme_end), Some(at)) if at > scheme_end => {
                format!("{}{}", &rtsp_url[..scheme_end + 3], &rtsp_url[at + 1..])
            }
            _ => rtsp_url.to_string(),
        },
    }
}

/// A retina frame's PES timestamp (90 kHz): its time since the stream's
/// start, from the muxer's PCR lead — the PCR runs that far behind the
/// timestamps, so from 0 the first one sat just below the 33-bit wrap.
///
/// `elapsed` is in the stream's RTP clock (`clock_rate` Hz), not 90 kHz:
/// video runs at 90 kHz, but AAC (RFC 3640) runs at its sample rate. Taken
/// as 90 kHz, 48 kHz AAC advanced 0.533 s of PTS a second — 4.7 s behind
/// the video after 10 s, and an audio-only session's PCR (derived from
/// those PTS) ran at 0.53x the wall clock. With more than one stream set
/// up retina requires the PLAY response's `RTP-Info` `rtptime` for every
/// stream (`InitialTimestampPolicy::Default`), so each stream's start is
/// the same NPT and the streams stay aligned.
fn rtsp_pts_90k(elapsed: i64, clock_rate: u32) -> u64 {
    let ticks = elapsed.max(0) as u128 * 90_000 / u128::from(clock_rate.max(1));
    (super::rtmp::ts_mux::PCR_LEAD_90K + ticks as u64) & 0x1_FFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::rtmp::ts_mux::PCR_LEAD_90K;
    use bytes::Bytes;

    fn config(extra: serde_json::Value) -> RtspInputConfig {
        let mut v = serde_json::json!({"rtsp_url": "rtsp://camera.local:554/live?channel=1"});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).expect("RTSP input config")
    }

    /// What a log line, an event or the stats show of the URL keeps
    /// everything but its userinfo — also when the URL does not parse (an
    /// unencoded `/` in the password ends the authority early).
    #[test]
    fn a_shown_url_never_carries_its_userinfo() {
        assert_eq!(
            shown_url("rtsp://admin:s3cret@camera.local:554/live?channel=1"),
            "rtsp://camera.local:554/live?channel=1"
        );
        assert_eq!(shown_url("rtsp://admin@camera.local/live"), "rtsp://camera.local/live");
        assert_eq!(shown_url("rtsp://admin:s3/cret@camera.local/live"), "rtsp://camera.local/live");
        assert_eq!(shown_url("rtsp://camera.local:554/live"), "rtsp://camera.local:554/live");
    }

    /// retina refuses a URL carrying credentials: the userinfo is lifted
    /// into the session's credentials, percent-decoded, and the URL it
    /// connects to carries none. `username` + `password` win over it.
    #[test]
    fn url_credentials_are_lifted_out_of_the_url() {
        let (url, creds) = session_target(&config(serde_json::json!({
            "rtsp_url": "rtsp://ad%40min:p%3Ass%2Fw@camera.local:554/live?channel=1"
        })))
        .unwrap();
        assert_eq!(url.as_str(), "rtsp://camera.local:554/live?channel=1");
        let creds = creds.expect("credentials from the URL");
        assert_eq!((creds.username.as_str(), creds.password.as_str()), ("ad@min", "p:ss/w"));

        let (url, creds) = session_target(&config(serde_json::json!({
            "rtsp_url": "rtsp://old:stale@camera.local/live",
            "username": "admin", "password": "s3cret"
        })))
        .unwrap();
        assert_eq!(url.as_str(), "rtsp://camera.local/live");
        let creds = creds.expect("the fields' credentials");
        assert_eq!((creds.username.as_str(), creds.password.as_str()), ("admin", "s3cret"));

        let (_, creds) = session_target(&config(serde_json::json!({}))).unwrap();
        assert!(creds.is_none());
    }

    /// A reconnect's fresh muxer continues every PID's continuity counter
    /// where the last session left it: no CC break for TR 101 290 to count,
    /// and none a strict receiver reads as a duplicate.
    #[test]
    fn a_reconnect_continues_every_pids_continuity_counter() {
        use crate::engine::ts_parse::{ts_cc, ts_has_payload, ts_pid};
        let cfg = config(serde_json::json!({}));
        let layout = SessionLayout { has_video: true, has_audio: true, is_h265: false };
        let idr = [0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33];
        let adts = [0xFFu8, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC, 0x21, 0x00];
        let session = |muxer: &mut TsMuxer, frames: u64| -> Vec<Bytes> {
            let mut out = Vec::new();
            for f in 0..frames {
                let pts = PCR_LEAD_90K + f * 3_600;
                out.extend(muxer.mux_video(&idr, pts, pts, f == 0));
                out.extend(muxer.mux_audio_pre_adts(&adts, pts));
            }
            out
        };

        // 7 frames: the CCs end mid-sequence, nowhere near 0.
        let mut first = session_muxer(&cfg, layout, None);
        let mut packets = session(&mut first, 7);
        let mut second = session_muxer(&cfg, layout, Some(first.current_cc()));
        packets.extend(session(&mut second, 3));

        let mut last: std::collections::HashMap<u16, u8> = Default::default();
        for chunk in &packets {
            for pkt in chunk.chunks(188) {
                let (pid, cc) = (ts_pid(pkt), ts_cc(pkt));
                if let Some(prev) = last.get(&pid) {
                    let want = if ts_has_payload(pkt) { (prev + 1) & 0x0F } else { *prev };
                    assert_eq!(cc, want, "PID 0x{pid:X}: CC {prev} then {cc}");
                }
                last.insert(pid, cc);
            }
        }
        assert_eq!(last.len(), 4, "PAT, PMT, video and audio all checked: {last:?}");
    }

    /// A server that takes the connection and never answers DESCRIBE is
    /// given up on after `timeout_secs`, and the input retries after
    /// `reconnect_delay_secs` as it does for any lost session. retina bounds
    /// none of the setup requests: the input sat in DESCRIBE for good.
    #[tokio::test]
    async fn an_unanswered_describe_times_out_and_retries() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let cfg = config(serde_json::json!({
            "rtsp_url": format!("rtsp://{addr}/live"),
            "timeout_secs": 1,
            "reconnect_delay_secs": 1
        }));
        let (tx, _rx) = broadcast::channel(16);
        let stats = Arc::new(FlowStatsAccumulator::new("f".into(), "flow".into(), "rtsp".into()));
        let cancel = CancellationToken::new();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let input = spawn_rtsp_input(
            cfg,
            tx,
            stats,
            cancel.clone(),
            events,
            "f".into(),
            "in".into(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        );

        let started = tokio::time::Instant::now();
        let (_held, _) = tokio::time::timeout(std::time::Duration::from_secs(5), server.accept())
            .await
            .expect("the input connects")
            .unwrap();
        let retried = tokio::time::timeout(std::time::Duration::from_secs(6), server.accept()).await;
        assert!(retried.is_ok(), "no second attempt within 6 s: the first DESCRIBE was never given up");
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(1_900),
            "retried after {waited:?}: before the 1 s timeout and the 1 s back-off"
        );

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), input)
            .await
            .expect("a cancel ends the input mid-setup")
            .unwrap();
    }

    /// A second of any stream is 90 000 ticks of PTS, whatever its RTP
    /// clock: 48 kHz AAC advanced 48 000 (0.533 s a second) when its clock
    /// was taken for 90 kHz.
    #[test]
    fn a_second_of_any_rtp_clock_is_90000_ticks() {
        for rate in [90_000u32, 48_000, 44_100, 16_000, 8_000] {
            assert_eq!(rtsp_pts_90k(0, rate), PCR_LEAD_90K, "{rate} Hz");
            assert_eq!(rtsp_pts_90k(i64::from(rate), rate), PCR_LEAD_90K + 90_000, "{rate} Hz");
            assert_eq!(rtsp_pts_90k(i64::from(rate) * 3_600, rate), PCR_LEAD_90K + 3_600 * 90_000, "{rate} Hz");
        }
        // 1024 samples of 48 kHz AAC are 1920 ticks.
        assert_eq!(rtsp_pts_90k(1_024, 48_000), PCR_LEAD_90K + 1_920);
        assert_eq!(rtsp_pts_90k(-5, 48_000), PCR_LEAD_90K, "before the start: the start");
    }
}
