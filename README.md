# srtedge

A high-performance RTP/SMPTE 2022-2 over SRT transport bridge with SMPTE 2022-7 hitless redundancy and SMPTE 2022-1 Forward Error Correction.

## Features

- **RTP/UDP I/O** -- unicast and multicast input/output with automatic multicast join
- **SRT Transport** -- caller and listener modes with AES encryption and configurable latency (pure Rust, no C dependencies)
- **Fan-out** -- one input to multiple outputs via broadcast channels, with hot-add/remove at runtime
- **SMPTE 2022-7 Hitless Redundancy** -- dual-leg SRT with automatic de-duplication on input and packet duplication on output
- **SMPTE 2022-1 FEC** -- column and row XOR parity encoding/decoding across an L x D matrix for packet recovery
- **REST API** -- full CRUD for flows, hot-add/remove outputs, start/stop/restart, config management
- **WebSocket Stats** -- real-time JSON stats pushed to subscribers every second with computed bitrates
- **Prometheus Metrics** -- `/metrics` endpoint with per-flow input/output counters, bitrates, FEC, SRT stats
- **Web Monitor Dashboard** -- optional browser-based status page on a separate port with auto-refreshing flow stats
- **Persistent Config** -- JSON config file auto-loaded on startup, mutations persisted immediately
- **Graceful Shutdown** -- CTRL+C triggers coordinated shutdown of all flows and SRT connections

## Architecture

```
                        +-------------------------------------------------+
                        |                   srtedge                        |
                        |                                                  |
  RTP/UDP ------+       |   +-----------+    broadcast     +-----------+   |     +-----> RTP/UDP
  (unicast/     |       |   |   Input   |    channel       |  Output   |   |     |      (unicast/
   multicast)   +------>|   |   Task    |---[fan-out]----->|  Task 1   |---+--->-+       multicast)
                |       |   |           |       |          +-----------+   |     |
  SRT ----------+       |   | (RTP or   |       |          |  Output   |   |     +-----> SRT
  (caller/      |       |   |  SRT)     |       +--------->|  Task 2   |---+--->-+      (caller/
   listener)    +------>|   +-----------+       |          +-----------+   |     |       listener)
                |       |   [optional]          |                         |     |
  SRT Leg 2 ----+       |   FEC decode      [optional]                   |     +-----> SRT Leg 2
  (2022-7)              |   2022-7 merge    FEC encode                   |            (2022-7)
                        |                   2022-7 dup                   |
                        +-------------------------------------------------+
                        |   REST API   |   WebSocket   |   Prometheus     |
                        |   :8080      |   /ws/stats   |   /metrics       |
                        +-------------------------------------------------+
                        |   Web Monitor Dashboard (optional, :9090)       |
                        +-------------------------------------------------+
```

**Data flow:** Input task receives packets and publishes `RtpPacket` structs to a tokio broadcast channel (capacity 2048). Each output task subscribes independently and forwards packets to its destination. Optional FEC and redundancy processing are inserted inline at the input/output boundaries.

## Prerequisites

| Dependency | Purpose |
|-----------|---------|
| **Rust 1.85+** | Edition 2024 support |

SRT transport is implemented in pure Rust via the `srt-native` crate -- no C compiler, CMake, or system libraries are required.

## Build and Run

```bash
# Clone and build
cd srtedge
cargo build --release

# Run tests
cargo test

# Generate Rust documentation
cargo doc --no-deps --open

# Copy the example config and run
cp srt-to-rtp.json config.json
./target/release/srtedge -c config.json
```

## CLI Usage

```
srtedge [OPTIONS]

Options:
  -c, --config <PATH>       Path to configuration file [default: ./config.json]
  -p, --port <PORT>         Override API listen port (overrides config file)
  -b, --bind <ADDR>         Override API listen address (overrides config file)
      --monitor-port <PORT>  Override monitor dashboard port (overrides config file)
  -l, --log-level <LEVEL>   Log level: trace, debug, info, warn, error [default: info]
  -h, --help                Print help
  -V, --version             Print version

Environment:
  RUST_LOG                  Override log level via tracing env filter
```

## Configuration Reference

The configuration is stored as JSON in the file specified by `--config` (default: `./config.json`). Changes made via the REST API are persisted to this file immediately.

