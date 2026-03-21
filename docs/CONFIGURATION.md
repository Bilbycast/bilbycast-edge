# Configuration Reference

bilbycast-edge is configured via a JSON file (default: `./config.json`). Changes made through the REST API are persisted to this file immediately.

---

## Full Config Structure

```json
{
  "version": 1,
  "node_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "tls": null,
    "auth": null
  },
  "monitor": null,
  "manager": null,
  "flows": []
}
```

## Node Identity

| Field     | Type      | Default | Description |
|-----------|-----------|---------|-------------|
| `node_id` | `string?` | Auto-generated | Persistent UUID v4 identifying this edge node. Auto-generated on first startup and saved to config. Used as the NMOS IS-04 Node ID. All NMOS resource UUIDs are derived from this value (UUID v5), so they remain stable across restarts. |

---

## Server Section

API server configuration.

| Field         | Type          | Default      | Description                          |
|---------------|---------------|--------------|--------------------------------------|
| `listen_addr` | `string`      | `"0.0.0.0"`  | API listen address                   |
| `listen_port` | `u16`         | `8080`       | API listen port                      |
| `tls`         | `TlsConfig?`  | `null`       | Optional TLS for HTTPS (see below)   |
| `auth`        | `AuthConfig?`  | `null`       | Optional API authentication (see below) |

### TLS Configuration

Enables HTTPS on the API server. Requires building with `--features tls`.

| Field       | Type     | Description                          |
|-------------|----------|--------------------------------------|
| `cert_path` | `string` | Path to PEM-encoded TLS certificate  |
| `key_path`  | `string` | Path to PEM-encoded TLS private key  |

```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "certs/server.crt",
      "key_path": "certs/server.key"
    }
  }
}
```

### Authentication Configuration

OAuth 2.0 client credentials authentication with JWT (HMAC-SHA256). When absent or `enabled: false`, all API endpoints are unauthenticated.

| Field                  | Type           | Default | Description                                    |
|------------------------|----------------|---------|------------------------------------------------|
| `enabled`              | `bool`         | --      | Master switch for authentication               |
| `jwt_secret`           | `string`       | --      | HMAC-SHA256 secret for signing JWTs            |
| `token_lifetime_secs`  | `u64`          | `3600`  | Token validity period in seconds               |
| `clients`              | `AuthClient[]` | --      | List of registered OAuth clients               |
| `public_metrics`       | `bool`         | `true`  | Allow unauthenticated access to `/metrics` and `/health` |

Each client entry:

| Field           | Type     | Description                                    |
|-----------------|----------|------------------------------------------------|
| `client_id`     | `string` | Client identifier                              |
| `client_secret` | `string` | Client secret                                  |
| `role`          | `string` | `"admin"` (full access) or `"monitor"` (read-only) |

To obtain a token, POST to `/oauth/token` with `grant_type=client_credentials`, `client_id`, and `client_secret`.

---

## Monitor Section

Optional web monitoring dashboard. When present, bilbycast-edge starts a second HTTP server serving a self-contained HTML dashboard for browser-based status monitoring.

| Field         | Type     | Description                          |
|---------------|----------|--------------------------------------|
| `listen_addr` | `string` | Dashboard listen address             |
| `listen_port` | `u16`    | Dashboard listen port                |

Omit this section entirely to disable the dashboard.

```json
{
  "monitor": {
    "listen_addr": "0.0.0.0",
    "listen_port": 9090
  }
}
```

---

## Manager Section

Optional connection to a bilbycast-manager instance for centralized monitoring and remote control.

| Field                | Type      | Default | Description                                     |
|----------------------|-----------|---------|-------------------------------------------------|
| `enabled`            | `bool`    | `false` | Enable the manager connection                   |
| `url`                | `string`  | --      | Manager WebSocket URL, e.g. `"wss://manager-host:8443/ws/node"` (must use `wss://`) |
| `accept_self_signed_cert` | `bool` | `false` | Accept self-signed TLS certificates from the manager. **Dev/testing only** — disables cert validation. |
| `registration_token` | `string?` | `null`  | One-time registration token (first connection only) |
| `node_id`            | `string?` | `null`  | Assigned node ID (set automatically after registration) |
| `node_secret`        | `string?` | `null`  | Assigned node secret (set automatically after registration) |

