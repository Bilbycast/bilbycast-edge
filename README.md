# bilbycast-edge

> 🌐 Learn more at **[bilbycast.com](https://bilbycast.com)** — the official website for the Bilbycast broadcast media transport suite.

Media transport edge node supporting SRT, RTP, UDP, RTMP, HLS, and WebRTC protocols. Each node runs one or more flows, where a flow consists of a single input fanning out to multiple outputs, with support for SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy.

Full MPTS (multi-program transport stream) support end-to-end on UDP/RTP/SRT/HLS with optional per-output `program_number` down-selection for extracting a single program as a rewritten SPTS. RTMP/WebRTC outputs and the thumbnail generator lock onto a chosen program deterministically.

Supports NMOS IS-04 (Discovery & Registration) and IS-05 (Connection Management) for integration with broadcast control systems. Exposes Prometheus metrics for monitoring.

## Supported Protocols

| Protocol | Input | Output | Notes                                          |
|----------|-------|--------|-------------------------------------------------|
| SRT      | Yes   | Yes    | Caller, listener, rendezvous modes; AES encryption; Stream ID access control; 2022-7 redundancy. **`transport_mode: "audio_302m"`** for SMPTE 302M LPCM-in-MPEG-TS over SRT (output side; input demux deferred) |
| RTP      | Yes   | Yes    | RTP-wrapped over UDP; unicast and multicast; SMPTE 2022-1 FEC; DSCP QoS |
| UDP      | Yes   | Yes    | Raw MPEG-TS over UDP; unicast and multicast; DSCP QoS marking. **`transport_mode: "audio_302m"`** for SMPTE 302M LPCM-in-MPEG-TS over UDP |
| RTMP     | Yes   | Yes    | H.264/AAC; accepts publish from OBS/ffmpeg; supports RTMPS (TLS); optional `audio_encode` re-encode block (AAC-LC / HE-AAC v1/v2) |
| RTSP     | Yes   | No     | Pull H.264/H.265 from IP cameras/media servers; TCP/UDP transport; auto-reconnect |
| HLS      | No    | Yes    | Segment-based ingest; supports HEVC/HDR; optional `audio_encode` per-segment re-encode (AAC family / MP2 / AC-3) |
| WebRTC   | Yes   | Yes    | WHIP/WHEP; H.264 video + Opus audio (enabled by default). Optional `audio_encode` block enables AAC → Opus on-the-fly via the ffmpeg sidecar |
| **SMPTE ST 2110-30** | Yes | Yes | RFC 3551 PCM audio (L16/L24); 2022-7 dual-network; PTP slave via external `ptp4l`. Optional per-output `transcode` block (sample-rate / bit-depth / channel routing) |
| **SMPTE ST 2110-31** | Yes | Yes | AES3 transparent audio (24-bit), preserves Dolby E and AES3 sub-frame bits |
| **SMPTE ST 2110-40** | Yes | Yes | RFC 8331 ancillary data (SCTE-104, SMPTE 12M timecode, CEA-608/708 captions) |
| **`rtp_audio`** | Yes | Yes | Generic RFC 3551 PCM-over-RTP — wire-identical to ST 2110-30 but **no PTP requirement**, sample rates 32 / 44.1 / 48 / 88.2 / 96 kHz. For radio contribution, talkback, ffmpeg / OBS interop. Same `transcode` block as ST 2110-30. Output supports `transport_mode: "audio_302m"` (RTP/MP2T encapsulation per RFC 2250) |

**Compressed-audio bridge (Phase A + Phase B):** AAC contribution audio
carried in MPEG-TS over RTMP / RTSP / SRT / UDP / RTP can be decoded
in-process via the pure-Rust `symphonia-codec-aac` (Phase A) and either
land into the PCM-only ST 2110 / `rtp_audio` / SMPTE 302M outputs, or
be re-encoded via an ffmpeg sidecar subprocess (Phase B) into AAC,
HE-AAC v1/v2, Opus, MP2, or AC-3 for the RTMP, HLS, and WebRTC outputs.
The pure-Rust binary stays unchanged — `ffmpeg` is invoked at runtime,
never linked. The marquee chain is **AAC RTMP contribution → Opus
WebRTC distribution** in a single bilbycast-edge process. See
[`docs/audio-gateway.md`](docs/audio-gateway.md).

## Quick Start

### Option 1: Standalone (no monitoring)

1. **Prerequisites**: Install the [Rust toolchain](https://rustup.rs/) (stable, edition 2024).

2. **Build**:
   ```bash
   cargo build --release
   ```

3. **Create a config file** (`config.json`) with server and flow definitions:
   ```json
   {
     "version": 1,
     "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
     "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
     "flows": [
       {
         "id": "srt-to-rtp",
         "name": "SRT Input to RTP Output",
         "enabled": true,
         "input": {
           "type": "srt",
           "mode": "listener",
           "local_addr": "0.0.0.0:9000",
           "latency_ms": 120
         },
         "outputs": [
           {
             "type": "rtp",
             "id": "out-1",
             "name": "RTP Output",
             "dest_addr": "192.168.1.100:5004"
           }
         ]
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
     "version": 1,
     "server": {
       "listen_addr": "0.0.0.0",
       "listen_port": 8080
     },
     "flows": []
   }
   ```

   **secrets.json** (set `chmod 600 secrets.json`):
   ```json
   {
     "version": 1,
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
     "version": 1,
     "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
     "manager": {
       "enabled": true,
       "url": "wss://manager-host:8443/ws/node"
     },
     "flows": []
   }
   ```

   **secrets.json** (optional — or use the setup wizard instead):
   ```json
   {
     "version": 1,
     "manager_registration_token": "<token-from-manager>"
   }
   ```

   > **Tip**: You can also place the `registration_token` inside `config.json` under `manager` — it will be automatically migrated to `secrets.json` on first startup.

4. **Start the node** -- it connects to the manager, authenticates with the registration token, and receives a permanent `node_id` and `node_secret`.

5. **The node appears** in the manager dashboard and can be configured and monitored remotely. Commands from the manager (create flow, delete flow, add/remove output) are executed automatically.

6. **Credentials are saved automatically** after registration: `node_id` goes to `config.json`, `node_secret` goes to `secrets.json` (with `0600` permissions). The `registration_token` is cleared. On subsequent starts, the node reconnects using the saved credentials. The connection uses exponential backoff (1s to 60s) for automatic reconnection.

### Option 4: Browser-based setup (field deployment)

For COTS hardware deployed at venues where SSH access is impractical:

1. Follow build steps from Option 1. Start the node with a minimal or empty config.

2. **Open the setup wizard** in a browser at `http://<edge-ip>:8080/setup`.

3. **Fill in the form**: device name, API listen address/port, manager URL, registration token, and whether to accept self-signed certificates.

4. **Save** -- the configuration is written to disk.

5. **Restart the service** to apply the new settings (e.g., `systemctl restart bilbycast-edge`).

The setup wizard is enabled by default (`setup_enabled: true` in config). Set it to `false` after provisioning to disable access. The wizard requires no authentication -- it is intended for initial setup of unconfigured nodes.

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

## License

This project is licensed under the [Elastic License 2.0](LICENSE). For use cases not covered by ELv2 (OEM, managed services, resale), a commercial license is available from Softside Tech Pty Ltd — contact admin@softsidetech.com.