### Root Config (`AppConfig`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `version` | `u32` | `1` | Schema version for forward compatibility |
| `server` | `ServerConfig` | see below | API server settings |
| `monitor` | `MonitorConfig?` | `null` | Optional web monitoring dashboard (omit to disable) |
| `flows` | `FlowConfig[]` | `[]` | List of flow definitions |

### Server Config (`ServerConfig`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `listen_addr` | `string` | `"0.0.0.0"` | API listen address |
| `listen_port` | `u16` | `8080` | API listen port |

### Monitor Config (`MonitorConfig`) -- optional

When present, srtedge starts a second HTTP server serving a self-contained HTML dashboard for browser-based status monitoring. Omit this section entirely to disable the dashboard.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `listen_addr` | `string` | required | Dashboard listen address, e.g. `"0.0.0.0"` |
| `listen_port` | `u16` | required | Dashboard listen port, e.g. `9090` |

### Flow Config (`FlowConfig`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | `string` | required | Unique flow identifier |
| `name` | `string` | required | Human-readable name |
| `enabled` | `bool` | `true` | Auto-start on application startup |
| `input` | `InputConfig` | required | Single input source |
| `outputs` | `OutputConfig[]` | required | One or more output destinations |

### Input Config (`InputConfig`)

Discriminated by `"type"`: `"rtp"` or `"srt"`.

#### RTP Input

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `"rtp"` | required | Input type discriminator |
| `bind_addr` | `string` | required | Local address to bind, e.g. `"0.0.0.0:5000"` or `"239.1.1.1:5000"` for multicast |
| `interface_addr` | `string?` | `null` | Network interface IP for multicast join |
| `fec_decode` | `FecConfig?` | `null` | SMPTE 2022-1 FEC decode parameters |

#### SRT Input

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `"srt"` | required | Input type discriminator |
| `mode` | `SrtMode` | required | `"caller"` or `"listener"` |
| `local_addr` | `string` | required | Local bind address, e.g. `"0.0.0.0:9000"` |
| `remote_addr` | `string?` | `null` | Remote address (required for caller mode) |
| `latency_ms` | `u64` | `120` | SRT latency in milliseconds |
| `passphrase` | `string?` | `null` | AES encryption passphrase (10-79 characters) |
| `aes_key_len` | `usize?` | `null` | AES key length: 16, 24, or 32 |
| `redundancy` | `SrtRedundancyConfig?` | `null` | SMPTE 2022-7 second leg config |

### Output Config (`OutputConfig`)

Discriminated by `"type"`: `"rtp"` or `"srt"`.

#### RTP Output

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `"rtp"` | required | Output type discriminator |
| `id` | `string` | required | Unique output ID within the flow |
| `name` | `string` | required | Human-readable name |
| `dest_addr` | `string` | required | Destination address, e.g. `"192.168.1.100:5004"` |
| `bind_addr` | `string?` | `null` | Source bind address (defaults to `"0.0.0.0:0"`) |
| `interface_addr` | `string?` | `null` | Network interface for multicast send |
| `fec_encode` | `FecConfig?` | `null` | SMPTE 2022-1 FEC encode parameters |

#### SRT Output

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `"srt"` | required | Output type discriminator |
| `id` | `string` | required | Unique output ID within the flow |
| `name` | `string` | required | Human-readable name |
| `mode` | `SrtMode` | required | `"caller"` or `"listener"` |
| `local_addr` | `string` | required | Local bind address |
| `remote_addr` | `string?` | `null` | Remote address (required for caller mode) |
| `latency_ms` | `u64` | `120` | SRT latency in milliseconds |
| `passphrase` | `string?` | `null` | AES encryption passphrase (10-79 characters) |
| `aes_key_len` | `usize?` | `null` | AES key length: 16, 24, or 32 |
| `redundancy` | `SrtRedundancyConfig?` | `null` | SMPTE 2022-7 second leg config |

### SRT Redundancy Config (`SrtRedundancyConfig`)

