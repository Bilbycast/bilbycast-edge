// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Flow engine: lifecycle management for input and output tasks.
//!
//! The [`manager::FlowManager`] orchestrates creating, starting, stopping, and
//! querying flows. Each flow is represented by a [`flow::FlowRuntime`] which
//! owns the input task, output tasks, a broadcast channel connecting them,
//! and a cancellation token hierarchy for graceful shutdown.
//!
//! Input tasks ([`input_rtp`], [`input_srt`], [`input_rtmp`]) receive packets and publish
//! [`packet::RtpPacket`] structs to the broadcast channel. Output tasks
//! ([`output_rtp`], [`output_srt`]) subscribe independently and forward
//! packets to their destinations.
//!
//! Optional features are inserted at the input/output boundaries:
//! - **FEC decode** on RTP input (SMPTE 2022-1 packet recovery)
//! - **FEC encode** on RTP output (SMPTE 2022-1 parity generation)
//! - **Redundancy merge** on SRT input (SMPTE 2022-7 de-duplication)
//! - **Redundancy duplicate** on SRT output (SMPTE 2022-7 dual-leg send)

/// Per-flow A/V sync mux — master-clock-driven PCR pacing helper
/// consumed by the TS replacers and per-output emit code. See
/// [`av_sync_mux`] for the architectural rationale (operator chose
/// "per-output replacer keeps current shape; mux only governs PCR /
/// emission timing" over the heavier per-flow producer).
pub mod av_quality_watch;
pub mod av_sync_mux;
pub mod bandwidth_monitor;
pub mod bandwidth_profile;
pub mod bond_leg_broker;
pub mod bond_leg_probe;
pub mod bond_leg_test;
pub mod bond_routing;
pub mod bonded_scheduler;
/// Fragmented-MP4 (CMAF / CMAF-LL) HTTP-push output with optional CENC
/// ClearKey encryption. Sibling to `output_hls` — same push model but
/// fMP4 instead of MPEG-TS.
pub mod cmaf;
/// In-depth content-analysis subsystem. Three tiers (Lite, Audio Full,
/// Video Full) gated per-flow on `FlowConfig.content_analysis`. Each
/// tier is a dedicated broadcast subscriber and cannot add jitter or
/// backpressure to the data path. See [`content_analysis`] for the full
/// rationale and tier cost breakdown.
pub mod content_analysis;
pub mod degradation_monitor;
pub mod delay_buffer;
pub mod flow;
/// Hardware probe + CPU class + software-encode capacity estimate. Powers
/// the `resource_budget` block on the manager `HealthPayload`. One-shot
/// probe at startup, no async / no I/O. See [`hardware_probe`].
pub mod hardware_probe;
pub mod input_bonded;
/// File-backed media-player input — TS / MP4 / image fallback source.
/// See [`crate::config::models::MediaPlayerInputConfig`] for the config
/// surface and `docs/configuration-guide.md` for the operator-facing
/// reference.
pub mod input_media_player;
/// Replay input — pumps recorded TS segments back onto the broadcast
/// channel. See [`crate::config::models::ReplayInputConfig`] for the
/// config surface, [`crate::replay`] for the on-disk store.
#[cfg(feature = "replay")]
pub mod input_replay;
pub mod input_rist;
pub mod input_rtmp;
pub mod input_rtp;
pub mod input_rtsp;
pub mod input_srt;
pub mod input_switch_watcher;
pub mod input_test_pattern;
pub mod input_udp;
#[cfg(feature = "webrtc")]
pub mod input_webrtc;
pub mod manager;
/// Master-clock abstraction: the per-flow reference every output paces
/// against. PCR generation, emission timing, and cross-edge coherence all
/// bottom out on `MasterClock::now_27mhz`. See [`master_clock`] for the
/// trait, kind enum, and selection policy. Sibling modules `pcr_pll` and
/// `av_sync_mux` (Phases 2 + 4) consume this abstraction.
pub mod master_clock;
pub mod media_analysis;
pub mod output_bonded;
/// Local-display output (HDMI / DisplayPort + ALSA). Linux-only,
/// gated on the `display` Cargo feature.
#[cfg(all(feature = "display", target_os = "linux"))]
pub mod output_display;
pub mod output_hls;
pub mod output_rist;
pub mod output_rtmp;
pub mod output_rtp;
pub mod output_srt;
pub mod output_udp;
pub mod output_webrtc;
pub mod packet;
/// Per-flow ingress PCR sampler — broadcast-channel subscriber that
/// extracts PCR samples from every TS packet and feeds the source-PCR
/// PLL. Sibling to `stats::pcr_trust` (egress accuracy).
pub mod pcr_ingress_sampler;
/// Software PI-controller PLL recovering source's 27 MHz from incoming
/// PCR samples. Used by [`master_clock::MasterClockKind::SourcePcrPll`].
/// See [`pcr_pll`] for the PI loop, lock-state hysteresis, and the
/// pre-sample wallclock fallback.
pub mod pcr_pll;
pub mod perf;
/// PES Switch Phase 4 — PES-aligned splice state machines driven by
/// `SwitchActiveInput { splice_mode: PesAligned }`: audio (PES-boundary
/// aligned) and video (H.264 / HEVC, IDR-aligned). Slots whose codec
/// supports neither fall through to the PmtBump path and emit
/// `pes_splice_degraded`.
pub mod pes_splice;
pub mod resource_monitor;
pub mod rtmp;
pub mod scte35_encode;
pub mod thumbnail;
pub mod tr101290;
/// Phase 5 PID-bus SPTS assembler: subscribes to elementary streams on
/// [`ts_es_bus::NodeEsBus`], rewrites PIDs, stamps per-out-PID continuity,
/// synthesises PAT/PMT, and emits bundled TS `RtpPacket`s onto the flow's
/// broadcast channel. Consumed by the FlowRuntime when a flow's
/// `assembly.kind = spts`.
pub mod ts_assembler;
pub mod ts_audio_replace;
pub mod ts_av_realign;
pub mod ts_continuity_fixer;
pub mod ts_demux;
/// Phase 8 PID-bus per-ES analyzer: one lightweight task per
/// `(input_id, source_pid)` bus key tracking packets, bytes, CC errors,
/// PCR discontinuity, and last-seen stream_type. Surfaced via
/// `FlowStats.per_es` alongside the existing flow-level TR-101290
/// analyzer (which keeps the heavy PES-level work).
pub mod ts_es_analysis;
pub mod ts_es_bus;
/// Phase 7 PID-bus Hitless merger: pre-bus dedup task that turns one
/// `SlotSource::Hitless { primary, backup }` slot into a single
/// synthetic [`ts_es_bus::NodeEsBus`] key the assembler consumes.
pub mod ts_es_hitless;
pub mod ts_null_padder;
pub mod ts_parse;
pub mod ts_pid_overrides_rewriter;
pub mod ts_pid_remapper;
pub mod ts_program_filter;
pub mod ts_psi_catalog;
/// Encoder-style PES PTS/DTS rewriter — byte-level, no decode. Gated
/// per-input by [`crate::config::models::RtpInputConfig::passthrough_clock`].
/// Plugs in as the fourth optional stage of [`input_post_process`].
pub mod ts_pts_rewriter;
pub mod ts_video_replace;
/// Per-output wire emission engine. Dedicated `std::thread` (Linux:
/// `SCHED_FIFO`) that paces TS datagrams onto the wire via PCR-anchored
/// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, ...)`. Decouples wire
/// timing from the Tokio timer wheel — sub-100 µs precision instead of
/// the ~1–5 ms ceiling `tokio::time::sleep_until` imposes. See
/// [`docs/wire-pacing.md`](../../docs/wire-pacing.md).
pub mod wire_emit;

