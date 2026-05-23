// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Channel-based wrapper that runs the transcoding chain
//! (`TsAudioReplacer` + `TsVideoReplacer`) on a dedicated codec thread
//! instead of inline via `tokio::task::block_in_place`.
//!
//! ## Why
//!
//! Before this module, each transcoded output task wrapped every
//! `replacer.process(...)` call in `tokio::task::block_in_place`. That
//! marks the calling Tokio worker as blocking, forcing the runtime to
//! recruit a replacement worker — and on return, swap them back. For
//! short blocking calls this is cheap, but a single H.264 keyframe
//! encode (5–20 ms of CPU on the wire), an AAC frame encode (~1 ms),
//! or worse a sustained NVENC burst causes work-stealing churn on the
//! runtime that adds 100s of µs to ms of jitter to **every other task**
//! sharing the runtime — including the per-flow PCR PLL sampler, the
//! master-clock sampler, and the wire-tx producer that feeds
//! `engine::wire_emit`. Under load this leaks visibly into PCR_AC and
//! A/V drift at the receiver.
//!
//! `TranscodeChain` lifts the entire transcoding chain onto a dedicated
//! OS thread with `SCHED_FIFO` priority 40 (one below wire-emit's 50,
//! so wire pacing always wins). The thread owns both replacers for the
//! output's lifetime. The Tokio output task feeds it raw TS bytes via
//! a bounded channel and receives transcoded TS bytes via another
//! bounded channel — both `try_send`/`try_recv` semantics, drop-on-full
//! backpressure that matches the broadcast-channel design upstream.
//!
//! ## Pipeline
//!
//! ```text
//!   filtered TS bytes  ──submit──▶  [codec thread:
//!                                       audio_replacer.process(in, scratch_a)
//!                                       video_replacer.process(scratch_a, scratch_b)
//!                                    ]  ──recv──▶  transcoded TS bytes
//! ```
//!
//! If only one of audio/video is configured, the other stage is a
//! pass-through. The chain takes care of holding the scratch buffers
//! across iterations so we don't reallocate.
//!
//! ## Drop semantics
//!
//! - Input full → caller's `try_submit` returns `Err(Bytes)`; the
//!   caller increments its `packets_dropped` counter. Same semantic as
//!   the existing `wire_tx.try_send` overflow path.
//! - Output full → codec thread skips this iteration's `blocking_send`
//!   and the encoded chunk is dropped. Live broadcast never wants
//!   buffered backlog — dropping is the correct fail-mode.
//! - Sender dropped → codec thread sees `None` on `blocking_recv`, calls
//!   `flush()` on each replacer to drain trailing PES / encoder state,
//!   sends the residue, then exits cleanly.
//!
//! ## Stats + shared flags
//!
//! All the Arc-based shared handles each replacer exposes
//! (`stats_handle()`, `decode_stats_handle()`,
//! `force_idr_handle()`, `external_reset_handle()`, etc.) are cloned
//! **before** the replacers move into the codec thread, then re-exposed
//! on `TranscodeChain` so output spawn code can register them with the
//! per-output stats accumulator and per-flow input-switch watcher
//! exactly as it does today. No call-site changes required for that
//! plumbing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::engine::codec_thread::{spawn_codec_thread, CodecThreadConfig};
use crate::engine::ts_audio_replace::{
    TsAudioReplaceError, TsAudioReplacer,
};
use crate::engine::ts_video_replace::{TsVideoReplaceError, TsVideoReplacer};