Defines the second SRT leg for SMPTE 2022-7 hitless redundancy. The primary SRT settings in the parent config define leg 1; this struct defines leg 2.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | `SrtMode` | required | SRT mode for leg 2 |
| `local_addr` | `string` | required | Bind address for leg 2 |
| `remote_addr` | `string?` | `null` | Remote address for leg 2 (for caller mode) |
| `latency_ms` | `u64` | `120` | SRT latency for leg 2 |
| `passphrase` | `string?` | `null` | AES passphrase for leg 2 |
| `aes_key_len` | `usize?` | `null` | AES key length for leg 2 |

### FEC Config (`FecConfig`)

SMPTE 2022-1 Forward Error Correction parameters.

| Field | Type | Range | Description |
|-------|------|-------|-------------|
| `columns` | `u8` | 1-20 | L parameter (number of columns in FEC matrix) |
| `rows` | `u8` | 4-20 | D parameter (number of rows in FEC matrix) |

### SRT Mode (`SrtMode`)

| Value | Description |
|-------|-------------|
| `"caller"` | Initiates connection to a remote listener |
| `"listener"` | Waits for an incoming caller connection |

## Example Configurations

### 1. Simple RTP to SRT Bridge

Receives an RTP multicast stream and bridges it over SRT (with optional web monitor on port 9090):

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "flows": [
    {
      "id": "rtp-to-srt",
      "name": "RTP to SRT Bridge",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "0.0.0.0:5000"
      },
      "outputs": [
        {
          "type": "srt",
          "id": "srt-out",
          "name": "SRT Output",
          "mode": "listener",
          "local_addr": "0.0.0.0:9000",
          "latency_ms": 200
        }
      ]
    }
  ]
}
```

### 2. SRT to RTP with FEC Encode

Receives SRT input and outputs RTP with SMPTE 2022-1 FEC protection:

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "srt-to-rtp-fec",
      "name": "SRT to RTP with FEC",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-fec-out",
          "name": "RTP with FEC",
          "dest_addr": "192.168.1.50:5004",
          "fec_encode": { "columns": 10, "rows": 10 }
        }
      ]
    }
  ]
}
```

### 3. SMPTE 2022-7 Redundant SRT Input

Receives from two independent SRT paths with hitless merge:

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "redundant-input",
      "name": "2022-7 Redundant SRT Input",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 500,
        "redundancy": {
          "mode": "listener",
          "local_addr": "0.0.0.0:9001",
          "latency_ms": 500
        }
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-out",
          "name": "RTP Output",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### 4. Full Matrix: Redundant SRT + FEC + Fan-out

Redundant SRT input with FEC-protected RTP output and a redundant SRT output:

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "full-matrix",
      "name": "Full Matrix Flow",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 500,
        "redundancy": {
          "mode": "listener",
          "local_addr": "0.0.0.0:9001",
          "latency_ms": 500
        }
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-fec-out",
          "name": "RTP with FEC",
          "dest_addr": "192.168.1.50:5004",
          "fec_encode": { "columns": 10, "rows": 10 }
        },
        {
          "type": "srt",
          "id": "srt-redundant-out",
          "name": "Redundant SRT Output",
          "mode": "caller",
          "local_addr": "0.0.0.0:0",
          "remote_addr": "10.0.1.100:9000",
          "latency_ms": 200,
          "redundancy": {
            "mode": "caller",
            "local_addr": "0.0.0.0:0",
            "remote_addr": "10.0.2.100:9000",
            "latency_ms": 200
          }
        }
      ]
    }
  ]
}
```

## API Reference

Base URL: `http://localhost:8080`

All REST responses use a standard wrapper:

```
{ "success": true, "data": <T> }
```

Error responses:

```
{ "success": false, "error": "Error description" }
```

| Status | Meaning |
|--------|---------|
| 400 | Bad Request -- validation failed or malformed input |
| 404 | Not Found -- flow or output does not exist |
| 409 | Conflict -- duplicate ID or flow already in requested state |
| 500 | Internal Server Error -- unexpected failure |

