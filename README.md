# bilbycast-edge

> 🌐 Learn more at **[bilbycast.com](https://bilbycast.com)** — the official website for the Bilbycast broadcast media transport suite.

Media transport edge node supporting SRT, RIST Simple Profile (VSF TR-06-1), RTP, UDP, RTMP, RTSP, HLS, and WebRTC (WHIP/WHEP) protocols, plus SMPTE ST 2110-30/-31/-40 (Phase 1 audio + data) and ST 2110-20/-23 (Phase 2 uncompressed video, RFC 4175 YCbCr-4:2:2 at 8/10-bit). Inputs, outputs, and flows are independently configurable top-level entities: inputs and outputs live in their own config arrays with stable IDs, and flows reference them via `input_ids` + `output_ids`. A flow may have multiple inputs (one active at a time, zero-gap seamless switching via `POST /api/v1/flows/{id}/activate-input`) fanning out to multiple outputs, with SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy (including 2022-7 Red/Blue dual-network support for ST 2110).

Full MPTS (multi-program transport stream) support end-to-end on UDP/RTP/SRT/HLS with optional per-output `program_number` down-selection for extracting a single program as a rewritten SPTS. RTMP/WebRTC outputs and the thumbnail generator lock onto a chosen program deterministically. A flow-level `thumbnail_program_number` picks which program feeds the manager UI preview.

Supports NMOS IS-04 (Discovery & Registration), IS-05 (Connection Management), IS-08 (Audio Channel Mapping), and BCP-004 receiver capabilities — optionally auth-protected — with mDNS-SD `_nmos-node._tcp` registration for integration with broadcast control systems. Exposes Prometheus `/metrics` for monitoring and WHIP/WHEP signaling endpoints for WebRTC.

## Supported Protocols

| Protocol | Input | Output | Notes                                          |
|----------|-------|--------|-------------------------------------------------|
| SRT      | Yes   | Yes    | Caller, listener, rendezvous modes; AES-128/192/256 encryption (CTR or GCM); Stream ID access control; SRT FEC (row/col/staircase, ARQ modes); 2022-7 redundancy. Optional `audio_encode` / `video_encode` + companion `transcode` blocks. **`transport_mode: "audio_302m"`** for SMPTE 302M LPCM-in-MPEG-TS (output side; input demux deferred) |
| RIST     | Yes   | Yes    | Simple Profile (VSF TR-06-1:2020), RTCP NACK-driven retransmission, dual-leg 2022-7, pure-Rust `bilbycast-rist`, wire-verified interop with librist 0.2.11. Same optional `audio_encode` / `video_encode` / `transcode` as SRT |
| RTP      | Yes   | Yes    | RTP-wrapped over UDP; unicast and multicast; SMPTE 2022-1 FEC; DSCP QoS; optional `audio_encode` / `video_encode` / `transcode` |
| UDP      | Yes   | Yes    | Raw MPEG-TS over UDP; unicast and multicast; DSCP QoS marking; optional `audio_encode` / `video_encode` / `transcode`. **`transport_mode: "audio_302m"`** for SMPTE 302M LPCM-in-MPEG-TS |
| RTMP     | Yes   | Yes    | H.264/AAC + HEVC via Enhanced RTMP v2 (`hvc1`); accepts publish from OBS/ffmpeg; supports RTMPS (TLS); optional `audio_encode` (AAC-LC / HE-AAC v1/v2) and `video_encode` (H.264 / HEVC via libx264 / libx265 / NVENC) |
| RTSP     | Yes   | No     | Pull H.264/H.265 from IP cameras/media servers; TCP/UDP transport; auto-reconnect |
| HLS      | No    | Yes    | Segment-based egress; supports HEVC/HDR; in-process TS audio remuxing (default `video-thumbnail` feature); optional `audio_encode` per-segment re-encode (AAC family / MP2 / AC-3) |
| WebRTC   | Yes   | Yes    | WHIP/WHEP (both client and server modes) via pure-Rust `str0m`; H.264 video + Opus audio (enabled by default); optional `audio_encode` (AAC → Opus in-process) and `video_encode` (H.264-only target — HEVC source auto-decoded and re-encoded to H.264 so browsers can play it) |
| **SMPTE ST 2110-30** | Yes | Yes | RFC 3551 PCM audio (L16/L24); 2022-7 Red/Blue dual-network; PTP slave via external `ptp4l`. Optional per-output `transcode` block (sample-rate / bit-depth / channel routing) |
| **SMPTE ST 2110-31** | Yes | Yes | AES3 transparent audio (24-bit), preserves Dolby E and AES3 sub-frame bits |
| **SMPTE ST 2110-40** | Yes | Yes | RFC 8331 ancillary data (SCTE-104, SMPTE 12M timecode, CEA-608/708 captions) |
| **SMPTE ST 2110-20** | Yes | Yes | Uncompressed video (RFC 4175 YCbCr-4:2:2 at 8/10-bit); 2022-7 Red/Blue dual-network. Inputs re-encode to H.264/HEVC via `video_encode` (requires a compiled-in software/NVENC encoder); outputs decode and scale to 4:2:2 planar for RFC 4175 packetization |
| **SMPTE ST 2110-23** | Yes | Yes | Multi-stream single-essence video — 2SI and sample-row partition modes; otherwise same pipeline as -20 |
| **`rtp_audio`** | Yes | Yes | Generic RFC 3551 PCM-over-RTP — wire-identical to ST 2110-30 but **no PTP requirement**, sample rates 32 / 44.1 / 48 / 88.2 / 96 kHz. For radio contribution, talkback, ffmpeg / OBS interop. Same `transcode` block as ST 2110-30. Output supports `transport_mode: "audio_302m"` (RTP/MP2T encapsulation per RFC 2250) |
| **Media Player** | Yes | No | File-backed input — replays MPEG-TS / MP4 / MOV / MKV / still images from the edge's media library as a paced fresh MPEG-TS feed. Playlist with `loop_playback` + `shuffle`. 4 GiB per file, 16 GiB library cap, library path resolved via `BILBYCAST_MEDIA_DIR`. Files are uploaded over the manager's chunked REST endpoint (`POST /api/v1/nodes/{id}/media/upload`). Designed to drop onto a PID-bus Hitless leg as an automatic fallback to a live primary |

**Compressed-audio bridge (Phase A + Phase B):** AAC contribution audio
carried in MPEG-TS over RTMP / RTSP / SRT / UDP / RTP can be decoded
in-process (Phase A) and either land into the PCM-only ST 2110 /
`rtp_audio` / SMPTE 302M outputs, or be re-encoded (Phase B) into AAC,
HE-AAC v1/v2, Opus, MP2, or AC-3 for the RTMP, HLS, WebRTC, **and SRT /
RIST / UDP / RTP TS outputs**. TS outputs rewrite only the audio ES and
PMT stream_type in place via the streaming `TsAudioReplacer`; video and
other PIDs pass through untouched. Every output that accepts
`audio_encode` also accepts an optional companion `transcode` block
(channel shuffle / sample-rate / bit-depth conversion) — `transcode`
wins on conflicting fields.
**Default (`fdk-aac` + `video-thumbnail` features):** decode and AAC
encode use Fraunhofer FDK AAC in-process (via `bilbycast-fdk-aac-rs`) —
AAC-LC, HE-AAC v1/v2, multichannel up to 7.1. Opus / MP2 / AC-3 use
in-process libavcodec (via `bilbycast-ffmpeg-video-rs`). **No external
ffmpeg binary required.** **Fallback (features disabled):** decode uses
`symphonia-codec-aac` (pure Rust, AAC-LC mono/stereo only); encode uses
ffmpeg subprocess. The marquee chain is **AAC RTMP contribution → Opus
WebRTC distribution** in a single bilbycast-edge process. See
[`docs/audio-gateway.md`](docs/audio-gateway.md).

**Video transcoding (Phase 4):** SRT, RIST, UDP, RTP, RTMP, **and
WebRTC** outputs accept an optional `video_encode` block. TS-carrying
outputs run `TsVideoReplacer` (source H.264/HEVC → decode → re-encode →
re-mux, with a fresh PMT when the codec family changes); RTMP emits
classic FLV for H.264 and Enhanced RTMP v2 `hvc1` for HEVC; WebRTC is
H.264-only target (browsers do not decode HEVC — HEVC source is
auto-transcoded to H.264). HLS `video_encode` is the only remaining
deferred transport. Software encoders (libx264 / libx265) are opt-in
via the `video-encoder-x264` / `video-encoder-x265` Cargo features and
are GPL-2.0-or-later — binaries built with them are an AGPL-3.0-or-later
combined work. NVENC (`video-encoder-nvenc`) is LGPL-clean at the API
layer. The composite `video-encoders-full` feature bundles all three
and is used by the `*-linux-full` release channel. See
[`docs/transcoding.md`](docs/transcoding.md) for the licensing and
capability matrix.

**Output delay:** SRT, RIST, RTP, and UDP outputs accept an optional
`delay` block for synchronizing parallel output paths with different
processing latencies (e.g., a clean feed alongside a
commentary-processed feed). Three modes: `fixed` (constant ms delay),
`target_ms` (target end-to-end latency, self-adjusting), `target_frames`
(target latency in video frames using auto-detected frame rate).

## Quick Start

### Option 1: Standalone (no monitoring)

1. **Prerequisites**: Install the [Rust toolchain](https://rustup.rs/) (stable, edition 2024).

2. **Build**:
   ```bash
   cargo build --release
   ```

3. **Create a config file** (`config.json`) with independent inputs, outputs, and flows that reference them:
   ```json
   {
     "version": 2,
     "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
     "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
     "inputs": [
       {
         "id": "srt-in-1",
         "name": "SRT Listener",
         "active": true,
         "type": "srt",
         "mode": "listener",
         "local_addr": "0.0.0.0:9000",
         "latency_ms": 120
       }
     ],
     "outputs": [
       {
         "id": "rtp-out-1",
         "name": "RTP Output",
         "active": true,
         "type": "rtp",
         "dest_addr": "192.168.1.100:5004"
       }
     ],
     "flows": [
       {
         "id": "srt-to-rtp",
         "name": "SRT Input to RTP Output",
         "enabled": true,
         "input_ids": ["srt-in-1"],
         "output_ids": ["rtp-out-1"]
       }
     ]
   }
   ```

4. **Start the node**:
   ```bash
   ./target/release/bilbycast-edge --config config.json
   ```

5. **Access the monitor dashboard** at `http://localhost:9090` (requires `monitor` section in config).

6. **Access the REST API** at `http://localhost:8080`.

### Option 2: With API Authentication (for Prometheus / external monitoring)

1. Follow build steps from Option 1.

2. **Create a config file** (`config.json`) with server settings, and a **secrets file** (`secrets.json`) with auth credentials:

   **config.json**:
   ```json
   {
     "version": 2,
     "server": {
       "listen_addr": "0.0.0.0",
       "listen_port": 8080
     },
     "inputs": [],
     "outputs": [],
     "flows": []
   }
   ```

   **secrets.json** (set `chmod 600 secrets.json`):
   ```json
   {
     "version": 2,
     "server_auth": {
       "enabled": true,
       "jwt_secret": "your-secret-key-at-least-32-characters",
       "token_lifetime_secs": 3600,
       "clients": [
         {
           "client_id": "prometheus",
           "client_secret": "a-strong-random-secret",
           "role": "monitor"
         },
         {
           "client_id": "admin-tool",
           "client_secret": "another-strong-secret",
           "role": "admin"
         }
       ],
       "public_metrics": true
     }
   }
   ```

   > **Tip**: You can also place the `auth` section inside `config.json` under `server` for convenience — it will be automatically migrated to `secrets.json` on first startup.

3. **Start the node**.

4. **Obtain a JWT token** via the OAuth 2.0 client credentials endpoint:
   ```bash
   curl -X POST http://localhost:8080/oauth/token \
     -d 'grant_type=client_credentials&client_id=prometheus&client_secret=a-strong-random-secret'
   ```

5. **Use the token** to access protected API endpoints:
   ```bash
   curl -H "Authorization: Bearer <token>" http://localhost:8080/api/v1/stats
   ```

6. **Configure Prometheus** to scrape `/metrics` with the Bearer token, or integrate with any monitoring system that supports REST API with JWT auth.

7. When `public_metrics` is `true`, `/metrics` and `/health` are accessible without authentication.

### Option 3: Connected to bilbycast-manager

1. Follow build steps from Option 1.

2. **Get a registration token** from the manager (Dashboard -- register a new node, or use the manager API to create a node entry).

3. **Create a config file** with the `manager` section:

   **config.json**:
   ```json
   {
     "version": 2,
     "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
     "manager": {
       "enabled": true,
       "url": "wss://manager-host:8443/ws/node"
     },
     "inputs": [],
     "outputs": [],
     "flows": []
   }
   ```

   **secrets.json** (optional — or use the setup wizard instead):
   ```json
   {
     "version": 2,
     "manager_registration_token": "<token-from-manager>"
   }
   ```

   > **Tip**: You can also place the `registration_token` inside `config.json` under `manager` — it will be automatically migrated to `secrets.json` on first startup.

4. **Start the node** -- it connects to the manager, authenticates with the registration token, and receives a permanent `node_id` and `node_secret`.

5. **The node appears** in the manager dashboard and can be configured and monitored remotely. Commands from the manager (create/update/delete input, create/update/delete output, create/update/delete flow, start/stop flow, rotate secret) are executed automatically.

6. **Credentials are saved automatically** after registration: `node_id` goes to `config.json`, `node_secret` goes to `secrets.json` (with `0600` permissions). The `registration_token` is cleared. On subsequent starts, the node reconnects using the saved credentials. The connection uses exponential backoff (1s to 60s) for automatic reconnection.

### Option 4: Browser-based setup (field deployment)

For COTS hardware deployed at venues where SSH access is impractical:

1. Follow build steps from Option 1. Start the node with a minimal or empty config.

2. **Open the setup wizard** in a browser at `http://<edge-ip>:8080/setup`.

3. **Fill in the form**: device name, API listen address/port, manager URL, registration token, and whether to accept self-signed certificates.

4. **Save** -- the configuration is written to disk.

5. **Restart the service** to apply the new settings (e.g., `systemctl restart bilbycast-edge`).

The setup wizard is enabled by default (`setup_enabled: true` in config). It auto-disables itself (writing `setup_enabled: false` to disk) as soon as the node completes its first successful registration with a manager, so `/setup` stops accepting reconfiguration after provisioning. Operators can also flip the flag manually. The wizard requires no authentication -- it is intended for initial setup of unconfigured nodes.

## CLI Options

```
bilbycast-edge [OPTIONS]

Options:
  -c, --config <PATH>         Path to configuration file [default: ./config.json]
  -p, --port <PORT>           Override API listen port
  -b, --bind <ADDR>           Override API listen address
      --monitor-port <PORT>   Override monitor dashboard port
  -l, --log-level <LEVEL>     Log level: trace, debug, info, warn, error [default: info]
  -V, --version               Print version
  -h, --help                  Print help
```

## Documentation

- [Configuration Reference](docs/CONFIGURATION.md) — full config file documentation with all fields and examples
- [Configuration Guide](docs/configuration-guide.md) — annotated examples for every input and output type
- [Supported Protocols](docs/supported-protocols.md) — protocol matrix with feature lists
- **[Audio Gateway Guide](docs/audio-gateway.md)** — bridging PCM audio between studios, transcoding, talkback, radio contribution, and SMPTE 302M LPCM-in-MPEG-TS over SRT/UDP/RTP. Read this if you're using Bilbycast for any audio that isn't strict byte-identical ST 2110-30 passthrough.
- [SMPTE ST 2110](docs/st2110.md) — ST 2110-30/-31/-40 architecture, validation rules, PTP integration
- [NMOS](docs/nmos.md) — IS-04 / IS-05 / IS-08 / BCP-004 surface, mDNS-SD registration, channel mapping
- [Events and Alarms](docs/events-and-alarms.md) — operational event reference
- [API Reference](docs/api-reference.md) — REST and WebSocket API
- [API Security](docs/api-security.md) — TLS, JWT, RBAC, ingress filters
- [Architecture](docs/architecture.md) — internal structure of the edge process

## Licensing

bilbycast-edge is **dual-licensed**:

- **AGPL-3.0-or-later** for open-source users — free for review, private use, small-scale deployment, and any use where you are comfortable releasing the source of your modifications and any network service built on top of bilbycast under AGPL terms. See [LICENSE](LICENSE).
- **Commercial licence** from Softside Tech Pty Ltd for OEMs, hardware integrators, SaaS providers, and commercial customers who need to ship bilbycast inside a closed-source product or operate it as a network service without AGPL § 13's source-release obligation. Contact **contact@bilbycast.com** for terms. See [LICENSE.commercial](LICENSE.commercial).

AGPL's network-copyleft (§ 13) means that if you modify bilbycast and run it as a service accessed by users over a network, those users must be able to obtain the complete corresponding source of your modified version. The commercial licence removes that obligation.

Contributions are accepted under the Developer Certificate of Origin — see [DCO.md](DCO.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## Choosing a release binary

Two variants are published on each tagged release, for Linux x86_64 and Linux ARM64:

| Variant          | When to use                                       | Binary licence       |
|------------------|---------------------------------------------------|----------------------|
| `*-linux`        | No H.264/H.265 transcoding needed (pass-through only) | AGPL-3.0-or-later    |
| `*-linux-full`   | You need H.264/H.265 transcoding out of the box   | AGPL-3.0-or-later combined work (bundles libx264 + libx265 under GPL-2.0-or-later; see `NOTICE.full` inside the tarball) |

The `*-linux-full` binary also includes NVENC support for NVIDIA hardware — select `codec: "h264_nvenc"` / `"hevc_nvenc"` in your flow config when running on an NVIDIA host; x264/x265 handle everything else.

See [docs/installation.md](docs/installation.md) for download and install instructions per OS.

**Commercial-licence customers**: a Softside Tech commercial licence covers the bilbycast source only — it cannot relicense libx264 / libx265, which remain GPL-2.0-or-later in the `*-linux-full` variant. Commercial deployments that need to avoid GPL copyleft should use the default (`*-linux`) variant or build with `video-encoder-nvenc` only (LGPL API layer; NVIDIA covers H.264/H.265 patents at the hardware layer). See [LICENSE.commercial](LICENSE.commercial) for the full scope statement.

**Patent notice**: H.264 / H.265 (AVC / HEVC) are patent-encumbered codecs. Patent licensing from MPEG-LA, Access Advance, Velos Media, and other rights holders is separate from software copyright and is the responsibility of the operator in commercial deployments. See the `NOTICE.full` file inside the `*-linux-full` tarball for details.