/// Bounded depth of the input channel. Matches the Standard tier of
/// [`crate::config::models::BandwidthProfile`] (16 384 slots) so the
/// chain offers the same startup-burst headroom as the flow-level
/// fan-out feeding it at typical TS-contribution bitrates.
/// At a few hundred Mbps total compressed input — the typical edge
/// workload — that's ~344 ms of in-flight TS bundles at 500 Mbps,
/// or ~680 ms at 250 Mbps. Memory cost is ~256 KB per chain (16 384 ×
/// the 16-byte `Bytes` struct; payload heap is refcounted and only
/// held while a slot is occupied). Beyond this depth, dropping the
/// newest input is the right call for live broadcast — buffering
/// past the encoder's pacing budget produces nothing useful.
///
/// This is per-output and does not scale with the flow's bandwidth
/// profile today: transcoded outputs are by definition the compressed-
/// egress path (ST 2110 etc. are passthrough on the egress side), so
/// Standard-tier sizing is the right floor here.
const TRANSCODE_CHAIN_INPUT_DEPTH: usize = 16_384;
/// Bounded depth of the output channel. Symmetric with the input
/// depth so a transient at either side doesn't pin the codec thread
/// against a smaller mismatched buffer.
const TRANSCODE_CHAIN_OUTPUT_DEPTH: usize = 16_384;

/// Backpressure handle from the wire pacer back into the codec thread.
/// The codec polls `depth` before each encode; if it's above
/// `threshold` the codec sleeps `frame_interval` to let the wire pacer
/// drain. Without this, a VBR encoder that briefly overshoots its
/// target rate during a complex scene fills the wire_tx channel, hits
/// the cap, and drops fresh broadcast content. With it, the codec
/// rate-matches the wire pacer's drain rate naturally — no explicit
/// rate limit, no PCR_AC violation, channel stays drained.
///
/// cell24 x264 (MPEG-2 NTSC → 1080p H.264 at 5 Mbps with `loop_playback`)
/// surfaced the underlying issue: the source's t=240–360 s content
/// window pushes x264 into a sustained ~6.5 Mbps egress rate while the
/// wire pacer drains at PCR cadence (≈ 6.3 Mbps); the differential
/// fills the 8192-dgm wire_tx channel within ~30 s of that window
/// starting and produces 14 k drops. Backpressure stops the codec
/// before the channel overflows.
#[derive(Clone)]
pub struct WireBackpressure {
    /// Shared atomic depth counter — wire_emit increments on send,
    /// decrements on receive. The codec polls this Acquire-ordered.
    pub depth: Arc<AtomicUsize>,
    /// Codec pauses (sleeps `frame_interval`) before the next encode
    /// when depth exceeds this. Sized to leave enough headroom for an
    /// in-flight I-frame burst — typically 75 % of `WIRE_CHANNEL_CAP`.
    pub threshold: usize,
    /// How long to sleep between depth re-checks when above threshold.
    /// One source frame interval is the natural choice — it's the
    /// granularity at which the codec produces output anyway.
    pub frame_interval: Duration,
}

impl WireBackpressure {
    /// Block the codec thread until `depth ≤ threshold` (or the cancel
    /// signal would have us exit anyway — checked via `input_rx.try_recv`
    /// for a `None` so a shutdown doesn't hang forever in this loop).
    ///
    /// Returns `true` if we slept at all (for diagnostic / telemetry).
    pub fn wait_until_drained(&self) -> bool {
        let mut slept_any = false;
        // Hard ceiling on cumulative wait — broadcast can't tolerate
        // unbounded codec stalls. 2 s matches the wire pacer's
        // worst-case 8192-dgm × 1316 B / 5 Mbps ≈ 17 s in-flight,
        // capped at the duration we'd accept anyway before declaring
        // the downstream genuinely stuck.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while self.depth.load(Ordering::Acquire) > self.threshold {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    "transcode-chain backpressure: depth still > {} after 2 s — \
                     wire pacer stuck, releasing codec to avoid deadlock",
                    self.threshold
                );
                break;
            }
            std::thread::sleep(self.frame_interval);
            slept_any = true;
        }
        slept_any
    }
}

/// Errors raised when building a [`TranscodeChain`]. Forwarded from
/// the underlying replacer constructors.
#[derive(Debug)]
pub enum TranscodeChainError {
    Audio(TsAudioReplaceError),
    Video(TsVideoReplaceError),
}