### Flow Management

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/flows` | List all flows (summary: id, name, enabled, input_type, output_count) |
| `POST` | `/api/v1/flows` | Create a flow (body: `FlowConfig` JSON). Auto-starts if `enabled: true` |
| `GET` | `/api/v1/flows/{flow_id}` | Get full flow configuration |
| `PUT` | `/api/v1/flows/{flow_id}` | Replace flow config (stops, updates, restarts if enabled) |
| `DELETE` | `/api/v1/flows/{flow_id}` | Delete a flow (stops if running) |
| `POST` | `/api/v1/flows/{flow_id}/start` | Start a stopped flow |
| `POST` | `/api/v1/flows/{flow_id}/stop` | Stop a running flow |
| `POST` | `/api/v1/flows/{flow_id}/restart` | Stop and restart a flow |

### Output Management (Hot-add/remove)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/v1/flows/{flow_id}/outputs` | Add output to a flow (body: `OutputConfig` JSON) |
| `DELETE` | `/api/v1/flows/{flow_id}/outputs/{output_id}` | Remove output from a flow |

### Statistics

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/stats` | All flow stats + system info (uptime, version, flow counts) |
| `GET` | `/api/v1/stats/{flow_id}` | Stats for a single flow |

### Configuration

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/config` | Get the full application config |
| `PUT` | `/api/v1/config` | Replace entire config (stops all flows, applies new config, restarts) |
| `POST` | `/api/v1/config/reload` | Reload config from disk |

### Health and Metrics

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check (returns status, version, uptime, flow counts) |
| `GET` | `/metrics` | Prometheus metrics in text exposition format |

### WebSocket

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/ws/stats` | WebSocket upgrade for real-time stats (JSON array of `FlowStats` every 1 second) |

### Web Monitor Dashboard (optional)

When `monitor` is configured, a self-contained HTML dashboard is served on the specified port:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | HTML dashboard with auto-refreshing flow stats (1.5s interval) |
| `GET` | `/api/stats` | JSON stats endpoint used by the dashboard |

Open `http://<monitor_addr>:<monitor_port>` in any browser to view system and per-flow status.

### Stats JSON Structure

The `FlowStats` object returned by stats endpoints and WebSocket:

```json
{
  "flow_id": "flow-1",
  "flow_name": "My Flow",
  "state": "Running",
  "input": {
    "input_type": "srt",
    "state": "connected",
    "packets_received": 50000,
    "bytes_received": 65000000,
    "bitrate_bps": 20000000,
    "packets_lost": 0,
    "packets_recovered_fec": 0,
    "srt_stats": {
      "state": "connected",
      "rtt_ms": 25.3,
      "send_rate_mbps": 0.0,
      "recv_rate_mbps": 20.0,
      "pkt_loss_total": 0,
      "pkt_retransmit_total": 0,
      "uptime_ms": 60000
    },
    "srt_leg2_stats": null,
    "redundancy_switches": 0
  },
  "outputs": [
    {
      "output_id": "out-1",
      "output_name": "RTP Output",
      "output_type": "rtp",
      "state": "sending",
      "packets_sent": 50000,
      "bytes_sent": 65000000,
      "bitrate_bps": 20000000,
      "packets_dropped": 0,
      "fec_packets_sent": 0,
      "srt_stats": null,
      "srt_leg2_stats": null
    }
  ],
  "uptime_secs": 60
}
```

## Prometheus Metrics

All metrics are exported at `GET /metrics` in Prometheus text exposition format (version 0.0.4).

### Application Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `srtedge_info{version="..."}` | gauge | Application version info |
| `srtedge_uptime_seconds` | gauge | Application uptime in seconds |
| `srtedge_flows_total` | gauge | Total configured flows |
| `srtedge_flows_active` | gauge | Currently running flows |

### Per-Flow Input Metrics

Labels: `flow_id`

| Metric | Type | Description |
|--------|------|-------------|
| `srtedge_flow_input_packets_total` | counter | Total RTP packets received |
| `srtedge_flow_input_bytes_total` | counter | Total bytes received |
| `srtedge_flow_input_bitrate_bps` | gauge | Current input bitrate (bits/sec) |
| `srtedge_flow_input_packets_lost` | counter | Packets lost (sequence gaps) |
| `srtedge_flow_input_fec_recovered_total` | counter | Packets recovered via FEC |
| `srtedge_flow_input_redundancy_switches_total` | counter | SMPTE 2022-7 leg switches |

### Per-Output Metrics

Labels: `flow_id`, `output_id`