On first connection, provide `registration_token`. After successful registration, the manager returns `node_id` and `node_secret`, which are written back to the config file automatically. The `registration_token` is cleared. Subsequent connections use `node_id` and `node_secret`.

```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "registration_token": "abc123-one-time-token"
  }
}
```

After registration, the file is updated to:

```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "node_id": "assigned-uuid",
    "node_secret": "assigned-secret"
  }
}
```

---

## Flows Section

An array of flow definitions. Each flow has one input and one or more outputs.

| Field     | Type             | Default | Description                              |
|-----------|------------------|---------|------------------------------------------|
| `id`      | `string`         | --      | Unique flow identifier                   |
| `name`    | `string`         | --      | Human-readable name                      |
| `enabled` | `bool`           | `true`  | Auto-start when the application starts   |
| `input`   | `InputConfig`    | --      | Single input source                      |
| `outputs` | `OutputConfig[]` | --      | One or more output destinations          |

---

## Input Types

The input is discriminated by the `"type"` field.

### SRT Input (`"type": "srt"`)

| Field         | Type                    | Default | Description                                     |
|---------------|-------------------------|---------|-------------------------------------------------|
| `type`        | `"srt"`                 | --      | Input type discriminator                        |
| `mode`        | `SrtMode`               | --      | `"caller"`, `"listener"`, or `"rendezvous"`     |
| `local_addr`  | `string`                | --      | Local bind address, e.g. `"0.0.0.0:9000"`      |
| `remote_addr` | `string?`               | `null`  | Remote address (required for caller/rendezvous) |
| `latency_ms`  | `u64`                   | `120`   | SRT latency in milliseconds                     |
| `passphrase`  | `string?`               | `null`  | AES encryption passphrase (10-79 characters)    |
| `aes_key_len` | `usize?`                | `null`  | AES key length: 16, 24, or 32                   |
| `redundancy`  | `SrtRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |

### RTP Input (`"type": "rtp"`)

| Field                    | Type         | Default | Description                                     |
|--------------------------|--------------|---------|-------------------------------------------------|
| `type`                   | `"rtp"`      | --      | Input type discriminator                        |
| `bind_addr`              | `string`     | --      | Local bind address, e.g. `"0.0.0.0:5000"` or `"239.1.1.1:5000"` for multicast |
| `interface_addr`         | `string?`    | `null`  | Network interface IP for multicast join          |
| `fec_decode`             | `FecConfig?` | `null`  | SMPTE 2022-1 FEC decode parameters              |
| `tr07_mode`              | `bool?`      | `null`  | Enable VSF TR-07 (JPEG XS) compliance reporting |
| `allowed_sources`        | `string[]?`  | `null`  | Source IP allowlist (RP 2129 C5)                 |
| `allowed_payload_types`  | `u8[]?`      | `null`  | RTP payload type allowlist (RP 2129 U4)          |
| `max_bitrate_mbps`       | `f64?`       | `null`  | Maximum ingress bitrate in Mbps (RP 2129 C7)     |

### RTMP Input (`"type": "rtmp"`)

Accepts incoming RTMP publish connections from OBS, ffmpeg, Wirecast, or any RTMP encoder. The received H.264 video and AAC audio are remuxed into MPEG-TS and pushed through the flow pipeline.

| Field            | Type      | Default  | Description                                     |
|------------------|-----------|----------|-------------------------------------------------|
| `type`           | `"rtmp"`  | --       | Input type discriminator                        |
| `listen_addr`    | `string`  | --       | RTMP listen address, e.g. `"0.0.0.0:1935"`     |
| `app`            | `string`  | `"live"` | RTMP application name (URL path component)      |
| `stream_key`     | `string?` | `null`   | Stream key for authentication (null = accept any)|
| `max_publishers` | `u32`     | `1`      | Max simultaneous publishers                     |

**Example:**

```json
{
  "type": "rtmp",
  "listen_addr": "0.0.0.0:1935",
  "app": "live",
  "stream_key": "my_secret_key"
}
```

**Publishing from ffmpeg:**
```bash
ffmpeg -re -i input.mp4 -c:v libx264 -c:a aac -f flv rtmp://edge:1935/live/my_secret_key
```

**Publishing from OBS:**
- Server: `rtmp://edge:1935/live`
- Stream Key: `my_secret_key`

