# Supported Protocols and Capabilities

## Overview

bilbycast-edge is a pure-Rust media gateway supporting multiple transport protocols for professional broadcast and streaming workflows. All protocol implementations are native Rust with no C library dependencies.

## Input Protocols

### RTP (SMPTE ST 2022-2)
- **Direction:** Input
- **Transport:** UDP unicast or multicast, IPv4 and IPv6
- **Payload:** MPEG-2 Transport Stream in RTP per SMPTE ST 2022-2
- **Features:**
  - SMPTE 2022-1 FEC decode (column + row parity)
  - IGMP/MLD multicast group join with interface selection
  - Source IP allow-list filtering (RP 2129 C5)
  - RTP payload type filtering (RP 2129 U4)
  - Per-flow ingress rate limiting (RP 2129 C7)
  - DSCP/QoS marking on egress (RP 2129 C10)
  - VSF TR-07 mode: auto-detects JPEG XS streams

### UDP (Raw MPEG-TS)
- **Direction:** Input
- **Transport:** UDP unicast or multicast, IPv4 and IPv6
- **Payload:** Raw MPEG-TS datagrams (no RTP headers required)
- **Features:**
  - Accepts any UDP datagram (no RTP v2 header filtering)
  - IGMP/MLD multicast group join with interface selection
  - Compatible with OBS, ffmpeg `udp://` output, srt-live-transmit, VLC