| Metric | Type | Description |
|--------|------|-------------|
| `srtedge_flow_output_packets_total` | counter | Total packets sent |
| `srtedge_flow_output_bytes_total` | counter | Total bytes sent |
| `srtedge_flow_output_bitrate_bps` | gauge | Current output bitrate (bits/sec) |
| `srtedge_flow_output_packets_dropped` | counter | Packets dropped (broadcast lag) |
| `srtedge_flow_output_fec_sent_total` | counter | FEC packets generated and sent |

### SRT Connection Metrics

Labels: `flow_id`, `leg` (and optionally `output_id`)

| Metric | Type | Description |
|--------|------|-------------|
| `srtedge_srt_rtt_ms` | gauge | SRT round-trip time in milliseconds |
| `srtedge_srt_loss_total` | counter | SRT total packet loss |

Leg values: `input`, `input_leg2`, `leg1`, `leg2`

## Manual Testing Guide

### Tools Required

| Tool | Purpose | Install |
|------|---------|---------|
| `ffmpeg` / `ffplay` | Generate and play test streams | `brew install ffmpeg` |
| `srt-live-transmit` | SRT send/receive testing | `brew install srt` or build from [github.com/Haivision/srt](https://github.com/Haivision/srt) |
| `curl` | REST API testing | Pre-installed on most systems |
| `wscat` | WebSocket testing | `npm install -g wscat` |

### Test 1: RTP to RTP Passthrough

Verifies basic RTP input and output without SRT.

**Config** (`config.json`):
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [{
    "id": "rtp-passthrough",
    "name": "RTP Passthrough",
    "enabled": true,
    "input": { "type": "rtp", "bind_addr": "0.0.0.0:5000" },
    "outputs": [{
      "type": "rtp", "id": "out1", "name": "RTP Out",
      "dest_addr": "127.0.0.1:5004"
    }]
  }]
}
```

**Steps:**

```bash
# Terminal 1: Start srtedge
cargo run -- -c config.json

# Terminal 2: Send test RTP stream
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -c:v libx264 -f rtp rtp://127.0.0.1:5000

# Terminal 3: Verify output (check stats)
curl -s http://localhost:8080/api/v1/stats | python3 -m json.tool
```

**Expected:** `packets_received` and `packets_sent` should increase. `packets_lost` and `packets_dropped` should be 0.

### Test 2: RTP to SRT Bridge

**Config:** Use Example 1 from above (RTP input on :5000, SRT listener output on :9000).

**Steps:**

```bash
# Terminal 1: Start srtedge
cargo run -- -c config.json

# Terminal 2: Send test RTP stream
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -c:v libx264 -f rtp rtp://127.0.0.1:5000

# Terminal 3: Receive SRT stream
srt-live-transmit "srt://127.0.0.1:9000?mode=caller" \
  "udp://127.0.0.1:6000"

# Terminal 4: Play received stream
ffplay udp://127.0.0.1:6000
```

**Expected:** Video plays in ffplay window. SRT stats show connected state with RTT values.

### Test 3: SRT to RTP Bridge

**Config:**
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [{
    "id": "srt-to-rtp",
    "name": "SRT to RTP",
    "enabled": true,
    "input": {
      "type": "srt",
      "mode": "listener",
      "local_addr": "0.0.0.0:9000",
      "latency_ms": 200
    },
    "outputs": [{
      "type": "rtp", "id": "out1", "name": "RTP Out",
      "dest_addr": "127.0.0.1:5004"
    }]
  }]
}
```

**Steps:**

```bash
# Terminal 1: Start srtedge
cargo run -- -c config.json

# Terminal 2: Send via SRT (generate MPEG-TS, send over SRT)
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -c:v libx264 -f mpegts "srt://127.0.0.1:9000?mode=caller"

# Terminal 3: Verify stats
curl -s http://localhost:8080/api/v1/stats/srt-to-rtp | python3 -m json.tool
```

### Test 4: Fan-out (Multiple Outputs)

**Config:**
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [{
    "id": "fanout",
    "name": "Fan-out Test",
    "enabled": true,
    "input": { "type": "rtp", "bind_addr": "0.0.0.0:5000" },
    "outputs": [
      { "type": "rtp", "id": "out1", "name": "Output 1", "dest_addr": "127.0.0.1:5004" },
      { "type": "rtp", "id": "out2", "name": "Output 2", "dest_addr": "127.0.0.1:5008" },
      { "type": "rtp", "id": "out3", "name": "Output 3", "dest_addr": "127.0.0.1:5012" }
    ]
  }]
}
```

**Expected:** All three outputs show matching `packets_sent` counts in stats.

### Test 5: REST API CRUD Operations

```bash
# List all flows
curl -s http://localhost:8080/api/v1/flows | python3 -m json.tool