---

## Output Types

Outputs are discriminated by the `"type"` field. All output types share `id` and `name` fields.

### SRT Output (`"type": "srt"`)

| Field         | Type                    | Default | Description                                     |
|---------------|-------------------------|---------|-------------------------------------------------|
| `type`        | `"srt"`                 | --      | Output type discriminator                       |
| `id`          | `string`                | --      | Unique output ID within this flow               |
| `name`        | `string`                | --      | Human-readable name                             |
| `mode`        | `SrtMode`               | --      | `"caller"`, `"listener"`, or `"rendezvous"`     |
| `local_addr`  | `string`                | --      | Local bind address                              |
| `remote_addr` | `string?`               | `null`  | Remote address (required for caller/rendezvous) |
| `latency_ms`  | `u64`                   | `120`   | SRT latency in milliseconds                     |
| `passphrase`  | `string?`               | `null`  | AES encryption passphrase (10-79 characters)    |
| `aes_key_len` | `usize?`                | `null`  | AES key length: 16, 24, or 32                   |
| `redundancy`  | `SrtRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |

### RTP Output (`"type": "rtp"`)

| Field            | Type         | Default | Description                                     |
|------------------|--------------|---------|-------------------------------------------------|
| `type`           | `"rtp"`      | --      | Output type discriminator                       |
| `id`             | `string`     | --      | Unique output ID within this flow               |
| `name`           | `string`     | --      | Human-readable name                             |
| `dest_addr`      | `string`     | --      | Destination address, e.g. `"192.168.1.100:5004"` |
| `bind_addr`      | `string?`    | `null`  | Source bind address (default: `"0.0.0.0:0"`)    |
| `interface_addr` | `string?`    | `null`  | Network interface IP for multicast send          |
| `fec_encode`     | `FecConfig?` | `null`  | SMPTE 2022-1 FEC encode parameters              |
| `dscp`           | `u8`         | `46`    | DSCP value for QoS marking (0-63, default: 46 = Expedited Forwarding) |

### RTMP Output (`"type": "rtmp"`)

H.264/AAC only. Supports RTMPS (TLS) via `rtmps://` URLs.

| Field                      | Type    | Default | Description                                     |
|----------------------------|---------|---------|-------------------------------------------------|
| `type`                     | `"rtmp"`| --      | Output type discriminator                       |
| `id`                       | `string`| --      | Unique output ID within this flow               |
| `name`                     | `string`| --      | Human-readable name                             |
| `dest_url`                 | `string`| --      | RTMP destination URL, e.g. `"rtmp://live.twitch.tv/app"` |
| `stream_key`               | `string`| --      | Stream key for authentication                   |
| `reconnect_delay_secs`     | `u64`   | `5`     | Delay between reconnection attempts             |
| `max_reconnect_attempts`   | `u32?`  | `null`  | Maximum reconnect attempts (null = unlimited)   |

### HLS Output (`"type": "hls"`)

Segment-based HTTP ingest. Supports HEVC/HDR content.

| Field                  | Type      | Default | Description                                     |
|------------------------|-----------|---------|-------------------------------------------------|
| `type`                 | `"hls"`   | --      | Output type discriminator                       |
| `id`                   | `string`  | --      | Unique output ID within this flow               |
| `name`                 | `string`  | --      | Human-readable name                             |
| `ingest_url`           | `string`  | --      | HLS ingest base URL for segment upload           |
| `segment_duration_secs`| `f64`     | `2.0`   | Target segment duration (range: 0.5-10.0)        |
| `auth_token`           | `string?` | `null`  | Bearer token for Authorization header            |
| `max_segments`         | `usize`   | `5`     | Maximum segments in rolling playlist             |

### WebRTC Output (`"type": "webrtc"`)