### SRT (Secure Reliable Transport)
- **Direction:** Input
- **Transport:** UDP with ARQ retransmission
- **Modes:** Caller, Listener, Rendezvous
- **Features:**
  - AES-128/192/256 encryption (AES-CTR default, AES-GCM authenticated encryption selectable via `crypto_mode`)
  - Stream ID access control (`stream_id`, max 512 chars per SRT spec; supports `#!::r=name,m=mode,u=user` structured format)
  - FEC (Forward Error Correction) via `packet_filter` — XOR-based row/column parity, staircase layout, ARQ integration modes (`always`/`onreq`/`never`), wire-compatible with libsrt v1.5.5
  - Configurable latency buffer (symmetric or asymmetric receiver/sender latency)
  - Retransmission bandwidth capping (Token Bucket shaper via `max_rexmit_bw`)
  - SMPTE 2022-7 hitless redundancy merge (dual-leg input)
  - Automatic reconnection
  - **`transport_mode: "audio_302m"`** (deferred — see [audio-gateway.md](audio-gateway.md#limitations-and-deferred-items)): demux SMPTE 302M LPCM-in-MPEG-TS from `ffmpeg -c:a s302m`, `srt-live-transmit`, or hardware encoders, and republish as RTP audio packets onto the broadcast channel

### RIST Simple Profile (VSF TR-06-1:2020)
- **Direction:** Input and Output
- **Transport:** UDP (RTP on even port P, RTCP on P+1) with RTCP NACK retransmission
- **Implementation:** Pure-Rust `bilbycast-rist` v0.2.0 (zero C deps, wire-verified interop with librist 0.2.11 `ristsender` / `ristreceiver`)
- **Features:**
  - Reliable RTP transport with NACK-based retransmission, configurable jitter / retransmit buffer depth
  - RTCP SR/RR/SDES and RTT echo, TR-06-1-compliant (RTCP interval ≤ 100 ms)
  - Dynamic RTCP source-address learning — no P+1 assumption on the receive side, so librist senders bound to ephemeral RTCP ports interop cleanly
  - SMPTE 2022-7 dual-leg hitless redundancy (two RistSockets merged via the shared `HitlessMerger`)
  - Payload carried as `RtpPacket { is_raw_ts: true }` — the RIST layer strips RTP before delivery, so the flow pipeline sees raw MPEG-TS
- **Limitations (v1):**
  - No PSK or DTLS encryption yet (stubbed in bilbycast-rist; use QUIC tunnels or a trusted network segment for confidentiality)
  - No FEC (RIST Main Profile / TR-06-2 is deferred pending a GRE-over-UDP encapsulation)
  - Input accepts both single-leg and SMPTE 2022-7 dual-leg; Output also supports SMPTE 2022-7 dual-leg (duplicates outbound packets to both sockets)

### `rtp_audio` (RFC 3551 PCM over RTP, no PTP)
- **Direction:** Input and Output
- **Transport:** UDP unicast or multicast, IPv4 and IPv6
- **Payload:** Big-endian L16 or L24 PCM in standard RFC 3551 RTP
- **Why it exists:** Wire-identical to SMPTE ST 2110-30 but with relaxed
  constraints — sample rates 32 / 44.1 / 48 / 88.2 / 96 kHz, no PTP
  requirement, no RFC 7273 timing reference, no NMOS `clock_domain`
  advertising. Use this for radio contribution feeds over the public
  internet, talkback between studios that don't share a PTP fabric, and
  ffmpeg / OBS / GStreamer interop where ST 2110-30's PTP assumption
  is overkill.
- **Features:**
  - Same `transcode` block as ST 2110-30 outputs (sample-rate / bit-depth
    / channel routing — see [audio-gateway.md](audio-gateway.md))
  - SMPTE 2022-7 dual-leg redundancy
  - Source IP allow-list filtering
  - **Output also supports** `transport_mode: "audio_302m"` to wrap the
    audio as SMPTE 302M-in-MPEG-TS inside RFC 2250 RTP/MP2T (PT 33)
    — useful for hardware decoders that consume MPEG-TS over RTP

### Bonded (multi-path aggregation)
- **Direction:** Input and Output
- **Transport:** N heterogeneous paths over UDP, QUIC (RFC 9221 DATAGRAM),
  or RIST Simple Profile, chosen per-path
- **Payload:** Any bytes — MPEG-TS, RTP, or raw — framed inside a 12-byte
  bond header with 32-bit sequence across all paths
- **Scheduler:** `media_aware` (default, walks H.264/HEVC NAL units and
  duplicates IDR frames across the two lowest-RTT paths), `weighted_rtt`,
  or `round_robin`
- **Why it exists:** Peplink-class path aggregation for broadcast —
  outperforms per-protocol bonding (SRT groups, RIST 2022-7) on mixed-link
  heterogeneous scenarios (5G + Starlink + fibre), with frame-accurate
  failover through IDR duplication rather than stream-wide doubling
- **Features:**
  - NACK-driven ARQ over a 32-bit sequence space with per-path retransmit
    targeting
  - Reassembly buffer with configurable `hold_ms` for late-arrival
    tolerance
  - Per-path keepalives → live RTT, jitter, loss, throughput stats
    exposed through the manager UI and Prometheus
  - QUIC paths negotiate ALPN `bilbycast-bond` (self-signed dev mode
    and production PEM mode both supported)
  - `program_number` filter on the output side for MPTS → SPTS
    down-selection before bonding
- **When not to use it:** For two SRT legs to the same SRT receiver use
  libsrt socket groups (configured on the SRT output `redundancy`
  block); for two RIST legs use RIST native SMPTE 2022-7. Bonded is the
  universal option for N ≥ 2 heterogeneous links carrying any inner
  protocol — see [bonding.md](bonding.md) for the full config schema,
  worked examples, and tuning guidance.

## Output Protocols

### RTP (SMPTE ST 2022-2)
- **Direction:** Output
- **Transport:** UDP unicast or multicast
- **Payload:** RTP-wrapped MPEG-TS packets with RTP headers
- **Features:**
  - SMPTE 2022-1 FEC encode
  - DSCP/QoS marking (default: 46 Expedited Forwarding)
  - Multicast with interface selection
  - **MPTS passthrough** (default) or optional MPTS→SPTS program filter via `program_number`

### UDP (Raw MPEG-TS)
- **Direction:** Output
- **Transport:** UDP unicast or multicast
- **Payload:** Raw MPEG-TS datagrams (7×188 = 1316 bytes, no RTP headers)
- **Features:**
  - Strips RTP headers when input is RTP-wrapped
  - TS sync detection and packet alignment
  - DSCP/QoS marking
  - Compatible with ffplay, VLC, multicast distribution
  - **MPTS passthrough** (default) or optional MPTS→SPTS program filter via `program_number`
  - **`transport_mode: "audio_302m"`**: when the upstream input is an audio
    essence, run the per-output transcode + SMPTE 302M packetizer +
    TsMuxer pipeline and emit 7×188-byte MPEG-TS chunks containing
    48 kHz LPCM (16/20/24 bit, 2/4/6/8 channels) as plain UDP datagrams.
    Useful for legacy hardware decoders that consume raw MPEG-TS over
    UDP. Mutually exclusive with `program_number`. See
    [audio-gateway.md](audio-gateway.md) for the full pipeline.

### SRT (Secure Reliable Transport)
- **Direction:** Output
- **Modes:** Caller, Listener, Rendezvous
- **Features:**
  - AES encryption (AES-CTR or AES-GCM via `crypto_mode`)
  - Stream ID access control (`stream_id`, max 512 chars; callers send during handshake, listeners filter)
  - FEC (Forward Error Correction) via `packet_filter` — same capabilities as SRT input
  - Asymmetric latency support (independent receiver/sender latency)
  - Retransmission bandwidth capping (Token Bucket shaper)
  - SMPTE 2022-7 hitless redundancy duplication (dual-leg output)
  - Non-blocking mpsc bridge prevents TCP backpressure from affecting other outputs
  - Automatic reconnection
  - **MPTS passthrough** (default) or optional MPTS→SPTS program filter via `program_number`
  - **`transport_mode: "audio_302m"`**: when the upstream input is an
    audio essence, run the per-output transcode (force 48 kHz, even
    channels, 16/20/24-bit) + SMPTE 302M packetizer + TsMuxer (BSSD
    registration descriptor in PMT) + 1316-byte chunk bundling
    pipeline. Interoperable with `ffmpeg -c:a s302m`,
    `srt-live-transmit`, and broadcast hardware decoders that expect
    302M LPCM in MPEG-TS over SRT. Mutually exclusive with
    `packet_filter`, `program_number`, and 2022-7 redundancy. See
    [audio-gateway.md](audio-gateway.md) for the full pipeline,
    interop tests, and worked use cases.

### RIST Simple Profile (VSF TR-06-1:2020)
- **Direction:** Output
- **Transport:** UDP (RTP on even port P, RTCP on P+1) with RTCP NACK retransmission
- **Implementation:** Pure-Rust `bilbycast-rist` v0.2.0 (zero C deps, wire-verified against librist 0.2.11)
- **Features:**
  - Reliable RTP transmission, NACK-driven retransmit with configurable buffer depth
  - RTCP SR/SDES emission, RTT echo, NTP-aligned RTP timestamps for precise receiver output timing
  - SMPTE 2022-7 dual-leg red/blue duplication (two RistSockets, one per `remote_addr`)
  - **MPTS passthrough** or optional MPTS→SPTS program filter via `program_number`
  - Shared `TsAudioReplacer` / `TsVideoReplacer` stages (same `audio_encode` / `video_encode` matrix as SRT / UDP / RTP outputs)
  - Output delay block for stream synchronisation (fixed ms, target latency ms, or target frames)

### RTMP/RTMPS
- **Direction:** Input (publish) and Output (publish)
- **Transport:** TCP (RTMP) or TLS over TCP (RTMPS)
- **Use case:** Delivering to Twitch, YouTube Live, Facebook Live; ingesting from OBS, Wirecast, ffmpeg
- **Features:**
  - Pure Rust RTMP protocol implementation (handshake, chunking, AMF0)
  - Demuxes H.264 and AAC from MPEG-2 TS, muxes into FLV
  - Automatic AVC sequence header (SPS/PPS) and AAC config generation
  - Reconnection with configurable delay and max attempts
  - Non-blocking: uses mpsc bridge pattern, never blocks other outputs
  - **MPTS-aware:** on an MPTS input, selects program by `program_number` or (default) locks onto the lowest-numbered program in the PAT
  - **Optional `audio_encode` block (Phase B):** runs the input AAC through the ffmpeg-sidecar encoder so the operator can normalise bitrate / sample rate / channel count or upgrade to HE-AAC v1/v2 (`aac_lc`, `he_aac_v1`, `he_aac_v2`). Same-codec passthrough fast path skips both decoder and encoder when the source is already AAC-LC and no overrides are set (disabled when `transcode` is set). Requires ffmpeg in PATH at runtime; outputs without `audio_encode` keep working without ffmpeg installed. See [audio-gateway.md](audio-gateway.md#the-audio_encode-block--compressed-audio-egress-rtmp--hls--webrtc).
  - **Optional companion `transcode` block:** channel shuffle / sample-rate conversion applied to the decoded PCM before re-encoding. See [transcoding.md](transcoding.md#transcode--channel-shuffle--sample-rate-conversion).
- **Limitations:**
  - Only H.264 video and AAC audio. HEVC/VP9 not supported via RTMP.
  - RTMPS (TLS) uses the `tls` feature (enabled by default).
  - Single-program by spec — only one program can be published per output.

### CMAF / CMAF-LL
- **Direction:** Output only
- **Transport:** HTTP PUT (standard) or HTTP PUT with chunked transfer encoding (LL-CMAF)
- **Use case:** AWS MediaStore, Fastly OA, Akamai MSL, Wowza, nimble — any modern CDN ingest accepting fragmented-MP4 push
- **Features:**
  - ISO/IEC 23000-19 CMAF media profile (`cmfc` brand) with hand-rolled fMP4 muxer (no external MP4 crate dep)
  - **Video:** H.264 + HEVC passthrough; optional `video_encode` block (libx264 / libx265 / NVENC, feature-gated) with explicit GoP alignment to `segment_duration_secs × fps` so segments always cut on IDR
  - **Audio:** AAC-LC / HE-AAC v1 / HE-AAC v2 passthrough or re-encode (in-process via fdk-aac)
  - **Manifests:** HLS `manifest.m3u8` and/or DASH `manifest.mpd` over the same fMP4 segments — emit either or both
  - **Low-Latency CMAF:** `low_latency: true` + `chunk_duration_ms` (100-2000) emits one `moof+mdat` chunk per chunk_duration into a single chunked-transfer PUT per segment; HLS `#EXT-X-PART` rows + DASH `availabilityTimeOffset` advertise parts to LL-aware players. Targets <3 s glass-to-glass with 500 ms chunks
  - **CENC encryption** (ISO/IEC 23001-7): `cenc` (AES-128 CTR) or `cbcs` (AES-128 CBC pattern 1:9 — FairPlay) with subsample encryption for H.264 / HEVC video, whole-sample encryption for AAC. ClearKey `pssh` emitted automatically; operators may add Widevine / PlayReady / FairPlay PSSH boxes via `pssh_boxes` (verbatim wrap into `moov`)
  - **MPTS passthrough** or optional MPTS→SPTS program filter via `program_number`
  - Optional Bearer token authentication via `auth_token`
  - Codec work runs in `tokio::task::block_in_place`; LL chunked PUT uses `mpsc(8)` with `try_send` + drop-on-full so the broadcast subscriber is never blocked
- **Limitations:**
  - Output only. Standard mode adds 1-4 s latency; LL targets <3 s
  - Source must emit IDR at least every `segment_duration_secs` unless `video_encode` is set
  - LL ingests must accept HTTP/1.1 chunked transfer encoding on PUT (every major CDN does; static nginx does not)
- **Reference:** [`docs/cmaf.md`](cmaf.md)

### HLS Ingest
- **Direction:** Output only
- **Transport:** HTTP PUT/POST over TCP
- **Use case:** YouTube HLS ingest (supports HEVC/HDR content)
- **Features:**
  - Segments MPEG-2 TS data into time-bounded chunks
  - Generates rolling M3U8 playlist
  - Configurable segment duration (0.5-10 seconds)
  - Optional Bearer token authentication
  - Async HTTP upload, non-blocking to other outputs
  - **MPTS passthrough** (default) or optional MPTS→SPTS program filter via `program_number` — filtered segments carry a rewritten single-program TS
  - **Optional `audio_encode` block (Phase B):** each segment is piped through `ffmpeg -i pipe:0 -c:v copy -c:a {codec} -f mpegts pipe:1` before HTTP PUT. Allowed codecs: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`. Per-segment fork rather than a long-lived encoder because HLS segments are 2-6 s and ffmpeg startup is small relative to that — also lets MP2/AC-3 work without a new TS muxer. Requires ffmpeg in PATH; the output refuses to start if ffmpeg is missing and emits a Critical `audio_encode` event.
  - **Optional companion `transcode` block:** channel shuffle / sample-rate conversion applied to the decoded PCM before re-encoding. Honoured only on the in-process remux path (`video-thumbnail` feature, default); the subprocess fallback ignores it with a warning. See [transcoding.md](transcoding.md#transcode--channel-shuffle--sample-rate-conversion).
- **Limitations:**
  - Output only. Segment-based transport inherently adds 1-4 seconds of latency.
  - Uses a minimal built-in HTTP client (not a full HTTP/2 client).

### RTSP
- **Direction:** Input only
- **Transport:** TCP (interleaved) or UDP
- **Client library:** `retina` (pure Rust)
- **Features:**
  - Pull H.264 or H.265/HEVC video and AAC audio from IP cameras and media servers
  - Digest and Basic authentication support
  - TCP interleaved (default, works through firewalls) or UDP transport
  - Automatic reconnection with configurable delay
  - Received media is muxed into MPEG-TS with proper PAT/PMT program tables
  - Audio-only streams supported (PAT/PMT emitted even without video)
- **Limitations:**
  - Input only (no RTSP server mode)
  - AAC audio is passed through as ADTS in MPEG-TS

### SMPTE ST 2110-30 / -31 (PCM / AES3 audio)
- **Direction:** Input and Output
- **Transport:** RFC 3551 RTP/UDP, multicast or unicast, with
  IGMP/MLD multicast group join and interface selection
- **Payload:** Big-endian L16/L24 PCM (-30) or AES3 sub-frames (-31,
  always 24-bit, transparent to Dolby E and AES3 user/channel-status
  bits)
- **Wire constraints:** sample_rate 48000 / 96000 Hz; bit_depth 16 or
  24; channels 1, 2, 4, 8, 16; packet_time 125 / 250 / 333 / 500 / 1000
  / 4000 µs; payload_type 96–127
- **PTP:** best-effort PTP slave via the external `ptp4l` daemon's
  management Unix socket. PTP state surfaces through stats; no
  in-process PTP daemon ships with the edge.
- **Features:**
  - SMPTE 2022-7 dual-network ("Red"/"Blue") bind on input and output,
    with hitless merge on input via `engine::st2110::redblue::RedBluePair`
  - Source IP allow-list filtering (single-leg only)
  - **Per-output PCM transcode** (`transcode` block): sample-rate / bit-depth
    / channel-routing conversion via the lock-free `engine::audio_transcode`
    stage. IS-08 channel-map activations propagate without flow restart
    via a `tokio::sync::watch` channel — see
    [audio-gateway.md](audio-gateway.md) for the full feature set,
    presets, and worked examples.
- **NMOS:** advertised as `urn:x-nmos:format:audio` in IS-04, with
  BCP-004 receiver caps reflecting the configured sample rate, channel
  count, and sample depth

### SMPTE ST 2110-40 (ancillary data)
- **Direction:** Input and Output
- **Transport:** RFC 8331 RTP/UDP
- **Payload:** Bit-packed RFC 8331 ANC (SCTE-104 ad markers, SMPTE 12M
  timecode, CEA-608/708 captions, AFD, CDP)
- **Features:**
  - Same SMPTE 2022-7 dual-network and source allow-list options as -30/-31
  - SCTE-104 messages auto-detected and emitted as `scte104` events
- **NMOS:** advertised as `urn:x-nmos:format:data` in IS-04

### SMPTE ST 2110-20 (uncompressed video, Phase 2)
- **Direction:** Input and Output
- **Transport:** RFC 4175 RTP/UDP
- **Pixel formats:** YCbCr-4:2:2 at 8-bit (pgroup=4 bytes/2 pixels) and
  10-bit LE (pgroup=5 bytes/2 pixels). Other RFC 4175 formats are
  validated-and-rejected in Phase 2.
- **Non-blocking pipeline:**
  - **Input:** RedBluePair → `Rfc4175Depacketizer` → bounded mpsc →
    `spawn_blocking` worker running `video-engine::VideoEncoder`
    (x264/x265/NVENC, selected by `video_encode.codec`) →
    `TsMuxer` → `RtpPacket { is_raw_ts: true }` onto the flow's
    broadcast. The `video_encode` block is **mandatory** — ingress is
    always a pixel-to-compressed conversion.
  - **Output:** broadcast → `TsDemuxer` → bounded mpsc →
    `spawn_blocking` worker running `video-engine::VideoDecoder` +
    `VideoScaler::new_with_dst_format` (new 4:2:2 8/10-bit targets)
    → pgroup pack → `Rfc4175Packetizer` → `UdpSocket::send_to`
    (Red + optional Blue). No encoder block is accepted.
- **2022-7 Red/Blue:** per-input and per-output via the existing
  `redundancy` field.
- **Feature gating:** ST 2110-20 **inputs** require a compile-time
  `video-encoder-*` feature (x264 / x265 / NVENC) or the
  `video-encoders-full` composite. The default `cargo build` has no
  software video encoders, so it will deserialize the config but the
  encode worker logs an error and drops frames. The `*-linux-full`
  release binary includes all three encoders. ST 2110-20 **outputs**
  only need the `video-thumbnail` feature (default on).
- **NMOS:** advertised as `urn:x-nmos:format:video` in IS-04 with
  BCP-004 receiver caps declaring `video/raw`,
  `color_sampling=YCbCr-4:2:2`, `component_depth` ∈ {8, 10},
  `frame_width`, `frame_height`, and `grain_rate` (from the configured
  `frame_rate_num`/`frame_rate_den`).

### SMPTE ST 2110-23 (single video essence over multiple -20 streams)
- **Direction:** Input and Output
- **Transport:** N independent RFC 4175 RTP/UDP sub-streams carrying
  partitions of one essence
- **Partition modes (Phase 2):** `two_sample_interleave` (2SI),
  `sample_row`. `sample_column` is validated-and-rejected.
- **Reassembler / partitioner:** lives in
  `src/engine/st2110/video.rs` and reuses the same encode / decode
  workers as ST 2110-20 once the essence is reassembled / before it
  is split. The reassembler is timestamp-keyed with `max_in_flight=4`
  to bound memory under asymmetric loss.
- **2022-7 Red/Blue:** configurable per sub-stream.
- **NMOS:** advertised as `urn:x-nmos:format:video` with BCP-004 caps
  identical to the -20 shape (the multi-stream decomposition is a
  transport detail, not a format difference).

### WebRTC (WHIP/WHEP)
- **Direction:** Input and Output
- **Transport:** UDP (ICE-lite/DTLS/SRTP) via `str0m` pure-Rust WebRTC stack
- **Status:** Fully implemented. The `webrtc` feature is enabled by default.
- **Four modes:**
  - **WHIP input** (server): Accept contributions from OBS, browsers — endpoint at `/api/v1/flows/{id}/whip`
  - **WHIP output** (client): Push media to external WHIP endpoints (CDN, cloud)
  - **WHEP output** (server): Serve browser viewers — endpoint at `/api/v1/flows/{id}/whep`
  - **WHEP input** (client): Pull media from external WHEP servers
- **Video:** H.264 only (RFC 6184 RTP packetization/depacketization)
- **Audio:** Opus passthrough by default. Opus flows natively on WebRTC paths and gets muxed into MPEG-TS for SRT/RTP/UDP outputs. **Without `audio_encode`, AAC sources going to a WebRTC output automatically fall back to video-only.** Setting an `audio_encode` block (codec: `opus`) enables the Phase B chain: input AAC is decoded in-process via the Phase A `engine::audio_decode::AacDecoder` (FDK AAC by default, supporting AAC-LC/HE-AAC v1/v2/multichannel) and re-encoded as Opus via the Phase B `engine::audio_encode::AudioEncoder` (ffmpeg subprocess for Opus), then written to the WebRTC audio MID via str0m. This is the marquee Phase A+B chain — **AAC RTMP contribution → Opus WebRTC distribution** — all inside one bilbycast-edge process with no external transcoder. Requires `video_only=false` and ffmpeg in PATH (for Opus encoding). The WebRTC output also accepts an optional companion `transcode` block: `transcode.channels` overrides the Opus encoder's channel count (unset keeps the source), and the full channel-routing matrix / per-channel gain surface works identically to the other outputs — see [transcoding.md](transcoding.md#transcode--channel-shuffle--sample-rate-conversion).
- **MPTS-aware outputs:** on an MPTS input, WHIP/WHEP outputs select program by `program_number` or (default) lock onto the lowest-numbered program in the PAT. Single-program by spec.
- **Interoperability:** Compatible with OBS, browsers, Cloudflare, LiveKit, and other standard WHIP/WHEP implementations.
- **Security:** Bearer token authentication on WHIP/WHEP endpoints, DTLS/SRTP encryption, ICE-lite for server modes.
- **NAT traversal:** Configurable `public_ip` and optional `stun_server` for ICE candidate advertisement.

## MPEG-TS Program Handling

### MPTS / SPTS Support
- **All TS-carrying inputs** (UDP, RTP, SRT) accept both **SPTS** (single-program) and **MPTS** (multi-program) transport streams. Non-TS inputs (RTMP, RTSP, WebRTC) synthesise a single-program TS internally.
- **TS-native outputs** (UDP, RTP, SRT, HLS) pass the full MPTS through by default. Each output may opt into a per-program filter via `program_number`, which rewrites the PAT to a single-program form and drops packets for every PID that isn't part of the selected program. FEC (2022-1) and hitless redundancy (2022-7) protect the filtered bytes, so receivers see a valid SPTS.
- **Re-muxing outputs** (RTMP, WebRTC) and the **thumbnail generator** honour the same `program_number` field to select which program's elementary streams to extract. The default (unset) locks onto the lowest `program_number` in the PAT for deterministic behaviour.
- **Per-output scope:** one flow can fan an MPTS out to multiple outputs, each locked onto a different program, while a sibling output forwards the full MPTS unchanged.
- See [configuration-guide.md — MPTS → SPTS filtering](configuration-guide.md#mpts--spts-filtering) for the full behaviour matrix.

### Flow Assembly (PID bus — build fresh SPTS / MPTS from N inputs)

A flow can optionally carry an `assembly` block (`kind = spts | mpts | passthrough`) that builds a fresh MPEG-TS from elementary streams pulled off any of the flow's inputs. Every output type consumes the assembled TS unchanged — there is **no output-type gate** (any of UDP / RTP / SRT / RIST / RTMP / HLS / CMAF / CMAF-LL / WebRTC works). Slot sources are `pid` (explicit `(input_id, source_pid)`), `essence` (first video/audio/subtitle/data off a named input), or `hitless` (primary-preference failover merger; not 2022-7 seq-aware dedup today). PCM / AES3 inputs (ST 2110-30, ST 2110-31, `rtp_audio`) become TS carriers for the bus when their input-level `audio_encode` is set (`aac_lc` / `he_aac_v1` / `he_aac_v2` / `s302m`). Runtime hot-swap via `UpdateFlowAssembly` (PMT version bumps mod 32, PAT only when the program set changes). See [configuration-guide.md — Flow Assembly (PID bus)](configuration-guide.md#flow-assembly-pid-bus--spts--mpts-from-n-inputs) for the full schema and rules.

### TS output PID remap (`pid_map`)
- TS-carrying outputs (`udp`, `rtp`, `srt`, `rist`, `hls`, `bonded`) accept an optional `pid_map: { src: tgt }` that rewrites PIDs on egress (PAT / PMT CRCs recomputed). ≤ 256 entries, PIDs in `0x0010..=0x1FFE`, bijective (no source or target collides with another), source ≠ target. Use for downstream systems with hard-coded PID expectations that don't match the upstream (or assembly's `out_pid`) layout.

## Monitoring and Analysis

### TR-101290 Transport Stream Analysis
- Priority 1: Sync byte, continuity counter, PAT/PMT presence
- Priority 2: TEI, PCR discontinuity, PCR accuracy
- Runs as independent broadcast subscriber (zero jitter impact)

### MPEG-TS Program Analysis
- Parses PAT and every PMT from the input broadcast channel
- Reports **per-program stream breakdown** in the manager UI: each program's video PIDs (codec, resolution, frame rate, profile, level) and audio PIDs (codec, sample rate, channels, language)
- Handles PAT/PMT version bumps and programs coming/going mid-stream
- `program_count` in stats reflects the number of programs advertised in the PAT

### VSF TR-07 Awareness
- Auto-detects JPEG XS (stream type 0x61) in PMT
- Reports TR-07 compliance status via API and dashboard
- Enable with `tr07_mode: true` in RTP input config

### SMPTE Trust Boundary Metrics (RP 2129)
- Inter-arrival time (IAT): min/max/avg per reporting window
- Packet delay variation (PDV/jitter): RFC 3550 exponential moving average
- Filtered PDV (CMAX): peak-to-peak filtered metric
- Source IP filtering (C5), payload type filtering (U4), rate limiting (C7)
- DSCP/QoS marking (C10)
- Flow health derivation (M6): Healthy/Warning/Error/Critical

## Cargo Features

| Feature | Description | Default |
|---------|-------------|---------|
| `tls` | Enable RTMPS (RTMP over TLS) via `rustls`/`tokio-rustls` | **Yes** |
| `webrtc` | Enable WebRTC WHIP/WHEP input and output via `str0m` | **Yes** |

All features are enabled by default. A plain `cargo build --release` includes everything.

## Configuration Examples

See the `config_examples/` directory for JSON configuration examples for each protocol type.

## Architecture Notes

- All outputs subscribe to a shared broadcast channel independently
- Slow outputs receive `Lagged` errors and drop packets — they never block the input or other outputs
- TCP-based outputs (RTMP, HLS) use an mpsc bridge pattern to keep TCP operations off the broadcast receive path
- All monitoring (TR-101290, IAT/PDV, TR-07) runs on independent analyzer tasks with zero impact on the data plane
- **Seamless input switching:** flows with multiple inputs use a TS continuity fixer that resets CC state on switch (creating a clean-break CC jump), injects the new input's cached PAT/PMT (version-bumped to force receivers to re-parse), forwards all packets immediately, and — for inputs with ingress `video_encode` — asks the re-encoder to emit an IDR on its first post-switch frame so decoders can resync without waiting for a natural keyframe. **Fully format-agnostic — inputs can use different codecs, containers, and transports** (e.g., H.264 on one input, H.265 on another, JPEG XS on a third, uncompressed ST 2110 on a fourth). Works for any video (H.264, HEVC, JPEG XS, JPEG 2000, uncompressed), any audio (AAC, HE-AAC, Opus, MP2, AC-3, LPCM, SMPTE 302M, ST 2110-30/-31), ancillary data (ST 2110-40), or any future format. For non-TS transports (raw ST 2110 RTP), the fixer is transparent — the switch mechanism works identically. Visible switch latency is one to two frames for both passthrough and ingress-transcoded inputs. Zero overhead on single-input flows.