# Create a new flow
curl -s -X POST http://localhost:8080/api/v1/flows \
  -H "Content-Type: application/json" \
  -d '{
    "id": "test-flow",
    "name": "Test Flow",
    "enabled": true,
    "input": { "type": "rtp", "bind_addr": "0.0.0.0:6000" },
    "outputs": [{
      "type": "rtp", "id": "out1", "name": "Out 1",
      "dest_addr": "127.0.0.1:6004"
    }]
  }' | python3 -m json.tool

# Get flow details
curl -s http://localhost:8080/api/v1/flows/test-flow | python3 -m json.tool

# Stop the flow
curl -s -X POST http://localhost:8080/api/v1/flows/test-flow/stop | python3 -m json.tool

# Start the flow
curl -s -X POST http://localhost:8080/api/v1/flows/test-flow/start | python3 -m json.tool

# Restart the flow
curl -s -X POST http://localhost:8080/api/v1/flows/test-flow/restart | python3 -m json.tool

# Delete the flow
curl -s -X DELETE http://localhost:8080/api/v1/flows/test-flow | python3 -m json.tool
```

### Test 6: Hot-Add/Remove Outputs

```bash
# Hot-add a new output to a running flow
curl -s -X POST http://localhost:8080/api/v1/flows/test-flow/outputs \
  -H "Content-Type: application/json" \
  -d '{
    "type": "rtp", "id": "out2", "name": "Hot Output",
    "dest_addr": "127.0.0.1:6008"
  }' | python3 -m json.tool

# Verify it appears in stats
curl -s http://localhost:8080/api/v1/stats/test-flow | python3 -m json.tool

# Remove the hot-added output
curl -s -X DELETE http://localhost:8080/api/v1/flows/test-flow/outputs/out2 \
  | python3 -m json.tool
```

### Test 7: WebSocket Real-Time Stats

```bash
# Connect to WebSocket (receives JSON every 1 second)
wscat -c ws://localhost:8080/api/v1/ws/stats
```

**Expected:** JSON array of `FlowStats` objects printed every second with `bitrate_bps` values computed from throughput estimation.

### Test 8: Prometheus Metrics

```bash
# Fetch Prometheus metrics
curl -s http://localhost:8080/metrics

# Or open in browser
open http://localhost:8080/metrics
```

**Expected:** Text output with `# HELP`, `# TYPE` annotations and metric values. Per-flow and per-output metrics appear when flows are running.

### Test 9: Configuration Management

```bash
# Get current config
curl -s http://localhost:8080/api/v1/config | python3 -m json.tool

# Replace entire config (stops all flows, applies new config, restarts)
curl -s -X PUT http://localhost:8080/api/v1/config \
  -H "Content-Type: application/json" \
  -d @new_config.json | python3 -m json.tool

# Reload config from disk (useful after manual file edits)
curl -s -X POST http://localhost:8080/api/v1/config/reload | python3 -m json.tool
```

### Test 10: SMPTE 2022-7 Redundancy

**Config:** Use Example 3 (redundant SRT input on :9000 and :9001).

**Steps:**

```bash
# Terminal 1: Start srtedge
cargo run -- -c config.json

# Terminal 2: Send to leg 1
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -c:v libx264 -f mpegts "srt://127.0.0.1:9000?mode=caller"

# Terminal 3: Send same stream to leg 2
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -c:v libx264 -f mpegts "srt://127.0.0.1:9001?mode=caller"

# Terminal 4: Monitor redundancy stats
watch -n1 'curl -s http://localhost:8080/api/v1/stats/redundant-input \
  | python3 -c "import sys,json; s=json.load(sys.stdin)[\"data\"]; \
    print(f\"switches: {s[\"input\"][\"redundancy_switches\"]}, \
    lost: {s[\"input\"][\"packets_lost\"]}\")"'
```

**Test failover:** Kill one of the ffmpeg senders. The `redundancy_switches` counter should increment but `packets_lost` should remain 0 (hitless switch).