/// Phase 2 of the wire-pacing rollout: kernel-paced wire emission via
/// Linux `SO_TXTIME` + ETF qdisc. Targets ST 2110-20/-22 narrow profile
/// at 250 k–1 M pps where userspace `clock_nanosleep` cannot meet the
/// per-packet timing budget. See [`docs/wire-pacing.md`](../../docs/wire-pacing.md)
/// "Phase 2" section. Linux-only — public surface compiles on other
/// platforms but every operation returns `Unsupported`.
pub mod wire_emit_txtime;

/// Dedicated OS-thread runtime for codec work (video / audio encoders,
/// decoders, transcoders). Moves heavy CPU work off Tokio's worker
/// pool — eliminates `tokio::task::block_in_place` work-stealing churn
/// that would otherwise leak scheduling jitter into the PCR PLL,
/// master-clock sampler, and wire-tx producer paths. SCHED_FIFO 40 +
/// optional CPU pinning via `BILBYCAST_CODEC_CPUS`. See
/// [`docs/production-tuning.md`](../../docs/production-tuning.md).
pub mod codec_thread;

/// Channel-based wrapper that drives the transcoding chain
/// (`TsAudioReplacer` + `TsVideoReplacer`) on a dedicated
/// `codec_thread`. Replaces the inline `tokio::task::block_in_place`
/// pattern at every transcoded output (output_udp / output_rtp /
/// output_srt / output_rist). See module docs for the rationale.
pub mod transcode_chain;

