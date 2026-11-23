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
use crate::manager::events::{Event, EventSender, EventSeverity, category};
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
        rtsp_input_loop(config, publisher, stats, cancel, event_sender, flow_id, input_id, &mut transcoder, &mut post).await;
    })
}

async fn rtsp_input_loop(
    config: RtspInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    // A URL that can never be played — `rtsps://`, or one url::Url cannot
    // read — fails the input once, rather than every
    // `reconnect_delay_secs` for good. The edits that name an input refuse
    // such a URL; a whole config keeps it with a Warning
    // (`repair_rtsp_inputs`), so a node holding one still starts. A config
    // change respawns the input, which judges it afresh.
    if let Some(problem) = RtspUrlProblem::of(&config.rtsp_url).filter(|p| p.is_fatal()) {
        let message = format!(
            "RTSP input '{input_id}' cannot connect: {}. It is not retried until its \
             configuration changes",
            problem.describe()
        );
        tracing::error!("{message}");
        event_sender.send(Event {
            severity: EventSeverity::Critical,
            category: category::RTSP.to_string(),
            message,
            details: Some(serde_json::json!({
                "error_code": problem.error_code(),
                "url": shown_url(&config.rtsp_url),
            })),
            flow_id: Some(flow_id),
            input_id: Some(input_id),
            output_id: None,
        });
        return;
    }
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
                // An error may quote the URL it was given: it is shown
                // without what [`shown_url`] hides, as the URL is.
                let e = scrub(&e.to_string(), &config.rtsp_url);
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
                            "error": e,
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
        opened = tokio::time::timeout(budget, open_session(config, url, creds, &mut step)) => opened,
    };
    let Ok(opened) = opened else {
        anyhow::bail!("no answer to {step} within timeout_secs ({}s)", config.timeout_secs);
    };
    let (mut demuxed, layout) = opened?;

    // The URL as configured, cut by [`shown_url`] — not `url`, which
    // url::Url may have read a misplaced password into the path, query or
    // fragment of.
    let shown = shown_url(&config.rtsp_url);
    tracing::info!("RTSP: connected and playing from {shown}");
    event_sender.emit_flow_with_details(
        EventSeverity::Info, category::RTSP,
        format!("RTSP connected to {shown}"), flow_id,
        serde_json::json!({
            "url": shown,
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

/// The byte range of `rtsp_url` that [`shown_url`] and [`scrub`] hide:
/// from just past its `://` (or its start, when it has none before the `@`)
/// to its last `@`.
///
/// Found in the text, not by url::Url: an unencoded `#`, `/` or `?` in a
/// password ends the authority early for the parser, which then reads
/// `rtsp://admin:1234#5@cam/live` as host `admin`, port 1234 and no userinfo
/// at all — and a redaction that cleared the parsed userinfo printed the
/// password whole. Everything a URL can carry as userinfo, however it is
/// written, lies before its last `@`. An `@` in a path or query hides the
/// host and path ahead of it as well: hiding a host is the safe error.
fn secret_span(rtsp_url: &str) -> Option<std::ops::Range<usize>> {
    let at = rtsp_url.rfind('@')?;
    let start = match rtsp_url.find("://") {
        Some(i) if i + 3 <= at => i + 3,
        _ => 0,
    };
    Some(start..at)
}

/// `rtsp_url` as a log line, an event, the stats or the health may show it:
/// without its userinfo, which is a camera's password as often as not —
/// everything between its `://` and its last `@` is left out
/// ([`secret_span`]).
pub(crate) fn shown_url(rtsp_url: &str) -> String {
    match secret_span(rtsp_url) {
        Some(span) => format!("{}{}", &rtsp_url[..span.start], &rtsp_url[span.end + 1..]),
        None => rtsp_url.to_string(),
    }
}

/// `text` — an error's, which may quote the URL it was given — with the
/// part of `rtsp_url` [`shown_url`] hides taken out wherever it appears.
fn scrub(text: &str, rtsp_url: &str) -> String {
    match secret_span(rtsp_url).map(|span| &rtsp_url[span]) {
        Some(secret) if !secret.is_empty() => text.replace(secret, ""),
        _ => text.to_string(),
    }
}

/// What stops an `rtsp_url` being played as it is written.
///
/// The edits that name an input refuse each
/// ([`crate::config::validation::validate_input_edit`]); a whole config — the
/// load, the reload, `PUT /api/v1/config`, the WS `update_config` — keeps
/// the input and warns of it
/// ([`crate::config::validation::repair_rtsp_inputs`]), since refusing it
/// there stopped a node at boot, and a node that will not start has no
/// manager WebSocket to be fixed over. Such an input that can never connect
/// ([`Self::is_fatal`]) then fails once when it starts, rather than every
/// `reconnect_delay_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspUrlProblem {
    /// `rtsps://`: the RTSP client (retina) speaks no TLS.
    Rtsps,
    /// url::Url cannot read it at all — `rtsp://admin:pa#ss@cam/live`
    /// is read as port `pa`.
    Unparseable(url::ParseError),
    /// url::Url reads it, but not the text before its last `@` as the
    /// username and password that text spells — `rtsp://admin:1234#5@cam/live`
    /// is host `admin`, port 1234 and fragment `5@cam/live`. The same text
    /// as an `@` in a path or query (`rtsp://cam/live?x@y`), which no parser
    /// can tell from it.
    UserinfoMisread,
}

impl RtspUrlProblem {
    /// The problem with `rtsp_url`, if it has one.
    pub fn of(rtsp_url: &str) -> Option<Self> {
        if rtsp_url.starts_with("rtsps://") {
            return Some(Self::Rtsps);
        }
        let url = match url::Url::parse(rtsp_url) {
            Ok(url) => url,
            Err(e) => return Some(Self::Unparseable(e)),
        };
        let written = &rtsp_url[secret_span(rtsp_url)?];
        let (user, pass) = written.split_once(':').unwrap_or((written, ""));
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        let read_as_written =
            decode(url.username()) == decode(user) && decode(url.password().unwrap_or_default()) == decode(pass);
        (!read_as_written).then_some(Self::UserinfoMisread)
    }

    /// Why, for a refusal, a log line or an event. Never quotes the URL.
    pub fn describe(self) -> String {
        const ENCODE: &str = "percent-encode a '#', '/', '?' or '@' in a username or password \
                              (%23, %2F, %3F, %40), or put them in the username and password fields";
        match self {
            Self::Rtsps => "rtsp_url: rtsps:// (RTSP over TLS) is not supported; use rtsp://".to_string(),
            Self::Unparseable(e) => format!("rtsp_url is not a URL the RTSP client can read ({e}): {ENCODE}"),
            Self::UserinfoMisread => format!(
                "rtsp_url: the text before its last '@' is not read as the username and password it \
                 spells: {ENCODE}; an '@' in the path or query reads the same way, so encode it as %40"
            ),
        }
    }

    /// `details.error_code` of the events that report it.
    pub fn error_code(self) -> &'static str {
        match self {
            Self::Rtsps => "rtsps_unsupported",
            Self::Unparseable(_) => "rtsp_url_unparseable",
            Self::UserinfoMisread => "rtsp_userinfo_misread",
        }
    }

    /// Whether an input with this URL can never connect. One whose userinfo
    /// is misread still connects to what url::Url reads, which for an `@` in
    /// a path or query is what was meant.
    pub fn is_fatal(self) -> bool {
        !matches!(self, Self::UserinfoMisread)
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

    /// What a log line, an event or the stats show of the URL leaves out
    /// everything between its `://` and its last `@`, whether or not
    /// url::Url reads that as userinfo. A digits-first password with an
    /// unencoded `#`, `/` or `?` parses, as host `admin` and port 1234 with
    /// no userinfo at all, and a redaction that cleared the parsed userinfo
    /// printed it whole; one that does not parse (`s3/cret`) is cut too.
    #[test]
    fn a_shown_url_never_carries_its_userinfo() {
        for (url, shown, secret) in [
            ("rtsp://admin:1234#5@cam/live", "rtsp://cam/live", "1234#5"),
            ("rtsp://admin:1234/x@cam.local/live", "rtsp://cam.local/live", "1234/x"),
            ("rtsp://admin:5555?q@cam.local/live", "rtsp://cam.local/live", "5555?q"),
            ("rtsp://admin:s3/cret@camera.local/live", "rtsp://camera.local/live", "s3/cret"),
            (
                "rtsp://admin:s3cret@camera.local:554/live?channel=1",
                "rtsp://camera.local:554/live?channel=1",
                "s3cret",
            ),
            ("rtsp://ad%40min:p%3Ass@camera.local/live", "rtsp://camera.local/live", "p%3Ass"),
            ("rtsp://ad@min:pw1@camera.local/live", "rtsp://camera.local/live", "pw1"),
            ("rtsp://admin@camera.local/live", "rtsp://camera.local/live", "admin"),
            ("rtsps://admin:hunter2@camera.local/s", "rtsps://camera.local/s", "hunter2"),
            ("admin:hunter2@camera.local/s", "camera.local/s", "hunter2"),
        ] {
            assert_eq!(shown_url(url), shown, "{url}");
            assert!(!shown_url(url).contains(secret), "{url}");
        }
        // No `@`, nothing hidden.
        assert_eq!(shown_url("rtsp://camera.local:554/live"), "rtsp://camera.local:554/live");
        // An `@` in a path or query reads the same as a misplaced password:
        // the host ahead of it is hidden too, the safe way to be wrong.
        assert_eq!(shown_url("rtsp://camera.local/live?token=ab@cd"), "rtsp://cd");
    }

    /// An error that quotes the URL loses what [`shown_url`] hides.
    #[test]
    fn an_error_is_scrubbed_of_the_userinfo() {
        let url = "rtsp://admin:1234#5@cam/live";
        assert_eq!(scrub("Must specify host in rtsp url rtsp://admin:1234#5@cam/live", url), "Must specify host in rtsp url rtsp://@cam/live");
        assert_eq!(scrub("RTSP stream ended", url), "RTSP stream ended");
        assert_eq!(scrub("RTSP stream ended", "rtsp://cam/live"), "RTSP stream ended");
    }

    /// The URLs an input cannot play as written, and why — never quoting
    /// the URL — and those it can, percent-encoded userinfo, an unencoded
    /// `@` in the username and a `:` in the password included (url::Url
    /// reads those as written).
    #[test]
    fn an_rtsp_url_problem_is_found_in_the_text() {
        use RtspUrlProblem::*;
        for (url, want) in [
            ("rtsps://admin:hunter2@camera.local/s", Some(Rtsps)),
            ("rtsps://camera.local/s", Some(Rtsps)),
            ("rtsp://admin:pa#ss@cam/live", Some(Unparseable(url::ParseError::InvalidPort))),
            ("rtsp://admin:1234#5@cam/live", Some(UserinfoMisread)),
            ("rtsp://admin:1234/x@cam/live", Some(UserinfoMisread)),
            ("rtsp://admin:5555?q@cam/live", Some(UserinfoMisread)),
            ("rtsp://cam/live?token=ab@cd", Some(UserinfoMisread)),
            ("rtsp://camera.local:554/live", None),
            ("rtsp://admin:s3cret@camera.local:554/live", None),
            ("rtsp://admin@camera.local/live", None),
            ("rtsp://ad%40min:p%23ss%2Fw%3F@camera.local/live", None),
            ("rtsp://ad@min:pw@camera.local/live", None),
            ("rtsp://admin:a:b@camera.local/live", None),
            ("rtsp://admin:@camera.local/live", None),
            ("rtsp://admin:pw@[::1]:554/live", None),
        ] {
            let got = RtspUrlProblem::of(url);
            assert_eq!(got, want, "{url}");
            if let Some(problem) = got {
                let said = problem.describe();
                assert!(!said.contains(&url["rtsp://".len()..]) && !said.contains("hunter2"), "{said}");
            }
        }
        assert!(Rtsps.is_fatal() && Unparseable(url::ParseError::InvalidPort).is_fatal());
        assert!(!UserinfoMisread.is_fatal(), "it connects to what url::Url reads");
    }

    /// An input whose URL can never be played fails once — one Critical
    /// event naming the flow and the input, with an error code and the
    /// redacted URL — and stops, rather than retrying every
    /// `reconnect_delay_secs` for good. It never dials.
    #[tokio::test]
    async fn an_unplayable_url_fails_the_input_once() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        for (url, code) in [
            (format!("rtsps://cam:hunter2@{addr}/live"), "rtsps_unsupported"),
            (format!("rtsp://cam:hun#ter2@{addr}/live"), "rtsp_url_unparseable"),
        ] {
            let cfg = config(serde_json::json!({ "rtsp_url": url, "reconnect_delay_secs": 1 }));
            let (tx, _rx) = broadcast::channel(16);
            let stats = Arc::new(FlowStatsAccumulator::new("f".into(), "flow".into(), "rtsp".into()));
            let (events, mut raised) = crate::manager::events::event_channel();
            let input = spawn_rtsp_input(
                cfg, tx, stats, CancellationToken::new(), events, "f".into(), "in".into(),
                Arc::new(std::sync::atomic::AtomicBool::new(false)), None,
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), input)
                .await
                .expect("the input ends without a cancel")
                .unwrap();
            let raised: Vec<_> = std::iter::from_fn(|| raised.try_recv().ok()).collect();
            assert_eq!(raised.len(), 1, "{url}: {raised:?}");
            let event = &raised[0];
            assert_eq!(event.severity, EventSeverity::Critical);
            assert_eq!(event.category, "rtsp");
            assert_eq!((event.flow_id.as_deref(), event.input_id.as_deref()), (Some("f"), Some("in")));
            let details = event.details.as_ref().unwrap();
            assert_eq!(details["error_code"], code);
            assert_eq!(details["url"], format!("{}{addr}/live", if code == "rtsps_unsupported" { "rtsps://" } else { "rtsp://" }));
            assert!(!event.message.contains("hunter2") && !event.message.contains("ter2"), "{}", event.message);
            assert!(event.message.contains("not retried until its configuration changes"), "{}", event.message);
        }
        let dialled = tokio::time::timeout(std::time::Duration::from_millis(200), server.accept()).await;
        assert!(dialled.is_err(), "an unplayable URL is never dialled");
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

    /// [`session_muxer`] continues every PID's continuity counter from the
    /// counters it is handed, audio included (the camera of
    /// `a_reconnect_continues_every_pids_continuity_counter` sends none).
    #[test]
    fn a_session_muxer_continues_the_carried_counters() {
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

    /// One request a client sent: its request line and `CSeq`. Interleaved
    /// data the client sends between requests (RTCP) is skipped. `None`
    /// when the client hangs up.
    async fn read_request(conn: &mut tokio::net::TcpStream, buf: &mut Vec<u8>) -> Option<(String, String)> {
        use tokio::io::AsyncReadExt;
        loop {
            if buf.first() == Some(&b'$') {
                if buf.len() >= 4 {
                    let len = 4 + usize::from(u16::from_be_bytes([buf[2], buf[3]]));
                    if buf.len() >= len {
                        buf.drain(..len);
                        continue;
                    }
                }
            } else if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..end + 4);
                let cseq = head
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.trim().eq_ignore_ascii_case("cseq"))
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default();
                return Some((head.lines().next()?.to_string(), cseq));
            }
            let mut chunk = [0u8; 4096];
            match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// A camera: each of `sessions` connections answers DESCRIBE, SETUP and
    /// PLAY for one H.264 track over interleaved TCP, sends `frames` access
    /// units — an IDR, then non-IDR slices — and hangs up. Returns every
    /// request line it was sent.
    async fn camera(server: tokio::net::TcpListener, sessions: usize, frames: u16) -> Vec<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let base = format!("rtsp://{}/live/", server.local_addr().unwrap());
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=camera\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                   a=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
                   a=fmtp:96 packetization-mode=1;profile-level-id=64001E;\
                   sprop-parameter-sets=Z2QAHqwsaoLA9puCgIKgAAADACAAAAMD0IAA,aO4xshsA\r\n\
                   a=control:trackID=0\r\n";
        let mut requests = Vec::new();
        for _ in 0..sessions {
            let (mut conn, _) = server.accept().await.unwrap();
            let mut buf = Vec::new();
            while let Some((line, cseq)) = read_request(&mut conn, &mut buf).await {
                requests.push(line.clone());
                let method = line.split(' ').next().unwrap_or_default().to_string();
                let (headers, body) = match method.as_str() {
                    "DESCRIBE" => (
                        format!(
                            "Content-Base: {base}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n",
                            sdp.len()
                        ),
                        sdp,
                    ),
                    "SETUP" => ("Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: 5EED;timeout=60\r\n".into(), ""),
                    "PLAY" => (format!("Session: 5EED\r\nRTP-Info: url={base}trackID=0;seq=0;rtptime=0\r\n"), ""),
                    _ => ("Session: 5EED\r\n".into(), ""),
                };
                let answer = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n{headers}\r\n{body}");
                conn.write_all(answer.as_bytes()).await.unwrap();
                if method != "PLAY" {
                    continue;
                }
                for seq in 0..frames {
                    let nal: &[u8] = if seq == 0 { &[0x65, 0x88, 0x84, 0x00, 0x33] } else { &[0x41, 0x9A, 0x00, 0x10] };
                    let mut rtp = vec![0x80, 0x80 | 96];
                    rtp.extend_from_slice(&seq.to_be_bytes());
                    rtp.extend_from_slice(&(u32::from(seq) * 3_000).to_be_bytes());
                    rtp.extend_from_slice(&0x5EED_u32.to_be_bytes());
                    rtp.extend_from_slice(nal);
                    let mut interleaved = vec![b'$', 0];
                    interleaved.extend_from_slice(&u16::try_from(rtp.len()).unwrap().to_be_bytes());
                    interleaved.extend_from_slice(&rtp);
                    conn.write_all(&interleaved).await.unwrap();
                }
                // Hang up after the last frame: a FIN, then whatever the
                // client still sends, so no RST takes the frames with it.
                let _ = conn.shutdown().await;
                let mut sink = [0u8; 1024];
                let drain = async { while matches!(conn.read(&mut sink).await, Ok(n) if n > 0) {} };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), drain).await;
                break;
            }
        }
        requests
    }

    /// What one input did against [`camera`].
    struct CameraRun {
        config: RtspInputConfig,
        events: Vec<crate::manager::events::Event>,
        packets: Vec<RtpPacket>,
        requests: Vec<String>,
    }

    /// Run an input against a [`camera`] serving `sessions` sessions of
    /// five frames each, until it has reported the last hang-up.
    async fn run_against_camera(rtsp_url: impl Fn(std::net::SocketAddr) -> String, sessions: usize) -> CameraRun {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = config(serde_json::json!({
            "rtsp_url": rtsp_url(server.local_addr().unwrap()),
            "timeout_secs": 5,
            "reconnect_delay_secs": 1
        }));
        let camera = tokio::spawn(camera(server, sessions, 5));
        let (tx, mut rx) = broadcast::channel(1024);
        let stats = Arc::new(FlowStatsAccumulator::new("f".into(), "flow".into(), "rtsp".into()));
        let cancel = CancellationToken::new();
        let (events, mut raised) = crate::manager::events::event_channel();
        let input = spawn_rtsp_input(
            cfg.clone(), tx, stats, cancel.clone(), events, "f".into(), "in".into(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)), None,
        );
        let requests = tokio::time::timeout(std::time::Duration::from_secs(10), camera)
            .await
            .expect("the camera served every session")
            .unwrap();
        let mut events = Vec::new();
        let hung_up = |events: &[crate::manager::events::Event]| {
            events.iter().filter(|e| e.message.starts_with("RTSP input disconnected")).count()
        };
        while hung_up(&events) < sessions {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), raised.recv())
                .await
                .expect("the input reports each hang-up")
                .expect("the event channel stays open");
            events.push(event);
        }
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), input).await.expect("a cancel ends the input").unwrap();
        events.extend(std::iter::from_fn(|| raised.try_recv().ok()));
        let packets = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        CameraRun { config: cfg, events, packets, requests }
    }

    /// The session loop carries every PID's continuity counter from one
    /// session to the next: across a camera's hang-up and the reconnect,
    /// what the input publishes has no CC break for TR 101 290 to count
    /// and none a strict receiver reads as a duplicate. Each session's
    /// fresh muxer used to start every PID at 0.
    #[tokio::test]
    async fn a_reconnect_continues_every_pids_continuity_counter() {
        use crate::engine::ts_parse::{ts_cc, ts_has_payload, ts_pid, ts_pusi};
        let run = run_against_camera(|addr| format!("rtsp://{addr}/live"), 2).await;
        let describes = run.requests.iter().filter(|r| r.starts_with("DESCRIBE")).count();
        assert_eq!(describes, 2, "two sessions: {:?}", run.requests);

        let mut last: std::collections::HashMap<u16, u8> = Default::default();
        let mut video_pes = 0;
        for pkt in run.packets.iter().flat_map(|p| p.data.chunks(188)) {
            let (pid, cc) = (ts_pid(pkt), ts_cc(pkt));
            if pid == 0x100 && ts_pusi(pkt) {
                video_pes += 1;
            }
            if let Some(prev) = last.get(&pid) {
                let want = if ts_has_payload(pkt) { (prev + 1) & 0x0F } else { *prev };
                assert_eq!(cc, want, "PID 0x{pid:X}: CC {prev} then {cc}");
            }
            last.insert(pid, cc);
        }
        assert_eq!(video_pes, 10, "both sessions' five frames published");
        assert!(last.contains_key(&0) && last.len() >= 3, "PAT, PMT and video all checked: {last:?}");
    }

    /// Every place the input shows its URL — the start log line, the
    /// connected and lost log lines, the connected and disconnected events'
    /// message and `details.url`, and the stats' `rtsp_url` — goes through
    /// [`shown_url`]. The URL carries a password and an `@` in its query,
    /// where [`shown_url`] (everything from `://` to the last `@` hidden)
    /// and url::Url's userinfo differ, so each site is seen to use the one
    /// function rather than a parse of its own. And the URL the client
    /// connects to carries no userinfo.
    #[tokio::test]
    async fn every_url_the_input_shows_goes_through_the_redaction() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = || {
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(CaptureWriter(captured.clone()))
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .finish(),
            )
        };
        let url = |addr: std::net::SocketAddr| format!("rtsp://admin:s3cret@{addr}/live?token=ab@cd");
        // Run twice and measure the second: `tracing` caches a callsite's
        // interest the first time it is hit, and one first hit by a test
        // running alongside with no subscriber can cache "never". See the
        // RTMP server's `the_stream_key_never_reaches_the_log_at_any_level`.
        let warm = subscriber();
        run_against_camera(url, 2).await;
        drop(warm);
        let _guard = subscriber();
        captured.lock().unwrap().clear();
        let run = run_against_camera(url, 2).await;
        let text = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();

        let shown = shown_url(&run.config.rtsp_url);
        assert_eq!(shown, "rtsp://cd");
        for line in [
            format!("RTSP input started, connecting to {shown}\n"),
            format!("RTSP: connected and playing from {shown}\n"),
            "RTSP connection lost: ".to_string(),
        ] {
            assert!(text.contains(&line), "no {line:?} in:\n{text}");
        }
        let connected: Vec<_> = run.events.iter().filter(|e| e.message.starts_with("RTSP connected")).collect();
        let lost: Vec<_> = run.events.iter().filter(|e| e.message.starts_with("RTSP input disconnected")).collect();
        assert_eq!((connected.len(), lost.len()), (2, 2), "{:?}", run.events);
        for event in connected.iter().chain(&lost) {
            assert_eq!(event.details.as_ref().unwrap()["url"], shown.as_str(), "{event:?}");
        }
        assert!(connected.iter().all(|e| e.message == format!("RTSP connected to {shown}")));
        let meta = super::super::flow::build_input_config_meta(&crate::config::models::InputConfig::Rtsp(run.config.clone()));
        assert_eq!(meta.rtsp_url.as_deref(), Some(shown.as_str()));

        let events = format!("{:?}", run.events);
        for secret in ["s3cret", "admin"] {
            assert!(!text.contains(secret), "{secret} logged:\n{text}");
            assert!(!events.contains(secret), "{secret} in an event: {events}");
        }
        assert!(
            run.requests.iter().all(|r| !r.contains("s3cret") && !r.contains("admin")),
            "the client was given the userinfo: {:?}",
            run.requests
        );
    }

    /// A `tracing` writer into a shared buffer.
    #[derive(Clone)]
    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture buffer poisoned").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
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