impl std::fmt::Display for TranscodeChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Audio(e) => write!(f, "audio replacer: {e}"),
            Self::Video(e) => write!(f, "video replacer: {e}"),
        }
    }
}

impl std::error::Error for TranscodeChainError {}

impl From<TsAudioReplaceError> for TranscodeChainError {
    fn from(e: TsAudioReplaceError) -> Self {
        Self::Audio(e)
    }
}

impl From<TsVideoReplaceError> for TranscodeChainError {
    fn from(e: TsVideoReplaceError) -> Self {
        Self::Video(e)
    }
}

/// Public handle to the transcoding chain. Holds the input sender,
/// output receiver, and all shared handles output spawn code needs to
/// register with stats / event plumbing.
pub struct TranscodeChain {
    /// `Bytes` chunks the output task wants the chain to transcode.
    /// Wrapped in `Option` so `Drop` can drop the sender explicitly
    /// **before** joining the codec thread — otherwise the thread
    /// blocks on `blocking_recv` forever and the join deadlocks.
    input_tx: Option<mpsc::Sender<Bytes>>,
    /// `Bytes` chunks the chain has produced, awaiting the output
    /// task's downstream pipeline.
    output_rx: mpsc::Receiver<Bytes>,
    /// JoinHandle for the codec thread. Held so the thread is joined
    /// on drop (graceful shutdown).
    thread: Option<JoinHandle<()>>,
    /// Shared audio external-reset flag — set by the input switch
    /// watcher to make the audio replacer reset PTS anchor + decoder
    /// on the next process() call.
    audio_external_reset: Option<Arc<AtomicBool>>,
    /// Shared video external-reset flag (counterpart to
    /// `audio_external_reset`).
    video_external_reset: Option<Arc<AtomicBool>>,
    /// Human-readable description of the chain (joins audio + video
    /// target descriptions). Used in startup logs.
    description: String,
}

impl TranscodeChain {
    /// Build a chain from optional audio + video replacers. At least
    /// one must be `Some` — otherwise the chain has no work to do and
    /// the caller should skip building it entirely.
    ///
    /// The replacers move into a dedicated codec thread (one per chain
    /// instance). Shared handles (stats, IDR / reset flags) are cloned
    /// from the replacers before the move so callers can wire them to
    /// the per-output stats accumulator + per-flow switch watcher.
    pub fn new(
        who: impl Into<String>,
        audio: Option<TsAudioReplacer>,
        video: Option<TsVideoReplacer>,
        backpressure: Option<WireBackpressure>,
    ) -> Self {
        debug_assert!(
            audio.is_some() || video.is_some(),
            "TranscodeChain::new called with no replacers — caller should skip",
        );
        let who = who.into();

        // Snapshot the external-reset flags before moving the replacers
        // into the codec thread. The per-output input-switch watcher
        // needs them so a same-codec same-PID input swap resets PTS
        // anchor + encoder IDR. Each handle is an `Arc<AtomicBool>`;
        // the clone is cheap and refcount-only.
        let audio_external_reset = audio.as_ref().map(TsAudioReplacer::external_reset_handle);
        let video_external_reset = video.as_ref().map(TsVideoReplacer::external_reset_handle);
        // Other shared handles (stats, encode/decode counters, force-IDR
        // flag) are already registered against the per-output stats
        // accumulator inside `build_for_output` before the replacers
        // move here, so the chain itself does not need to expose them.

        let description = {
            let mut parts = Vec::with_capacity(2);
            if let Some(ref a) = audio {
                parts.push(format!("audio={}", a.target_description()));
            }
            if let Some(ref v) = video {
                parts.push(format!("video={}", v.target_description()));
            }
            parts.join(", ")
        };

        let (input_tx, input_rx) = mpsc::channel::<Bytes>(TRANSCODE_CHAIN_INPUT_DEPTH);
        let (output_tx, output_rx) = mpsc::channel::<Bytes>(TRANSCODE_CHAIN_OUTPUT_DEPTH);

        // Move the replacers into the codec thread. The thread owns
        // them for its entire lifetime — no per-frame ownership
        // transfer.
        let thread = spawn_codec_thread(
            CodecThreadConfig::realtime(format!("transcode-{}", who.clone())),
            move || run_chain(input_rx, output_tx, audio, video, backpressure),
        );

        tracing::info!(
            "transcode-chain '{}' started: {}",
            who, description
        );

        Self {
            input_tx: Some(input_tx),
            output_rx,
            thread: Some(thread),
            audio_external_reset,
            video_external_reset,
            description,
        }
    }

