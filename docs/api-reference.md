# BilbyCast Edge API Reference

Complete REST API reference for bilbycast-edge. The API server listens on the address and port configured in `server.listen_addr` and `server.listen_port` (default `0.0.0.0:8080`).

All successful responses use a standard envelope:

```json
{
  "success": true,
  "data": { ... }
}
```

All error responses use the same envelope without `data`:

```json
{
  "success": false,
  "error": "Human-readable error message"
}
```

---

## Table of Contents

- [Health](#health)
- [Setup Wizard](#setup-wizard)
- [Authentication](#authentication)
- [Flows](#flows)
- [Flow Actions](#flow-actions)
- [Outputs](#outputs)
- [Statistics](#statistics)
- [Configuration](#configuration)
- [Tunnels](#tunnels)
- [Prometheus Metrics](#prometheus-metrics)
- [WebSocket](#websocket)
- [Error Codes](#error-codes)

---

## Health

### GET /health

Lightweight health check suitable for load balancers, orchestrators, and monitoring probes. This endpoint is always public (no authentication required).

**Response:**

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_secs": 3661,
  "active_flows": 2,
  "total_flows": 3
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | Always `"ok"` when the server is responsive |
| `version` | string | Application version from Cargo.toml |
| `uptime_secs` | integer | Seconds since the application started |
| `active_flows` | integer | Number of flows currently running |
| `total_flows` | integer | Total flows defined in configuration |

**curl example:**

```bash
curl http://localhost:8080/health
```

---

## Setup Wizard

Browser-based initial provisioning for edge nodes deployed on COTS hardware. All setup endpoints are public (no authentication required). Access is controlled by the `setup_enabled` config flag (default: `true`). Once the node completes its first successful registration with a manager, the flag is automatically flipped to `false` and persisted to disk — subsequent requests to `/setup` return the "Setup Disabled" page.

### GET /setup

Serves the setup wizard HTML page. Returns the wizard form when `setup_enabled` is true, or a "Setup Disabled" page when false.

### GET /setup/status

Returns the current setup-relevant configuration as JSON for pre-filling the form. The `registration_token` is always `null` in responses — secrets are never exposed via this endpoint.

**Response:**

```json
{
  "listen_addr": "0.0.0.0",
  "listen_port": 8080,
  "manager_url": null,
  "accept_self_signed_cert": false,
  "registration_token": null,
  "device_name": null,
  "setup_enabled": true
}
```

### POST /setup

Validates and saves setup configuration. Returns 403 if `setup_enabled` is false.

**Request body:**

```json
{
  "listen_addr": "0.0.0.0",
  "listen_port": 8080,
  "manager_url": "wss://manager.example.com:8443/ws/node",
  "accept_self_signed_cert": false,
  "registration_token": "token-from-manager",
  "device_name": "Studio-A Encoder"
}
```

| Field | Type | Required | Constraints |
|-------|------|----------|-------------|
| `listen_addr` | `string` | No | API server bind address |
| `listen_port` | `u16` | No | 1-65535 |
| `manager_url` | `string` | Yes | Must start with `wss://`, max 2048 chars |
| `accept_self_signed_cert` | `bool` | No | Default: false |
| `registration_token` | `string` | No | Max 4096 chars |
| `device_name` | `string` | No | Max 256 chars |

**Success response (200):**

```json
{
  "success": true,
  "message": "Configuration saved. Restart the bilbycast-edge service to apply the new settings."
}
```

**Error response (400/403):**

```json
{
  "success": false,
  "error": "Manager URL must start with wss:// (TLS required)"
}
```

---

## Authentication

### POST /oauth/token

OAuth 2.0 Client Credentials token endpoint. Always public (no Bearer token required). Returns a signed JWT that must be included as a Bearer token in subsequent requests to protected endpoints.

Accepts both `application/x-www-form-urlencoded` and `application/json` request bodies.

**Request body (JSON):**

```json
{
  "grant_type": "client_credentials",
  "client_id": "admin-client",
  "client_secret": "super-secret-admin-password-here"
}
```

**Request body (form-urlencoded):**

```
grant_type=client_credentials&client_id=admin-client&client_secret=super-secret-admin-password-here
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `grant_type` | string | Yes | Must be `"client_credentials"` |
| `client_id` | string | Yes | Registered client identifier |
| `client_secret` | string | Yes | Client secret |

**Success response (200):**

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJhZG1pbi1jbGllbnQiLCJyb2xlIjoiYWRtaW4iLCJpYXQiOjE3MDk4MjAwMDAsImV4cCI6MTcwOTgyMzYwMCwiaXNzIjoiYmlsYnljYXN0LWVkZ2UifQ.signature",
  "token_type": "bearer",
  "expires_in": 3600,
  "role": "admin"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `access_token` | string | Signed HS256 JWT token |
| `token_type` | string | Always `"bearer"` |
| `expires_in` | integer | Token lifetime in seconds |
| `role` | string | Role granted: `"admin"` or `"monitor"` |

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Auth not enabled, unsupported grant_type, invalid credentials, unparseable body |

**curl examples:**

```bash
# JSON body
curl -X POST http://localhost:8080/oauth/token \
  -H "Content-Type: application/json" \
  -d '{"grant_type":"client_credentials","client_id":"admin-client","client_secret":"super-secret-admin-password-here"}'

# Form-urlencoded body
curl -X POST http://localhost:8080/oauth/token \
  -d "grant_type=client_credentials&client_id=admin-client&client_secret=super-secret-admin-password-here"
```

**JWT Claims structure:**

The returned JWT contains these claims:

| Claim | Type | Description |
|-------|------|-------------|
| `sub` | string | The `client_id` |
| `role` | string | `"admin"` or `"monitor"` |
| `iat` | integer | Issued-at timestamp (Unix epoch seconds) |
| `exp` | integer | Expiration timestamp (Unix epoch seconds) |
| `iss` | string | Always `"bilbycast-edge"` |

---

## Flows

### GET /api/v1/flows

List all configured flows. Returns a summary for each flow without full input/output details.

**Auth:** Requires valid JWT (any role: admin or monitor).

**Response (200):**

```json
{
  "success": true,
  "data": {
    "flows": [
      {
        "id": "main-feed",
        "name": "Main Program Feed",
        "enabled": true,
        "input_type": "rtp",
        "output_count": 3
      },
      {
        "id": "backup-srt",
        "name": "SRT Backup Path",
        "enabled": true,
        "input_type": "srt",
        "output_count": 1
      }
    ]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `flows[].id` | string | Unique flow identifier |
| `flows[].name` | string | Human-readable display name |
| `flows[].enabled` | boolean | Whether the flow is enabled in config |
| `flows[].input_type` | string | `"rtp"` or `"srt"` |
| `flows[].output_count` | integer | Number of configured outputs |

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/flows
```

---

### GET /api/v1/flows/{flow_id}

Retrieve the full configuration of a single flow, including all input and output details.

**Auth:** Requires valid JWT (any role).

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow |

**Response (200):**

```json
{
  "success": true,
  "data": {
    "id": "main-feed",
    "name": "Main Program Feed",
    "enabled": true,
    "input": {
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "192.168.1.100",
      "fec_decode": {
        "columns": 10,
        "rows": 10
      }
    },
    "outputs": [
      {
        "type": "rtp",
        "id": "rtp-out-1",
        "name": "Local Playout",
        "dest_addr": "192.168.1.50:5004",
        "dscp": 46
      },
      {
        "type": "srt",
        "id": "srt-out-1",
        "name": "Remote Site",
        "mode": "caller",
        "local_addr": "0.0.0.0:0",
        "remote_addr": "203.0.113.10:9000",
        "latency_ms": 500
      }
    ]
  }
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found |

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/flows/main-feed
```

---

### POST /api/v1/flows

Create a new flow. The flow is validated, persisted to the config file, and (if `enabled: true`) started immediately.

**Auth:** Requires `admin` role.

**Request body:**

```json
{
  "id": "new-flow",
  "name": "New Feed",
  "enabled": true,
  "input": {
    "type": "rtp",
    "bind_addr": "0.0.0.0:5000"
  },
  "outputs": [
    {
      "type": "rtp",
      "id": "out-1",
      "name": "Output 1",
      "dest_addr": "192.168.1.50:5004"
    }
  ]
}
```

> **MPTS inputs:** every output type accepts an optional `program_number` field to down-select an MPTS input to a single program. `null` (default) passes the full MPTS through on TS-native outputs (UDP/RTP/SRT/HLS) or locks onto the lowest `program_number` in the PAT on re-muxing outputs (RTMP/WebRTC). See [configuration-guide.md — MPTS → SPTS filtering](configuration-guide.md#mpts--spts-filtering) for the full behaviour matrix. `FlowConfig` also accepts an optional `thumbnail_program_number` field that filters the buffered TS before piping it to ffmpeg for the manager UI preview.

**Response (200):**

Returns the created flow configuration in the standard envelope.

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Validation failure (invalid addresses, empty ID/name, bad FEC params) |
| 409 | A flow with the same ID already exists |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "new-flow",
    "name": "New Feed",
    "enabled": true,
    "input": {
      "type": "rtp",
      "bind_addr": "0.0.0.0:5000"
    },
    "outputs": [{
      "type": "rtp",
      "id": "out-1",
      "name": "Output 1",
      "dest_addr": "192.168.1.50:5004"
    }]
  }'
```

---

### PUT /api/v1/flows/{flow_id}

Replace an existing flow's configuration. The flow ID in the path takes precedence over the `id` field in the body. If the flow is running, it is stopped, updated, and restarted (if enabled).

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow to update |

**Request body:**

Full `FlowConfig` JSON (same structure as POST /api/v1/flows).

**Response (200):**

Returns the updated flow configuration.

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Validation failure |
| 404 | Flow not found |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X PUT http://localhost:8080/api/v1/flows/main-feed \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "main-feed",
    "name": "Main Feed (Updated)",
    "enabled": true,
    "input": {
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "192.168.1.100"
    },
    "outputs": [{
      "type": "rtp",
      "id": "out-1",
      "name": "Output 1",
      "dest_addr": "192.168.1.50:5004"
    }]
  }'
```

---

### DELETE /api/v1/flows/{flow_id}

Delete a flow. Stops the flow if running, removes it from configuration, and persists the change.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow to delete |

**Response (200):**

```json
{
  "success": true,
  "data": null
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X DELETE http://localhost:8080/api/v1/flows/old-flow \
  -H "Authorization: Bearer $TOKEN"
```

---

## Flow Actions

### POST /api/v1/flows/{flow_id}/start

Start a stopped flow. Reads the flow configuration and starts it in the engine.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow to start |

**Response (200):**

```json
{
  "success": true,
  "data": null
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found in configuration |
| 409 | Flow is already running |
| 500 | Engine failed to start the flow |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/start \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /api/v1/flows/{flow_id}/stop

Stop a running flow. The flow configuration is preserved; only the runtime instance is torn down. The flow can be restarted later.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow to stop |

**Response (200):**

```json
{
  "success": true,
  "data": null
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found in configuration |
| 409 | Flow is not currently running |
| 500 | Engine failed to stop the flow |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/stop \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /api/v1/flows/{flow_id}/restart

Restart a flow (stop + start). Destroys the running instance (if any) and creates a fresh one from the current configuration. Useful for picking up config changes or recovering from transient errors.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Unique identifier of the flow to restart |

**Response (200):**

```json
{
  "success": true,
  "data": null
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found in configuration |
| 500 | Engine failed to create the new flow instance |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/restart \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /api/v1/flows/{flow_id}/activate-input

Switch the active input on a multi-input flow. Atomically marks the target input as active and all other inputs on the flow as passive. If the flow is running, the switch is applied immediately via the engine's watch channel — no task restart, no reconnect gap.

A built-in **TS continuity fixer** ensures clean switching for downstream receivers: output-side CC state is reset (creating a clean-break CC jump that receivers handle via packet-loss recovery), and the new input's cached PAT/PMT is injected immediately with a bumped `version_number` (forcing receivers to re-parse the tables and detect any codec/structure changes). All packets (video, audio, data) are forwarded immediately. **Fully format-agnostic — inputs can use different codecs, containers, and transports** (e.g., H.264 on one input, H.265 on another, JPEG XS on a third, uncompressed ST 2110 on a fourth). Works for any video, audio, or data format. For non-TS transports (raw ST 2110 RTP), the fixer is transparent.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Flow containing the inputs |

**Request body:**

```json
{
  "input_id": "backup-srt"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `input_id` | string | Yes | ID of the input to activate. Must be a member of the flow's `input_ids`. |

**Response (200):**

```json
{
  "success": true,
  "data": { ... }
}
```

Returns the updated `FlowConfig`.

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Input is not a member of the flow |
| 404 | Flow not found |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/activate-input \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "input_id": "backup-srt" }'
```

---

## Outputs

### POST /api/v1/flows/{flow_id}/outputs

Add a new output to an existing flow. The output is validated, appended to the flow's output list, and persisted. If the flow is currently running, the output is hot-added without stopping the flow.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Flow to add the output to |

**Request body:**

An `OutputConfig` object. The `type` field determines the output kind. See the [Configuration Guide](configuration-guide.md) for all output types and their fields.

```json
{
  "type": "srt",
  "id": "srt-backup",
  "name": "SRT Backup Output",
  "mode": "caller",
  "local_addr": "0.0.0.0:0",
  "remote_addr": "203.0.113.20:9000",
  "latency_ms": 300
}
```

**Response (200):**

Returns the created output configuration.

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Output validation failure |
| 404 | Flow not found |
| 409 | An output with the same ID already exists in the flow |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/outputs \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "type": "srt",
    "id": "srt-backup",
    "name": "SRT Backup Output",
    "mode": "caller",
    "local_addr": "0.0.0.0:0",
    "remote_addr": "203.0.113.20:9000",
    "latency_ms": 300
  }'
```

---

### DELETE /api/v1/flows/{flow_id}/outputs/{output_id}

Remove an output from a flow. If the flow is running, the output is hot-removed from the engine first, then removed from config and persisted.

**Auth:** Requires `admin` role.

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Flow containing the output |
| `output_id` | string | Output to remove |

**Response (200):**

```json
{
  "success": true,
  "data": null
}
```

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found, or output not found within the flow |
| 500 | Failed to persist config to disk |

**curl example:**

```bash
curl -X DELETE http://localhost:8080/api/v1/flows/main-feed/outputs/srt-backup \
  -H "Authorization: Bearer $TOKEN"
```

---

## Statistics

### GET /api/v1/stats

Retrieve aggregated system-wide and per-flow statistics. Running flows include live counters; configured-but-stopped flows are included with zeroed counters.

**Auth:** Requires valid JWT (any role).

**Response (200):**

```json
{
  "success": true,
  "data": {
    "system": {
      "uptime_secs": 86400,
      "total_flows": 3,
      "active_flows": 2,
      "version": "0.1.0"
    },
    "flows": [
      {
        "flow_id": "main-feed",
        "flow_name": "Main Program Feed",
        "state": "Running",
        "health": "Healthy",
        "uptime_secs": 86350,
        "input": {
          "input_type": "udp",
          "state": "receiving",
          "packets_received": 15234567,
          "bytes_received": 20113628844,
          "bitrate_bps": 50000000,
          "packets_lost": 12,
          "packets_filtered": 0,
          "packets_recovered_fec": 8,
          "redundancy_switches": 0,
          "srt_stats": null,
          "srt_leg2_stats": null
        },
        "outputs": [
          {
            "output_id": "rtp-out-1",
            "output_name": "Local Playout",
            "output_type": "udp",
            "state": "active",
            "packets_sent": 15234555,
            "bytes_sent": 20113612680,
            "bitrate_bps": 50000000,
            "packets_dropped": 0,
            "fec_packets_sent": 3046911,
            "srt_stats": null,
            "srt_leg2_stats": null
          }
        ],
        "tr101290": {
          "ts_packets_analyzed": 106641969,
          "pat_count": 17280,
          "pmt_count": 17280,
          "sync_loss_count": 0,
          "sync_byte_errors": 0,
          "cc_errors": 0,
          "pat_errors": 0,
          "pmt_errors": 0,
          "tei_errors": 0,
          "pcr_discontinuity_errors": 0,
          "pcr_accuracy_errors": 0,
          "priority1_ok": true,
          "priority2_ok": true,
          "tr07_compliant": false,
          "jpeg_xs_pid": null
        },
        "media_analysis": {
          "protocol": "srt",
          "payload_format": "raw_ts",
          "fec": null,
          "redundancy": null,
          "program_count": 1,
          "programs": [
            {
              "program_number": 1,
              "pmt_pid": 4096,
              "video_streams": [
                {
                  "pid": 256,
                  "codec": "H.264/AVC",
                  "stream_type": 27,
                  "resolution": "1920x1080",
                  "frame_rate": 29.97,
                  "profile": "High",
                  "level": "4.0",
                  "bitrate_bps": 5000000
                }
              ],
              "audio_streams": [
                {
                  "pid": 257,
                  "codec": "AAC-LC",
                  "stream_type": 15,
                  "sample_rate_hz": 48000,
                  "channels": 2,
                  "language": "eng",
                  "bitrate_bps": 128000
                }
              ],
              "total_bitrate_bps": 5128000
            }
          ],
          "video_streams": [
            {
              "pid": 256,
              "codec": "H.264/AVC",
              "stream_type": 27,
              "resolution": "1920x1080",
              "frame_rate": 29.97,
              "profile": "High",
              "level": "4.0",
              "bitrate_bps": 5000000
            }
          ],
          "audio_streams": [
            {
              "pid": 257,
              "codec": "AAC-LC",
              "stream_type": 15,
              "sample_rate_hz": 48000,
              "channels": 2,
              "language": "eng",
              "bitrate_bps": 128000
            }
          ],
          "total_bitrate_bps": 5200000
        },
        "iat": {
          "min_us": 120.5,
          "max_us": 180.3,
          "avg_us": 142.7
        },
        "pdv_jitter_us": 15.2
      }
    ]
  }
}
```

**System stats fields:**

| Field | Type | Description |
|-------|------|-------------|
| `uptime_secs` | integer | Application uptime in seconds |
| `total_flows` | integer | Total configured flows |
| `active_flows` | integer | Currently running flows |
| `version` | string | Application version |

**Per-flow fields:**

| Field | Type | Description |
|-------|------|-------------|
| `flow_id` | string | Flow identifier |
| `flow_name` | string | Display name |
| `state` | string | `"Idle"`, `"Starting"`, `"Running"`, `"Error"`, or `"Stopped"` |
| `health` | string | `"Healthy"`, `"Warning"`, `"Error"`, or `"Critical"` (RP 2129 M6) |
| `uptime_secs` | integer | Seconds since the flow was started |
| `input` | object | Input leg statistics (see below) |
| `outputs` | array | Per-output statistics (see below) |
| `tr101290` | object/null | TR-101290 analysis (present when running) |
| `media_analysis` | object/null | Media content analysis — codec, resolution, frame rate, audio format, per-PID bitrate (present when running and `media_analysis` config is `true`). The `programs[]` array (new) contains one entry per MPEG-TS program with its own `video_streams`/`audio_streams` — use it for MPTS introspection. The top-level `video_streams`/`audio_streams` are kept for backward compatibility and contain the flat union across all programs. |
| `iat` | object/null | Inter-arrival time stats in microseconds |
| `pdv_jitter_us` | float/null | Packet delivery variation (jitter) in microseconds |
| `bandwidth_exceeded` | boolean | `true` if the flow's input bitrate currently exceeds the configured `bandwidth_limit`. Omitted when `false`. |
| `bandwidth_blocked` | boolean | `true` if the flow is currently gated (packets dropped) due to bandwidth limit enforcement. Omitted when `false`. |
| `bandwidth_limit_mbps` | float/null | Configured bandwidth limit in Mbps (for display). Absent if no limit configured. |

**Input stats fields:**

| Field | Type | Description |
|-------|------|-------------|
| `input_type` | string | `"rtp"`, `"srt"`, `"rtmp"`, `"rtsp"`, `"webrtc"`, or `"whep"` |
| `state` | string | Connection state (e.g., `"receiving"`, `"connecting"`) |
| `packets_received` | integer | Total RTP packets received |
| `bytes_received` | integer | Total bytes received |
| `bitrate_bps` | integer | Current bitrate in bits/sec |
| `packets_lost` | integer | Packets lost (sequence gaps) |
| `packets_filtered` | integer | Packets dropped by ingress filters |
| `packets_recovered_fec` | integer | Packets recovered via FEC |
| `srt_stats` | object/null | SRT leg 1 stats (if SRT input) |
| `srt_leg2_stats` | object/null | SRT leg 2 stats (if redundancy enabled) |
| `redundancy_switches` | integer | SMPTE 2022-7 leg switch count |

**Output stats fields:**

| Field | Type | Description |
|-------|------|-------------|
| `output_id` | string | Output identifier |
| `output_name` | string | Display name |
| `output_type` | string | `"udp"`, `"srt"`, `"rtmp"`, `"hls"`, or `"webrtc"` |
| `state` | string | Connection state |
| `packets_sent` | integer | Total packets sent |
| `bytes_sent` | integer | Total bytes sent |
| `bitrate_bps` | integer | Current bitrate in bits/sec |
| `packets_dropped` | integer | Packets dropped (channel full) |
| `fec_packets_sent` | integer | FEC packets sent |
| `srt_stats` | object/null | SRT leg 1 stats (if SRT output) |
| `srt_leg2_stats` | object/null | SRT leg 2 stats (if redundancy) |

**SRT leg stats fields:**

| Field | Type | Description |
|-------|------|-------------|
| `state` | string | SRT socket state (e.g., `"connected"`, `"broken"`) |
| `rtt_ms` | float | Round-trip time in milliseconds |
| `send_rate_mbps` | float | Estimated send rate in Mbps |
| `recv_rate_mbps` | float | Estimated receive rate in Mbps |
| `pkt_loss_total` | integer | Total packets lost |
| `pkt_retransmit_total` | integer | Total retransmitted packets |
| `uptime_ms` | integer | Socket uptime in milliseconds |

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/stats
```

---

### GET /api/v1/stats/{flow_id}

Retrieve statistics for a single flow. Returns live stats if running, or zeroed stats if the flow is configured but stopped.

**Auth:** Requires valid JWT (any role).

**Path parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `flow_id` | string | Flow to retrieve stats for |

**Response (200):**

Returns a single `FlowStats` object (same structure as entries in the `flows` array above).

**Error responses:**

| Status | Condition |
|--------|-----------|
| 404 | Flow not found in engine or configuration |

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/stats/main-feed
```

---

## Configuration

### GET /api/v1/config

Retrieve the running application configuration with infrastructure secrets stripped. Returns the operational config including flow definitions with all user-configured parameters (SRT passphrases, RTSP credentials, RTMP stream keys, bearer tokens). Infrastructure secrets (`node_secret`, tunnel encryption keys, JWT secrets, client credentials, TLS config) are never included.

**Auth:** Requires valid JWT (any role).

**Response (200):**

```json
{
  "success": true,
  "data": {
    "version": 1,
    "server": {
      "listen_addr": "0.0.0.0",
      "listen_port": 8080
    },
    "monitor": {
      "listen_addr": "0.0.0.0",
      "listen_port": 9090
    },
    "flows": [ ... ]
  }
}
```

**curl example:**

```bash
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/config
```

---

### PUT /api/v1/config

Replace the entire application configuration atomically. Stops all running flows, replaces the in-memory config, persists to disk (flow configs including user parameters to `config.json`, infrastructure secrets to `secrets.json`), and starts all flows with `enabled: true`.

**Auth:** Requires `admin` role.

**Request body:**

A complete `AppConfig` JSON object (see [Configuration Guide](configuration-guide.md)). Flow parameters (SRT passphrases, RTSP credentials, RTMP keys, etc.) are stored in `config.json`. Infrastructure secrets (auth config, TLS) are stored in `secrets.json`.

**Response (200):**

Returns the new configuration with infrastructure secrets stripped (flow parameters preserved).

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Config validation failure (duplicate flow IDs, invalid addresses, etc.) |
| 500 | Failed to persist config or individual flows failed to start |

**curl example:**

```bash
curl -X PUT http://localhost:8080/api/v1/config \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d @config.json
```

---

### POST /api/v1/config/reload

Reload configuration from disk (`config.json` + `secrets.json`). Stops all running flows, reads and validates both files, merges secrets into the config, replaces the in-memory state, and starts all enabled flows. Useful after manual edits to the config files.

**Auth:** Requires `admin` role.

**Request body:** None.

**Response (200):**

Returns the reloaded configuration with secrets stripped.

**Error responses:**

| Status | Condition |
|--------|-----------|
| 400 | Loaded config fails validation |
| 500 | Config file cannot be read or parsed |

**curl example:**

```bash
curl -X POST http://localhost:8080/api/v1/config/reload \
  -H "Authorization: Bearer $TOKEN"
```

---

## Prometheus Metrics

### GET /metrics

Prometheus-compatible metrics endpoint. Returns metrics in the Prometheus text exposition format (`text/plain; version=0.0.4`).

**Auth:** Public by default when `public_metrics: true` (the default). When `public_metrics: false`, requires a valid JWT (any role).

**Metric families:**

**Application-level gauges:**

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_info{version="..."}` | gauge | Application version (always 1) |
| `bilbycast_edge_uptime_seconds` | gauge | Seconds since startup |
| `bilbycast_edge_flows_total` | gauge | Total configured flows |
| `bilbycast_edge_flows_active` | gauge | Currently running flows |

**Per-flow input metrics** (labeled by `flow_id`):

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_flow_input_packets_total` | counter | RTP packets received |
| `bilbycast_edge_flow_input_bytes_total` | counter | Bytes received |
| `bilbycast_edge_flow_input_bitrate_bps` | gauge | Input bitrate (bits/sec) |
| `bilbycast_edge_flow_input_packets_lost` | counter | Packets lost |
| `bilbycast_edge_flow_input_fec_recovered_total` | counter | Packets recovered via FEC |
| `bilbycast_edge_flow_input_redundancy_switches_total` | counter | SMPTE 2022-7 leg switches |
| `bilbycast_edge_flow_input_packets_filtered` | counter | Packets dropped by ingress filters |
| `bilbycast_edge_flow_pdv_jitter_us` | gauge | PDV jitter in microseconds |
| `bilbycast_edge_flow_iat_avg_us` | gauge | Average inter-arrival time in microseconds |

**Per-output metrics** (labeled by `flow_id`, `output_id`):

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_flow_output_packets_total` | counter | Packets sent |
| `bilbycast_edge_flow_output_bytes_total` | counter | Bytes sent |
| `bilbycast_edge_flow_output_bitrate_bps` | gauge | Output bitrate (bits/sec) |
| `bilbycast_edge_flow_output_packets_dropped` | counter | Packets dropped |
| `bilbycast_edge_flow_output_fec_sent_total` | counter | FEC packets sent |

**SRT metrics** (labeled by `flow_id`, optionally `output_id` and `leg`):

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_srt_rtt_ms` | gauge | SRT round-trip time in ms |
| `bilbycast_edge_srt_loss_total` | counter | SRT total packet loss |

**TR-101290 metrics** (labeled by `flow_id`):

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_tr101290_ts_packets_total` | counter | TS packets analyzed |
| `bilbycast_edge_tr101290_sync_byte_errors_total` | counter | Sync byte errors |
| `bilbycast_edge_tr101290_cc_errors_total` | counter | Continuity counter errors |
| `bilbycast_edge_tr101290_pat_errors_total` | counter | PAT timeout errors |
| `bilbycast_edge_tr101290_pmt_errors_total` | counter | PMT timeout errors |
| `bilbycast_edge_tr101290_tei_errors_total` | counter | Transport error indicator errors |
| `bilbycast_edge_tr101290_pcr_discontinuity_errors_total` | counter | PCR discontinuity errors |
| `bilbycast_edge_tr101290_pcr_accuracy_errors_total` | counter | PCR accuracy errors |

**Media analysis metrics** (labeled by `flow_id` and `pid`):

| Metric | Type | Description |
|--------|------|-------------|
| `bilbycast_edge_media_video_info` | info | Video stream info (labels: `codec`, `resolution`, `profile`, `level`) |
| `bilbycast_edge_media_video_framerate` | gauge | Video frame rate in fps |
| `bilbycast_edge_media_audio_info` | info | Audio stream info (labels: `codec`, `sample_rate`, `channels`, `language`) |
| `bilbycast_edge_media_pid_bitrate_bps` | gauge | Per-PID bitrate in bits/sec (label: `type` = `video` or `audio`) |
| `bilbycast_edge_media_total_bitrate_bps` | gauge | Total TS bitrate in bits/sec |

Only metrics for currently running flows are emitted.

**curl example:**

```bash
curl http://localhost:8080/metrics
```

---

## Tunnels

### GET /api/v1/tunnels

List all active IP tunnels.

**Auth:** Requires valid JWT (any role) when auth is enabled.

**Response (200 OK):**

```json
{
  "tunnels": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "name": "Stadium to Studio",
      "protocol": "udp",
      "mode": "relay",
      "direction": "egress",
      "state": "Connected",
      "local_addr": "0.0.0.0:9000",
      "relay_addr": "relay.example.com:4433"
    }
  ]
}
```

### GET /api/v1/tunnels/{id}

Get status of a specific tunnel.

**Auth:** Requires valid JWT (any role) when auth is enabled.

**Response (200 OK):**

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "name": "Stadium to Studio",
  "protocol": "udp",
  "mode": "relay",
  "direction": "egress",
  "state": "Connected",
  "local_addr": "0.0.0.0:9000",
  "relay_addr": "relay.example.com:4433"
}
```

**Response (404 Not Found):**

```json
{
  "error": "Tunnel not found"
}
```

### POST /api/v1/tunnels

Create a new IP tunnel. The tunnel configuration is validated before creation.

**Auth:** Requires valid JWT with `admin` role when auth is enabled.

**Request body:** A `TunnelConfig` JSON object. See [Tunnel Configuration](configuration-guide.md#tunnel-configuration) for all fields.

**Example — relay mode UDP tunnel:**

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "name": "Stadium to Studio",
  "enabled": true,
  "protocol": "udp",
  "mode": "relay",
  "direction": "egress",
  "local_addr": "0.0.0.0:9000",
  "relay_addr": "relay.example.com:4433",
  "tunnel_encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

**Response (201 Created):**

```json
{
  "status": "created"
}
```

**Response (400 Bad Request):**

```json
{
  "error": "tunnel_encryption_key is required for relay mode"
}
```

### DELETE /api/v1/tunnels/{id}

Destroy a tunnel and clean up its connections.

**Auth:** Requires valid JWT with `admin` role when auth is enabled.

**Response (200 OK):**

```json
{
  "status": "deleted"
}
```

**Response (404 Not Found):**

```json
{
  "error": "Tunnel not found"
}
```

---

## WebSocket

### GET /api/v1/ws/stats

WebSocket endpoint for real-time statistics streaming. Upgrades the HTTP connection to a WebSocket and pushes JSON stats messages at approximately 1-second intervals.

**Auth:** Requires valid JWT (any role) when auth is enabled. The token can be passed as a standard `Authorization: Bearer` header on the upgrade request or as a query parameter for browser clients (see [Security Guide](api-security.md)).

**Protocol:** This is a server-push channel. The server sends JSON text frames; client-to-server messages are ignored.

**Message format:**

Each WebSocket text frame contains a JSON array of per-flow stats snapshots:

```json
[
  {
    "flow_id": "main-feed",
    "flow_name": "Main Program Feed",
    "state": "Running",
    "health": "Healthy",
    "uptime_secs": 86350,
    "input": { ... },
    "outputs": [ ... ],
    "tr101290": { ... },
    "iat": { ... },
    "pdv_jitter_us": 15.2
  }
]
```

The structure of each flow stats object is identical to the entries in `GET /api/v1/stats`.

**Connection behavior:**

- Messages are broadcast on a shared channel. If a client falls behind, messages are skipped (lagged) rather than buffered.
- The connection closes when the client sends a `Close` frame, disconnects, or when the broadcast channel is closed.

**JavaScript example:**

```javascript
const ws = new WebSocket("ws://localhost:8080/api/v1/ws/stats");

ws.onmessage = (event) => {
  const flows = JSON.parse(event.data);
  flows.forEach(flow => {
    console.log(`${flow.flow_id}: ${flow.input.bitrate_bps} bps, health=${flow.health}`);
  });
};

ws.onerror = (err) => console.error("WebSocket error:", err);
ws.onclose = () => console.log("WebSocket closed");
```

**curl example (wscat):**

```bash
wscat -c "ws://localhost:8080/api/v1/ws/stats" \
  -H "Authorization: Bearer $TOKEN"
```

---

## Error Codes

All API errors return a JSON body with `"success": false` and an `"error"` message string.

| HTTP Status | Error Type | Description |
|-------------|-----------|-------------|
| 400 | Bad Request | Request body failed validation, malformed JSON, unsupported grant type |
| 401 | Unauthorized | Missing or invalid Authorization header, expired token, bad signature |
| 403 | Forbidden | Valid token but insufficient role (admin required) |
| 404 | Not Found | Flow or output does not exist |
| 409 | Conflict | Resource already exists, flow already running/stopped |
| 500 | Internal Server Error | Disk I/O failure, engine error, JWT signing failure |

**Example error responses:**

```json
// 401 Unauthorized
{
  "success": false,
  "error": "missing Authorization header"
}

// 403 Forbidden
{
  "success": false,
  "error": "admin role required"
}

// 404 Not Found
{
  "success": false,
  "error": "Flow 'nonexistent' not found"
}

// 409 Conflict
{
  "success": false,
  "error": "Flow 'main-feed' is already running"
}
```

---

## Endpoint Summary

| Method | Path | Auth | Role | Description |
|--------|------|------|------|-------------|
| GET | `/health` | No | - | Health check |
| POST | `/oauth/token` | No (rate-limited) | - | Get JWT token |
| GET | `/metrics` | Configurable | any | Prometheus metrics |
| GET | `/api/v1/flows` | Yes | any | List all flows |
| GET | `/api/v1/flows/{flow_id}` | Yes | any | Get flow details |
| POST | `/api/v1/flows` | Yes | admin | Create flow |
| PUT | `/api/v1/flows/{flow_id}` | Yes | admin | Update flow |
| DELETE | `/api/v1/flows/{flow_id}` | Yes | admin | Delete flow |
| POST | `/api/v1/flows/{flow_id}/start` | Yes | admin | Start flow |
| POST | `/api/v1/flows/{flow_id}/stop` | Yes | admin | Stop flow |
| POST | `/api/v1/flows/{flow_id}/restart` | Yes | admin | Restart flow |
| POST | `/api/v1/flows/{flow_id}/activate-input` | Yes | admin | Switch active input (seamless TS continuity) |
| POST | `/api/v1/flows/{flow_id}/outputs` | Yes | admin | Add output |
| DELETE | `/api/v1/flows/{flow_id}/outputs/{output_id}` | Yes | admin | Remove output |
| POST | `/api/v1/flows/{flow_id}/whip` | Yes | admin | WHIP: Accept WebRTC publisher (SDP offer → answer) |
| DELETE | `/api/v1/flows/{flow_id}/whip/{session_id}` | Yes | admin | WHIP: Disconnect publisher |
| POST | `/api/v1/flows/{flow_id}/whep` | Yes | admin | WHEP: Accept WebRTC viewer (SDP offer → answer) |
| DELETE | `/api/v1/flows/{flow_id}/whep/{session_id}` | Yes | admin | WHEP: Disconnect viewer |
| GET | `/api/v1/tunnels` | Yes | any | List all tunnels |
| GET | `/api/v1/tunnels/{id}` | Yes | any | Get tunnel status |
| POST | `/api/v1/tunnels` | Yes | admin | Create tunnel |
| DELETE | `/api/v1/tunnels/{id}` | Yes | admin | Delete tunnel |
| GET | `/api/v1/stats` | Yes | any | All statistics |
| GET | `/api/v1/stats/{flow_id}` | Yes | any | Single flow stats |
| GET | `/api/v1/config` | Yes | any | Get running config |
| PUT | `/api/v1/config` | Yes | admin | Replace entire config |
| POST | `/api/v1/config/reload` | Yes | admin | Reload config from disk |
| GET | `/api/v1/ws/stats` | Yes | any | WebSocket stats stream |
| GET | `/x-nmos/node/v1.3/` | Configurable | any | NMOS IS-04: Node API root |
| GET | `/x-nmos/node/v1.3/self` | Configurable | any | NMOS IS-04: Node resource |
| GET | `/x-nmos/node/v1.3/devices/` | Configurable | any | NMOS IS-04: List devices |
| GET | `/x-nmos/node/v1.3/devices/{id}` | Configurable | any | NMOS IS-04: Get device |
| GET | `/x-nmos/node/v1.3/sources/` | Configurable | any | NMOS IS-04: List sources |
| GET | `/x-nmos/node/v1.3/sources/{id}` | Configurable | any | NMOS IS-04: Get source |
| GET | `/x-nmos/node/v1.3/flows/` | Configurable | any | NMOS IS-04: List flows |
| GET | `/x-nmos/node/v1.3/flows/{id}` | Configurable | any | NMOS IS-04: Get flow |
| GET | `/x-nmos/node/v1.3/senders/` | Configurable | any | NMOS IS-04: List senders |
| GET | `/x-nmos/node/v1.3/senders/{id}` | Configurable | any | NMOS IS-04: Get sender |
| GET | `/x-nmos/node/v1.3/receivers/` | Configurable | any | NMOS IS-04: List receivers |
| GET | `/x-nmos/node/v1.3/receivers/{id}` | Configurable | any | NMOS IS-04: Get receiver |
| GET | `/x-nmos/connection/v1.1/single/senders/` | Configurable | any | NMOS IS-05: List senders |
| GET | `/x-nmos/connection/v1.1/single/senders/{id}/staged` | Configurable | any | NMOS IS-05: Get staged params |
| PATCH | `/x-nmos/connection/v1.1/single/senders/{id}/staged` | Configurable | any | NMOS IS-05: Update staged + activate |
| GET | `/x-nmos/connection/v1.1/single/senders/{id}/active` | Configurable | any | NMOS IS-05: Get active params |
| GET | `/x-nmos/connection/v1.1/single/senders/{id}/transporttype` | Configurable | any | NMOS IS-05: Get transport type |
| GET | `/x-nmos/connection/v1.1/single/senders/{id}/constraints` | Configurable | any | NMOS IS-05: Get constraints |
| GET | `/x-nmos/connection/v1.1/single/receivers/` | Configurable | any | NMOS IS-05: List receivers |
| GET | `/x-nmos/connection/v1.1/single/receivers/{id}/staged` | Configurable | any | NMOS IS-05: Get staged params |
| PATCH | `/x-nmos/connection/v1.1/single/receivers/{id}/staged` | Configurable | any | NMOS IS-05: Update staged + activate |
| GET | `/x-nmos/connection/v1.1/single/receivers/{id}/active` | Configurable | any | NMOS IS-05: Get active params |
| GET | `/x-nmos/connection/v1.1/single/receivers/{id}/transporttype` | Configurable | any | NMOS IS-05: Get transport type |
| GET | `/x-nmos/connection/v1.1/single/receivers/{id}/constraints` | Configurable | any | NMOS IS-05: Get constraints |

**Auth notes:**
- "Configurable" for NMOS: requires JWT whenever `auth.enabled: true` (the secure-by-default). Set `nmos_require_auth: false` to opt out — a `SECURITY:` warning is logged at startup.
- "Configurable" for `/metrics`: public by default, requires JWT when `public_metrics: false`
- `/oauth/token` is rate-limited to 10 requests/minute per IP by default (configurable via `token_rate_limit_per_minute`)