WHIP signaling for WebRTC PeerConnection. Video only supports H.264; audio supports Opus passthrough only.

| Field          | Type      | Default | Description                                     |
|----------------|-----------|---------|-------------------------------------------------|
| `type`         | `"webrtc"`| --      | Output type discriminator                       |
| `id`           | `string`  | --      | Unique output ID within this flow               |
| `name`         | `string`  | --      | Human-readable name                             |
| `whip_url`     | `string`  | --      | WHIP endpoint URL for signaling                  |
| `bearer_token` | `string?` | `null`  | Bearer token for WHIP authentication             |
| `video_only`   | `bool`    | `false` | Send only video (use when audio is AAC)          |

---

## Shared Types

### SRT Mode (`SrtMode`)

| Value          | Description                                              |
|----------------|----------------------------------------------------------|
| `"caller"`     | Active mode: initiates connection to a remote listener   |
| `"listener"`   | Passive mode: waits for incoming connection              |
| `"rendezvous"` | Symmetric mode: both sides connect simultaneously (NAT traversal) |

`"caller"` and `"rendezvous"` require `remote_addr` to be set.

### SRT Redundancy Config (`SrtRedundancyConfig`)

Defines the second SRT leg for SMPTE 2022-7 hitless redundancy. The primary SRT settings in the parent define leg 1; this defines leg 2.

| Field         | Type      | Default | Description                              |
|---------------|-----------|---------|------------------------------------------|
| `mode`        | `SrtMode` | --      | SRT mode for leg 2                       |
| `local_addr`  | `string`  | --      | Bind address for leg 2                   |
| `remote_addr` | `string?` | `null`  | Remote address for leg 2                 |
| `latency_ms`  | `u64`     | `120`   | SRT latency for leg 2                    |
| `passphrase`  | `string?` | `null`  | AES passphrase for leg 2                 |
| `aes_key_len` | `usize?`  | `null`  | AES key length for leg 2                 |

### FEC Config (`FecConfig`)

SMPTE 2022-1 Forward Error Correction parameters.

| Field     | Type | Range | Description                              |
|-----------|------|-------|------------------------------------------|
| `columns` | `u8` | 1-20  | L parameter (columns in FEC matrix)      |
| `rows`    | `u8` | 4-20  | D parameter (rows in FEC matrix)         |

---

## Example Configurations

### Simple SRT to RTP Bridge

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "flows": [
    {
      "id": "srt-to-rtp",
      "name": "SRT to RTP Bridge",
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
          "id": "rtp-out",
          "name": "RTP Output",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### SRT with 2022-7 Redundancy and FEC Output

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "redundant-with-fec",
      "name": "Redundant SRT to FEC-protected RTP",
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
          "id": "rtp-fec",
          "name": "RTP with FEC",
          "dest_addr": "192.168.1.50:5004",
          "fec_encode": { "columns": 10, "rows": 10 }
        }
      ]
    }
  ]
}
```

### Multi-Output Fan-out with RTMP and HLS

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "fanout",
      "name": "SRT to RTMP + HLS + RTP",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200
      },
      "outputs": [
        {
          "type": "rtmp",
          "id": "twitch",
          "name": "Twitch",
          "dest_url": "rtmp://live.twitch.tv/app",
          "stream_key": "live_xxxxx",
          "reconnect_delay_secs": 5
        },
        {
          "type": "hls",
          "id": "youtube-hls",
          "name": "YouTube HLS",
          "ingest_url": "https://a.upload.youtube.com/http_upload_hls",
          "segment_duration_secs": 2.0,
          "auth_token": "your-youtube-token"
        },
        {
          "type": "rtp",
          "id": "local-rtp",
          "name": "Local RTP",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### Manager-Connected Node

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "registration_token": "token-from-manager-dashboard"
  },
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Feed",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200,
        "passphrase": "my-srt-encryption-key"
      },
      "outputs": [
        {
          "type": "srt",
          "id": "srt-relay",
          "name": "SRT Relay",
          "mode": "caller",
          "local_addr": "0.0.0.0:0",
          "remote_addr": "10.0.1.100:9000",
          "latency_ms": 200
        }
      ]
    }
  ]
}
```