### Test 11: SMPTE 2022-1 FEC

**Config:** Use Example 2 (SRT input, RTP output with FEC encode).

**Steps:**

```bash
# Start srtedge with FEC-enabled config
cargo run -- -c config.json

# Send stream and check fec_packets_sent in output stats
curl -s http://localhost:8080/api/v1/stats | python3 -m json.tool
```

**Expected:** `fec_packets_sent` increments as FEC packets are generated. For a 10x10 matrix, expect 10 column FEC + 10 row FEC = 20 FEC packets per 100 media packets.

### Test 12: Web Monitor Dashboard

Add a `"monitor"` section to your config:

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "flows": [...]
}
```

```bash
# Start srtedge with monitor enabled
cargo run -- -c config.json

# Open the dashboard in your browser
open http://localhost:9090
```

**Expected:** A dark-themed dashboard showing system uptime, active flows, and per-flow cards with input/output stats that auto-refresh every 1.5 seconds.

You can also override the monitor port via CLI: `--monitor-port 9090`

### Test 13: Health Check

```bash
curl -s http://localhost:8080/health | python3 -m json.tool
```

**Expected:**
```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_secs": 120,
  "active_flows": 1,
  "total_flows": 1
}
```

## Project Structure

```
srtedge/
├── Cargo.toml                     # Workspace root
├── README.md                      # This file
├── srt-native/                    # Pure Rust SRT protocol implementation
│   ├── srt-protocol/              # Protocol state machines, buffers, crypto
│   └── srt-transport/             # Async I/O layer (tokio): SrtSocket, SrtListener
└── srtedge/                       # Main application crate
    ├── Cargo.toml
    └── src/
        ├── main.rs                # CLI, server startup, stats publisher
        ├── lib.rs                 # Module re-exports
        ├── api/                   # REST API, WebSocket, Prometheus (axum)
        │   ├── server.rs          # Router + AppState
        │   ├── flows.rs           # Flow CRUD handlers
        │   ├── stats.rs           # Stats + health + metrics handlers
        │   ├── ws.rs              # WebSocket handler
        │   ├── models.rs          # Response DTOs
        │   └── errors.rs          # ApiError types
        ├── config/                # Configuration
        │   ├── models.rs          # Config structs (AppConfig, FlowConfig, etc.)
        │   ├── persistence.rs     # JSON load/save with atomic writes
        │   └── validation.rs      # Config validation rules
        ├── monitor/               # Web monitoring dashboard
        │   ├── server.rs          # Monitor HTTP server (separate port)
        │   └── dashboard.rs       # Embedded HTML/CSS/JS dashboard
        ├── engine/                # Flow engine
        │   ├── manager.rs         # FlowManager: lifecycle orchestration
        │   ├── flow.rs            # FlowRuntime: input + outputs + broadcast
        │   ├── input_rtp.rs       # RTP/UDP input with optional FEC decode
        │   ├── input_srt.rs       # SRT input with optional 2022-7 merge
        │   ├── output_rtp.rs      # RTP/UDP output with optional FEC encode
        │   ├── output_srt.rs      # SRT output with optional 2022-7 duplicate
        │   └── packet.rs          # RtpPacket struct
        ├── fec/                   # SMPTE 2022-1 FEC
        │   ├── matrix.rs          # L x D XOR parity matrix
        │   ├── encoder.rs         # FEC packet generation
        │   └── decoder.rs         # FEC packet recovery
        ├── redundancy/            # SMPTE 2022-7
        │   ├── merger.rs          # Hitless merge (RX de-duplication)
        │   └── duplicator.rs      # Packet duplication (TX)
        ├── stats/                 # Statistics
        │   ├── collector.rs       # Atomic counters per flow/output
        │   ├── models.rs          # FlowStats, InputStats, OutputStats
        │   └── throughput.rs      # Bitrate estimation
        ├── srt/                   # SRT connection management
        │   ├── connection.rs      # Connect helpers with retry/backoff
        │   └── stats.rs           # SRT statistics polling
        └── util/                  # Utilities
            ├── rtp_parse.rs       # RTP header field extraction
            ├── socket.rs          # UDP socket + multicast helpers
            └── time.rs            # Monotonic microsecond clock
```

## License

See LICENSE file for details.