/// Per-task dedicated `tokio::current_thread` runtime on an OS thread
/// with `SCHED_FIFO` + optional CPU pinning. Stages 2 and 3 use this
/// to lift the PID-bus demuxer + assembler (Stage 2) and the PCR PLL
/// sampler (Stage 3) off the main Tokio worker pool without rewriting
/// either as raw synchronous OS-thread code.
pub mod dedicated_runtime;

/// Per-input ingress publisher — the buffering front-end between an input
/// task and the flow broadcast channel. Picks one of three modes:
/// `Direct` (passthrough), `Delayed` (per-input `ingress_delay_ms` — a
/// FIXED delay line for alignment / cross-device sync; does NOT remove
/// jitter), or `Dejitter` (delegates to [`ingress_dejitter`]). Opt-in, off
/// by default. See the module docs for the delay-vs-de-jitter distinction.
pub mod ingress_publisher;

/// Per-input de-jitter buffer — opt-in, off by default. The ingress
/// counterpart to the egress release-rate servo in [`wire_emit`]:
/// recovers the source rate from inter-PCR observations and releases
/// packets paced at that rate (leaky bucket + fill servo + residence-cap
/// shed), so downstream consumers see a smooth cadence regardless of
/// network PDV. Unlike the `ingress_delay_ms` fixed delay (a pure delay
/// line) it actually removes jitter. Wired on raw UDP/RTP only; cooperates
/// with the SMPTE 2022-7 merger by sitting after it.
pub mod ingress_dejitter;

#[cfg(feature = "media-codecs")]
pub mod video_encode_util;

pub mod input_post_process;
/// Ingress-side audio + video transcoding composer for TS-carrying inputs.
/// Wraps [`ts_audio_replace::TsAudioReplacer`] and [`ts_video_replace::TsVideoReplacer`]
/// in the same audio-first-then-video order the TS outputs already use, so
/// every Group A input (RTP, UDP, SRT, RIST, RTMP, RTSP, WebRTC) can normalise
/// its feed once before the flow broadcast channel and amortise codec work
/// across all attached outputs.
pub mod input_transcode;

/// Ingress-side PCM-shape processor for raw PCM-RTP inputs (ST 2110-30,
/// `rtp_audio`). Depacketizes, optionally reshapes via `PlanarAudioTranscoder`
/// (stays PCM), optionally encodes via `AudioEncoder` and muxes audio-only TS
/// (shape-change: PCM-RTP → MPEG-TS). Runs entirely inside the input task via
/// `tokio::task::block_in_place`.
pub mod input_pcm_encode;
#[cfg(feature = "webrtc")]
pub mod webrtc;

/// Per-output PCM audio transcoding stage (sample rate, bit depth, channel
/// routing, packet time, payload type). Pure Rust via `rubato`. Fed by ST 2110
/// audio inputs and the new `rtp_audio` input; consumed by ST 2110-30/-31,
/// `rtp_audio`, and SMPTE 302M-over-SRT/UDP/RTP outputs.
pub mod audio_transcode;

/// In-process AAC-LC decoder. Bridges compressed contribution audio (AAC in
/// MPEG-TS via RTMP/RTSP/SRT/UDP inputs) into the PCM pipeline so it can land
/// into the existing PCM-only outputs (ST 2110-30/-31, `rtp_audio`,
/// SMPTE 302M). Pure Rust via `symphonia-codec-aac`. AAC-LC mono/stereo only;
/// HE-AAC and multichannel AAC are rejected with a clear error.
pub mod audio_decode;

/// Lock-free counters for the video-decode stage. Shared between
/// [`ts_video_replace::TsVideoReplacer`] (paired with `VideoEncodeStats` for
/// a true transcode) and [`output_display`] (decode-only). Surfaced on
/// [`crate::stats::models::VideoDecodeStatsSnapshot`].
pub mod video_decode_stats;

/// ffmpeg-sidecar audio encoder. Wraps a long-running ffmpeg subprocess as a
/// PCM → compressed-audio encoder for the RTMP, HLS, and WebRTC outputs.
/// Pure Rust binary — ffmpeg is invoked at runtime via subprocess, never
/// linked. Outputs that don't request `audio_encode` keep working unchanged.
/// Supports AAC-LC, HE-AAC v1/v2, Opus, MP2, AC-3.
pub mod audio_encode;

/// Silent-audio generator + audio-drop watchdog used by
/// `audio_encode.silent_fallback` on RTMP, WebRTC and CMAF outputs.
/// Emits zero-filled planar PCM chunks at the encoder's native cadence
/// whenever the upstream source has no audio PID, or stops delivering
/// audio mid-stream.
pub mod audio_silence;

