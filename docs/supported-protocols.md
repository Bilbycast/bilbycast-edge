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
  - AES-128/192/256 encryption
  - Configurable latency buffer
  - SMPTE 2022-7 hitless redundancy merge (dual-leg input)
  - Automatic reconnection

## Output Protocols

### RTP (SMPTE ST 2022-2)
- **Direction:** Output
- **Transport:** UDP unicast or multicast
- **Payload:** RTP-wrapped MPEG-TS packets with RTP headers
- **Features:**
  - SMPTE 2022-1 FEC encode
  - DSCP/QoS marking (default: 46 Expedited Forwarding)
  - Multicast with interface selection

### UDP (Raw MPEG-TS)
- **Direction:** Output
- **Transport:** UDP unicast or multicast
- **Payload:** Raw MPEG-TS datagrams (7×188 = 1316 bytes, no RTP headers)
- **Features:**
  - Strips RTP headers when input is RTP-wrapped
  - TS sync detection and packet alignment
  - DSCP/QoS marking
  - Compatible with ffplay, VLC, multicast distribution

### SRT (Secure Reliable Transport)
- **Direction:** Output
- **Modes:** Caller, Listener, Rendezvous
- **Features:**
  - AES encryption
  - SMPTE 2022-7 hitless redundancy duplication (dual-leg output)
  - Non-blocking mpsc bridge prevents TCP backpressure from affecting other outputs
  - Automatic reconnection

### RTMP/RTMPS
- **Direction:** Output only (publish)
- **Transport:** TCP (RTMP) or TLS over TCP (RTMPS)
- **Use case:** Delivering to Twitch, YouTube Live, Facebook Live
- **Features:**
  - Pure Rust RTMP protocol implementation (handshake, chunking, AMF0)
  - Demuxes H.264 and AAC from MPEG-2 TS, muxes into FLV
  - Automatic AVC sequence header (SPS/PPS) and AAC config generation
  - Reconnection with configurable delay and max attempts
  - Non-blocking: uses mpsc bridge pattern, never blocks other outputs
- **Limitations:**
  - Output only. RTMP input (ingest from OBS etc.) is not implemented.
  - Only H.264 video and AAC audio. HEVC/VP9 not supported via RTMP.
  - RTMPS (TLS) requires building with the `tls` cargo feature.

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

### WebRTC (WHIP/WHEP)
- **Direction:** Input and Output
- **Transport:** UDP (ICE-lite/DTLS/SRTP) via `str0m` pure-Rust WebRTC stack
- **Status:** Fully implemented. Requires the `webrtc` cargo feature.
- **Four modes:**
  - **WHIP input** (server): Accept contributions from OBS, browsers — endpoint at `/api/v1/flows/{id}/whip`
  - **WHIP output** (client): Push media to external WHIP endpoints (CDN, cloud)
  - **WHEP output** (server): Serve browser viewers — endpoint at `/api/v1/flows/{id}/whep`
  - **WHEP input** (client): Pull media from external WHEP servers
- **Video:** H.264 only (RFC 6184 RTP packetization/depacketization)
- **Audio:** Opus passthrough. Opus flows natively on WebRTC paths and gets muxed into MPEG-TS for SRT/RTP/UDP outputs. AAC sources going to WebRTC output automatically fall back to video-only (no C-library transcoding).
- **Interoperability:** Compatible with OBS, browsers, Cloudflare, LiveKit, and other standard WHIP/WHEP implementations.
- **Security:** Bearer token authentication on WHIP/WHEP endpoints, DTLS/SRTP encryption, ICE-lite for server modes.
- **NAT traversal:** Configurable `public_ip` and optional `stun_server` for ICE candidate advertisement.

## Monitoring and Analysis

### TR-101290 Transport Stream Analysis
- Priority 1: Sync byte, continuity counter, PAT/PMT presence
- Priority 2: TEI, PCR discontinuity, PCR accuracy
- Runs as independent broadcast subscriber (zero jitter impact)

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
| `tls` | Enable RTMPS (RTMP over TLS) via `rustls`/`tokio-rustls` | No |
| `webrtc` | Enable WebRTC WHIP/WHEP input and output via `str0m` | No |

Build with all features:
```bash
cargo build --release --features tls,webrtc
```

## Configuration Examples

See the `config_examples/` directory for JSON configuration examples for each protocol type.

## Architecture Notes

- All outputs subscribe to a shared broadcast channel independently
- Slow outputs receive `Lagged` errors and drop packets — they never block the input or other outputs
- TCP-based outputs (RTMP, HLS) use an mpsc bridge pattern to keep TCP operations off the broadcast receive path
- All monitoring (TR-101290, IAT/PDV, TR-07) runs on independent analyzer tasks with zero impact on the data plane