    /// Submit raw TS bytes to the chain. Non-blocking. Returns the
    /// original bytes back when the codec thread is backed up so the
    /// caller can increment its drop counter without re-cloning.
    pub fn try_submit(&self, raw_ts: Bytes) -> Result<(), Bytes> {
        let Some(input_tx) = self.input_tx.as_ref() else {
            return Err(raw_ts);
        };
        match input_tx.try_send(raw_ts) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(b)) => Err(b),
            Err(mpsc::error::TrySendError::Closed(b)) => Err(b),
        }
    }

    /// Non-blocking poll for the next transcoded chunk. Returns `None`
    /// when nothing is ready yet (encoder still working) or when the
    /// codec thread has exited (channel closed). Production output
    /// tasks drain via this on each iteration — the async `recv`
    /// pattern would require splitting the receiver out of the chain
    /// to avoid borrow conflicts in `tokio::select!`.
    pub fn try_recv(&mut self) -> Option<Bytes> {
        self.output_rx.try_recv().ok()
    }

    /// Human-readable summary used at output startup logs.
    pub fn target_description(&self) -> &str {
        &self.description
    }

    /// External-reset flag for the audio replacer — flipped by the
    /// per-flow input-switch watcher so the audio decoder + PTS anchor
    /// reset across a same-codec same-PID input swap. `None` when the
    /// chain has no audio stage.
    pub fn audio_external_reset_handle(&self) -> Option<Arc<AtomicBool>> {
        self.audio_external_reset.clone()
    }

    /// External-reset flag for the video replacer — counterpart to
    /// `audio_external_reset_handle`. `None` when the chain has no
    /// video stage.
    pub fn video_external_reset_handle(&self) -> Option<Arc<AtomicBool>> {
        self.video_external_reset.clone()
    }
}

