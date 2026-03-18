use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::WebrtcOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;

/// Spawn a WebRTC/WHIP output task.
///
/// **This is a stub implementation.** The full WebRTC output requires the
/// `webrtc` crate (webrtc-rs) which is a heavy dependency not yet included
/// in Cargo.toml. This stub allows the configuration to be accepted and
/// the project to compile without that dependency.
///
/// The stub subscribes to the broadcast channel and silently consumes
/// packets (so the broadcast channel does not report lag for this output),
/// but does not transmit any data.
///
/// # Full implementation roadmap
///
/// A complete WebRTC output would perform these steps:
///
/// ## 1. WHIP Signaling (WebRTC-HTTP Ingestion Protocol, RFC draft)
///
/// - Construct an SDP offer describing a sendonly H.264 video track
///   (and optionally an Opus audio track).
/// - HTTP POST the SDP offer to `config.whip_url` with
///   `Content-Type: application/sdp`.
/// - If `config.bearer_token` is set, include an `Authorization: Bearer`
///   header for authentication.
/// - Parse the 201 Created response body as the SDP answer.
/// - Extract ICE candidates and DTLS fingerprint from the answer.
///
/// ## 2. ICE + DTLS + SRTP establishment
///
/// - Use the `webrtc::ice` agent to perform ICE connectivity checks
///   (STUN binding requests) using candidates from the SDP exchange.
/// - Once ICE succeeds, perform a DTLS handshake over the selected
///   candidate pair to derive SRTP keys.
/// - Open an SRTP session for encrypted media transport.
///
/// ## 3. H.264 NALU extraction from MPEG-2 TS
///
/// - Demux the incoming TS stream (from the broadcast channel's RTP
///   packets with headers stripped) using `TsDemuxer` to extract PES
///   packets carrying H.264 Annex B byte streams.
/// - Parse the Annex B stream to identify individual NAL Units (NALUs),
///   splitting on 0x00000001 start codes.
/// - Identify NALU types: SPS (type 7), PPS (type 8), IDR (type 5),
///   non-IDR slices (type 1), SEI (type 6), etc.
/// - Cache the most recent SPS/PPS for periodic re-sending (needed for
///   late joiners and after packet loss).
///
/// ## 4. RFC 6184 RTP packetization (H.264 over RTP)
///
/// - Small NALUs (< MTU, typically 1200 bytes for WebRTC) are sent as
///   Single NAL Unit packets: RTP header + NALU directly.
/// - Large NALUs are fragmented using FU-A (Fragmentation Unit type A):
///   each fragment has an FU indicator byte and FU header byte before
///   the NALU fragment data.
/// - Set the RTP marker bit on the last packet of each access unit
///   (frame boundary).
/// - Use payload type 96 (dynamic) and clock rate 90000 Hz.
/// - Increment the RTP sequence number per packet, RTP timestamp per
///   frame (using 90 kHz clock derived from PTS).
///
/// ## 5. Audio handling
///
/// - **Opus passthrough only**: if the source TS carries Opus audio
///   (PID with stream type 0x06 and Opus descriptor), the Opus frames
///   can be forwarded directly in RTP packets (RFC 7587, clock 48000).
/// - **AAC is NOT supported**: transcoding AAC to Opus requires a C
///   library (e.g. libopus + FFmpeg). If the source carries AAC audio,
///   the user should set `video_only: true` in the config or the audio
///   track will be silent.
///
/// ## 6. Congestion control and stats
///
/// - Implement TWCC (Transport-Wide Congestion Control) or GCC
///   (Google Congestion Control) feedback to adapt video bitrate.
/// - Report `packets_sent`, `bytes_sent` to the stats accumulator.
/// - On ICE failure or DTLS timeout, log an error and attempt
///   re-signaling after a backoff delay.
pub fn spawn_webrtc_output(
    config: WebrtcOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        tracing::warn!(
            "WebRTC output '{}' is a stub: the `webrtc` cargo feature/dependency is not enabled. \
             Packets will be consumed but not transmitted. \
             WHIP endpoint: {}",
            config.id,
            config.whip_url,
        );

        webrtc_stub_loop(&config, &mut rx, output_stats, cancel).await;
    })
}

/// Stub receive loop that consumes packets without transmitting.
///
/// This prevents the broadcast channel from reporting lag for this output
/// while the real WebRTC implementation is not yet available. The loop
/// exits when the cancellation token fires or the broadcast channel closes.
async fn webrtc_stub_loop(
    config: &WebrtcOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("WebRTC output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(_packet) => {
                        // Stub: consume and discard. A real implementation would
                        // demux TS, extract H.264 NALUs, repacketize per RFC 6184,
                        // and send via the SRTP session established through WHIP.
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "WebRTC output '{}' lagged, dropped {n} packets",
                            config.id,
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("WebRTC output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }
}