/// SMPTE 302M-2007 LPCM audio packetizer / depacketizer. Bit-packs 48 kHz
/// 16/20/24-bit audio into private MPEG-TS PES payloads tagged with the
/// `BSSD` registration descriptor. Pairs with [`audio_transcode`] (which
/// resamples non-48 kHz inputs and promotes to even channel counts) and
/// the TS muxer (which wraps the PES payload in `stream_type = 0x06`).
pub mod audio_302m;

/// Short-lived connection test tasks for validating input/output configs
/// without requiring a running flow.
pub mod test_connection;

/// Standby listener manager for passive-type inputs that are not assigned
/// to running flows. Keeps sockets bound so users can see connection status.
pub mod standby_listeners;

/// SMPTE ST 2110 essence-flow support (audio, ancillary, future video).
///
/// Phase 1 lands the foundation in three pure-Rust submodules:
/// - [`st2110::ptp`]: PTP state reporter (reads `ptp4l`'s management Unix
///   socket; reports `Unavailable` when no daemon is running)
/// - [`st2110::hwts`]: hardware timestamp helpers (`SO_TIMESTAMPING` enable
///   and cmsghdr parsing on Linux; no-op on other platforms)
/// - [`st2110::redblue`]: SMPTE 2022-7 dual-network ("Red"/"Blue") bind
///   helpers and recv loop, reusing `redundancy::merger::HitlessMerger`
///
/// Packetizers and runtime input/output tasks land in Phase 1 step 4.
pub mod st2110;

/// Shared input/output runtime helpers for ST 2110 essence flows. The thin
/// `input_st2110_*` / `output_st2110_*` modules below delegate to the helpers
/// in this module so the recv/send/RedBluePair plumbing lives in one place.
pub mod st2110_io;

/// Shared runtime helpers for ST 2110-20 / -23 (uncompressed video). Heavy
/// codec work (encode / decode / scale) runs on dedicated blocking workers so
/// the tokio reactor is never blocked.
pub mod st2110_video_io;

pub mod input_st2110_20;
pub mod input_st2110_23;
pub mod input_st2110_30;
pub mod input_st2110_31;
pub mod input_st2110_40;
pub mod output_st2110_20;
pub mod output_st2110_23;
pub mod output_st2110_30;
pub mod output_st2110_31;
pub mod output_st2110_40;

#[cfg(feature = "mxl")]
pub mod input_mxl_anc;
#[cfg(feature = "mxl")]
pub mod input_mxl_audio;
#[cfg(feature = "mxl")]
pub mod input_mxl_video;
/// MXL (Media eXchange Layer) — same-host cloud-native broadcast composition.
/// Wraps `dmf-mxl/mxl` v1.0.1 via the sibling `bilbycast-mxl-rs` crate. Boot
/// probe lives in `engine::mxl::domain::MxlDomainManager::probe`. Off by
/// default — gated by the `mxl` Cargo feature. See
/// `docs/mxl-integration-plan.md` for the rationale and
/// `bilbycast-mxl-rs/CLAUDE.md` for the build prereq footprint.
#[cfg(feature = "mxl")]
pub mod mxl;
#[cfg(feature = "mxl")]
pub mod mxl_io;
#[cfg(feature = "mxl")]
pub mod mxl_video_io;
#[cfg(feature = "mxl")]
pub mod output_mxl_anc;
#[cfg(feature = "mxl")]
pub mod output_mxl_audio;
#[cfg(feature = "mxl")]
pub mod output_mxl_video;

/// SDI capture/playout via Blackmagic DeckLink. Wraps FFmpeg's `decklink`
/// avdevice via the sibling `bilbycast-decklink-rs` crate. Boot probe lives in
/// `engine::decklink::domain::DecklinkDeviceManager::probe`. Off by default —
/// gated by the `sdi-decklink` Cargo feature. See `bilbycast-decklink-rs/CLAUDE.md`.
#[cfg(feature = "sdi-decklink")]
pub mod decklink;
#[cfg(feature = "sdi-decklink")]
pub mod output_sdi;
#[cfg(feature = "sdi-decklink")]
pub mod sdi_io;

/// Generic RFC 3551 PCM-over-RTP audio input. Wire-identical to ST 2110-30
/// but with no PTP / RFC 7273 / NMOS clock_domain. Drives the same shared
/// runtime as the ST 2110 audio modules.
pub mod input_rtp_audio;
/// Generic RFC 3551 PCM-over-RTP audio output (companion to `input_rtp_audio`).
/// Supports the same `transcode` block as ST 2110-30 outputs and a future
/// `transport_mode: "audio_302m"` for SMPTE 302M-in-MPEG-TS over RTP.
pub mod output_rtp_audio;