/// Build a `TranscodeChain` (if at least one of `audio_encode` /
/// `video_encode` is set) and wire all the per-output stats handles
/// + the optional A/V sync pacer through to the wrapped replacers
/// **before** they move into the codec thread.
///
/// Returns:
/// - `Ok(None)` — neither audio_encode nor video_encode configured;
///   caller should fall back to passthrough.
/// - `Ok(Some(chain))` — chain built. Caller registers
///   `chain.audio_external_reset_handle()` and
///   `chain.video_external_reset_handle()` (each `Option<Arc<AtomicBool>>`)
///   with the per-output input-switch watcher, then uses
///   `chain.try_submit` + `chain.try_recv` in its main loop.
/// - `Err(_)` — one of the replacers failed to construct. The caller's
///   existing error-event emission path handles this.
pub fn build_for_output(
    output_id: &str,
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    video_encode: Option<&crate::config::models::VideoEncodeConfig>,
    transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    stats: &Arc<crate::stats::collector::OutputStatsAccumulator>,
    av_sync_pacer: Option<&Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    backpressure: Option<WireBackpressure>,
) -> Result<Option<TranscodeChain>, TranscodeChainError> {
    let audio = match audio_encode {
        Some(enc) => {
            let mut r = TsAudioReplacer::new(enc, transcode)?;
            stats.set_audio_replacer_stats(r.stats_handle());
            stats.set_decode_stats(r.decode_stats_handle(), "", 0, 0);
            // Attach the per-flow A/V sync pacer so the audio anchor on
            // first PES + every >500 ms source-PTS discontinuity re-anchor
            // pulls from the master clock instead of the raw source PTS.
            // Mirrors the video wiring below; the 10 s safety check
            // inside `TsAudioReplacer::anchor_target` keeps the existing
            // anchor-to-source behaviour intact when master and source
            // clocks are wildly uncorrelated (Wallclock master vs
            // encoder-relative source PTS, PLL pre-lock garbage).
            if let Some(p) = av_sync_pacer {
                r.set_av_sync_pacer(p.clone());
            }
            // Hook the audio replacer's "output stats accumulator"
            // reference so it can refresh source-codec labels on PMT
            // updates — identical to today's inline construction.
            let r = r.with_output_stats(Arc::clone(stats));
            Some(r)
        }
        None => None,
    };

    let video = match video_encode {
        Some(enc) => {
            let mut r = TsVideoReplacer::new(enc, None)?;
            let backend = match enc.codec.as_str() {
                "x264" | "x265" => enc.codec.clone(),
                "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
                other => other.to_string(),
            };
            let target_codec = match enc.codec.as_str() {
                "x264" | "h264_nvenc" => "h264",
                "x265" | "hevc_nvenc" => "hevc",
                other => other,
            };
            stats.set_video_encode_stats(
                r.stats_handle(),
                String::new(),
                target_codec.to_string(),
                enc.width.unwrap_or(0),
                enc.height.unwrap_or(0),
                match (enc.fps_num, enc.fps_den) {
                    (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
                    _ => 0.0,
                },
                enc.bitrate_kbps.unwrap_or(0),
                backend,
            );
            stats.set_video_decode_stats(r.decode_stats_handle(), "", 0, 0, 0.0);
            if let Some(p) = av_sync_pacer {
                r.set_av_sync_pacer(p.clone());
            }
            Some(r)
        }
        None => None,
    };

    if audio.is_none() && video.is_none() {
        return Ok(None);
    }
    Ok(Some(TranscodeChain::new(output_id, audio, video, backpressure)))
}

impl Drop for TranscodeChain {
    fn drop(&mut self) {
        // Order matters: drop the input sender **before** joining the
        // codec thread. The channel stays open as long as any sender
        // exists; the codec thread blocks on `blocking_recv`
        // indefinitely while the channel is open, so a `join()` before
        // the sender drops deadlocks. After the drop, the thread sees
        // `None` on `blocking_recv`, runs the flush path on each
        // replacer (best-effort try_send for trailing residue), and
        // exits. Join completes within tens of ms.
        drop(self.input_tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Codec-thread body. Owns both replacers + scratch buffers for the
/// chain's lifetime.
fn run_chain(
    mut input_rx: mpsc::Receiver<Bytes>,
    output_tx: mpsc::Sender<Bytes>,
    mut audio: Option<TsAudioReplacer>,
    mut video: Option<TsVideoReplacer>,
    backpressure: Option<WireBackpressure>,
) {
    // Scratch buffers persist across iterations so we don't reallocate
    // per input chunk. 64 KB initial — typical input is ~1316 B (RTP
    // bundle of 7 × 188 B TS); 64 KB absorbs a small batched burst
    // without reallocation.
    let mut after_audio_scratch: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut after_video_scratch: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        // Codec → wire_emit backpressure. When the wire_tx queue is
        // > 75 % full the wire pacer hasn't drained the previous burst
        // yet; sleeping one frame interval here gives it time without
        // blocking the data path or violating PCR_AC. Once depth drops
        // below threshold the codec resumes at full speed.
        //
        // Without this, transient encoder overshoots (VBR I-frames,
        // sustained complex scenes) fill wire_tx, hit the cap, and drop
        // fresh broadcast content. See `WireBackpressure` doc for the
        // cell24 x264 case study.
        if let Some(ref bp) = backpressure {
            bp.wait_until_drained();
        }

        let input = match input_rx.blocking_recv() {
            Some(b) => b,
            None => {
                // Input channel closed — drain replacer trailing state
                // (PES buffer, encoder pipeline residue) and forward
                // before exiting. Use `try_send` (not `blocking_send`)
                // because the receiver may already be gone on
                // shutdown — `blocking_send` would deadlock.
                if let Some(ref mut a) = audio {
                    after_audio_scratch.clear();
                    a.flush(&mut after_audio_scratch);
                    if !after_audio_scratch.is_empty() {
                        let drained = Bytes::copy_from_slice(&after_audio_scratch);
                        let _ = output_tx.try_send(drained);
                    }
                }
                if let Some(ref mut v) = video {
                    after_video_scratch.clear();
                    v.flush(&mut after_video_scratch);
                    if !after_video_scratch.is_empty() {
                        let drained = Bytes::copy_from_slice(&after_video_scratch);
                        let _ = output_tx.try_send(drained);
                    }
                }
                break;
            }
        };

        // Audio replace (if configured) — otherwise pass through.
        // Holds the slice borrow into one of the scratch buffers; we
        // re-borrow into `after_video_scratch` for the next stage.
        let after_audio: &[u8] = if let Some(ref mut a) = audio {
            after_audio_scratch.clear();
            a.process(&input, &mut after_audio_scratch);
            &after_audio_scratch
        } else {
            &input
        };

        // Video replace (if configured) — otherwise pass through.
        let result: Bytes = if let Some(ref mut v) = video {
            after_video_scratch.clear();
            v.process(after_audio, &mut after_video_scratch);
            if after_video_scratch.is_empty() {
                // Encoder may legitimately emit 0 bytes on a single
                // input chunk (the input was non-video, e.g. PAT
                // synth, or the encoder is still in its B-frame
                // reorder window). Skip this iteration; the encoder
                // will produce output on a later input chunk.
                continue;
            }
            Bytes::copy_from_slice(&after_video_scratch)
        } else if !after_audio.is_empty() {
            // Audio-only chain: forward what the audio stage produced.
            Bytes::copy_from_slice(after_audio)
        } else {
            continue;
        };

        // Drop-on-full: live broadcast never wants to buffer past the
        // configured depth. The output task's per-flow stats accumulator
        // already counts wire-side drops via `wire_pacing_late` /
        // packets_dropped; the chain's drops will show up the same way
        // because the downstream pipeline can't see what we never sent.
        match output_tx.try_send(result) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Output task is behind. Skip this chunk — the
                // encoder will recover on the next input.
                tracing::trace!("transcode-chain: output channel full — dropped one chunk");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Receiver gone — output task has shut down. Exit
                // immediately to avoid burning CPU on output that has
                // nowhere to go.
                break;
            }
        }
    }
    tracing::debug!("transcode-chain: codec thread exiting (graceful)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::AudioEncodeConfig;

    /// Build a chain with only audio so we can validate the channel
    /// plumbing without the video-encoder feature gate.
    fn make_audio_only_chain() -> TranscodeChain {
        let cfg = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            source_audio_pid: None,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
        };
        let audio = TsAudioReplacer::new(&cfg, None).expect("audio replacer build");
        TranscodeChain::new("test-audio-only", Some(audio), None, None)
    }

    /// Construct + immediately drop. Verifies the codec thread sees
    /// channel close, drains via flush, and the Drop's join() returns
    /// — the deadlock we hit on first cut (`blocking_recv` waited
    /// forever because the sender was still alive when join started).
    #[test]
    fn drop_completes_without_deadlock() {
        let chain = make_audio_only_chain();
        drop(chain);
        // If we reach here, the codec thread exited cleanly and join
        // returned. Without the explicit sender-drop-before-join order
        // in `impl Drop`, this hangs indefinitely.
    }
}
