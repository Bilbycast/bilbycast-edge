# bilbycast-edge Interoperability Test Procedure

## Overview

This test validates SRT and RTP flows across multiple bilbycast-edge nodes, native C++ SRT tools (srt-live-transmit), ffmpeg, and VLC. It tests:

- SRT caller/listener between edge nodes (Rust-to-Rust)
- SRT interop with C++ srt-live-transmit
- SRT ingest from remote source
- SRT ingest from local ffmpeg
- RTP forwarding between edge nodes
- RTP playback to VLC
- Manager monitoring of all nodes

## Test Topology

```
[Remote SRT 192.168.50.186:9000]          [ffmpeg AO_VIDEO.mp4]
         │ SRT caller                          │ srt-live-transmit
         ▼                                     ▼ SRT caller
┌──────────────────────┐            ┌──────────────────────┐
│  mac_edge-1 (:8080)  │            │  mac_edge-4 (:8083)  │
│  SRT input (caller)  │            │  SRT input (listener │
│                      │            │           on :9002)  │
│  Out 1: RTP → VLC-1  │            │  Out 1: SRT caller   │
│         (:5004)      │            │    → srt-live-transmit│
│  Out 2: SRT listener │            │         (:9003)      │
│         (:9001)      │            │  Out 2: RTP → VLC-4  │
│  Out 3: RTP → edge-3 │            │         (:5008)      │
│         (:5006)      │            └──────────────────────┘
└───┬─────────┬────────┘
    │SRT      │RTP
    ▼         ▼
┌────────────┐ ┌─────────────┐
│mac_edge-2  │ │ mac_edge-3  │
│(:8081)     │ │ (:8082)     │
│SRT caller  │ │ RTP input   │
│from :9001  │ │ on :5006    │
│            │ │             │
│Out: RTP    │ │Out: RTP     │
│→ VLC-2    │ │→ VLC-3     │
│(:5005)    │ │(:5007)     │
└────────────┘ └─────────────┘

VLC-1 (rtp://@:5004) — Remote feed via Edge-1
VLC-2 (rtp://@:5005) — Remote feed via Edge-1 SRT → Edge-2
VLC-3 (rtp://@:5007) — Remote feed via Edge-1 RTP → Edge-3
VLC-4 (rtp://@:5008) — Local AO_VIDEO via Edge-4
```

## Prerequisites

- macOS with bilbycast-edge and bilbycast-manager built
- `srt-live-transmit` installed (`brew install srt`)
- `ffmpeg` installed
- VLC installed at `/Applications/VLC.app`
- Test video: `AO_VIDEO.mp4` in the dev_claude directory
- Optional: Remote SRT source at 192.168.50.186:9000

## Ports Used

| Port  | Service                          |
|-------|----------------------------------|
| 8443  | bilbycast-manager web UI + API   |
| 8080  | mac_edge-1 API                   |
| 8081  | mac_edge-2 API                   |
| 8082  | mac_edge-3 API                   |
| 8083  | mac_edge-4 API                   |
| 8090-8093 | Edge monitor dashboards      |
| 9000  | Remote SRT source (external)     |
| 9001  | Edge-1 SRT listener output       |
| 9002  | Edge-4 SRT listener input        |
| 9003  | Native SRT receiver (srt-live-transmit) |
| 5004  | VLC-1 RTP (from edge-1)          |
| 5005  | VLC-2 RTP (from edge-2)          |
| 5006  | Edge-3 RTP input (from edge-1)   |
| 5007  | VLC-3 RTP (from edge-3)          |
| 5008  | VLC-4 RTP (from edge-4)          |

## Step-by-Step Setup

### Step 1: Clean up and start manager

```bash
# Kill old processes
pkill -f "bilbycast" ; pkill -f "srt-live" ; pkill -f "ffmpeg" ; pkill -f VLC

# Start manager (fresh DB)
cd bilbycast-manager
rm -f bilbycast-manager.db*
echo -e "admin\nAdmin\nadmin@test.com\nAdmin123!\nAdmin123!" | \
  ./target/debug/bilbycast-manager setup --config config/default.toml
./target/debug/bilbycast-manager serve --config config/default.toml &
sleep 2
```

### Step 2: Register nodes

```bash
TOKEN=$(curl -s http://localhost:8443/api/v1/auth/login -X POST \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"Admin123!"}' | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

for NAME in mac_edge-1 mac_edge-2 mac_edge-3 mac_edge-4; do
  curl -s http://localhost:8443/api/v1/nodes -X POST \
    -H "Content-Type: application/json" -H "Authorization: Bearer $TOKEN" \
    -d "{\"name\":\"$NAME\",\"location\":\"Local Mac\"}"
  echo ""
done
```

Save the registration tokens and put them in the config files below.

### Step 3: Create config files

Create `/tmp/mac-edge-1.json`:
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8090 },
  "manager": {
    "enabled": true,
    "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN_FROM_STEP_2>"
  },
  "flows": [{
    "id": "remote-ingest",
    "name": "Remote SRT Ingest + Fanout",
    "enabled": true,
    "input": {
      "type": "srt", "mode": "caller",
      "local_addr": "0.0.0.0:0", "remote_addr": "192.168.50.186:9000",
      "latency_ms": 200, "peer_idle_timeout_secs": 60
    },
    "outputs": [
      { "type": "rtp", "id": "vlc1", "name": "RTP to VLC-1", "dest_addr": "127.0.0.1:5004" },
      { "type": "srt", "id": "srt-to-edge2", "name": "SRT Listener for Edge-2",
        "mode": "listener", "local_addr": "0.0.0.0:9001", "latency_ms": 200, "peer_idle_timeout_secs": 60 },
      { "type": "rtp", "id": "rtp-to-edge3", "name": "RTP to Edge-3", "dest_addr": "127.0.0.1:5006" }
    ]
  }]
}
```

Create `/tmp/mac-edge-2.json`:
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8081 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8091 },
  "manager": {
    "enabled": true,
    "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN_FROM_STEP_2>"
  },
  "flows": [{
    "id": "srt-from-edge1",
    "name": "SRT from Edge-1",
    "enabled": true,
    "input": {
      "type": "srt", "mode": "caller",
      "local_addr": "0.0.0.0:0", "remote_addr": "127.0.0.1:9001",
      "latency_ms": 200, "peer_idle_timeout_secs": 60
    },
    "outputs": [
      { "type": "rtp", "id": "vlc2", "name": "RTP to VLC-2", "dest_addr": "127.0.0.1:5005" }
    ]
  }]
}
```

Create `/tmp/mac-edge-3.json`:
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8082 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8092 },
  "manager": {
    "enabled": true,
    "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN_FROM_STEP_2>"
  },
  "flows": [{
    "id": "rtp-from-edge1",
    "name": "RTP from Edge-1",
    "enabled": true,
    "input": { "type": "rtp", "bind_addr": "0.0.0.0:5006" },
    "outputs": [
      { "type": "rtp", "id": "vlc3", "name": "RTP to VLC-3", "dest_addr": "127.0.0.1:5007" }
    ]
  }]
}
```

Create `/tmp/mac-edge-4.json`:
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8083 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8093 },
  "manager": {
    "enabled": true,
    "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN_FROM_STEP_2>"
  },
  "flows": [{
    "id": "local-ingest",
    "name": "Local AO_VIDEO Ingest",
    "enabled": true,
    "input": {
      "type": "srt", "mode": "listener",
      "local_addr": "0.0.0.0:9002", "latency_ms": 200, "peer_idle_timeout_secs": 60
    },
    "outputs": [
      { "type": "srt", "id": "srt-to-native", "name": "SRT to Native Receiver",
        "mode": "caller", "local_addr": "0.0.0.0:0", "remote_addr": "127.0.0.1:9003",
        "latency_ms": 200, "peer_idle_timeout_secs": 60 },
      { "type": "rtp", "id": "vlc4", "name": "RTP to VLC-4", "dest_addr": "127.0.0.1:5008" }
    ]
  }]
}
```

### Step 4: Start native SRT receiver and edge nodes

```bash
cd bilbycast-edge

# Native SRT receiver (receives from edge-4)
srt-live-transmit "srt://:9003?mode=listener&latency=200" "udp://127.0.0.1:7000" &

# Start edges (order matters: listeners before callers)
./target/debug/bilbycast-edge --config /tmp/mac-edge-4.json &
sleep 2
./target/debug/bilbycast-edge --config /tmp/mac-edge-1.json &
sleep 2
./target/debug/bilbycast-edge --config /tmp/mac-edge-3.json &
sleep 2
./target/debug/bilbycast-edge --config /tmp/mac-edge-2.json &
sleep 3
```

### Step 5: Start ffmpeg source and VLC players

```bash
# Local video source → edge-4 via SRT
ffmpeg -re -stream_loop -1 -i AO_VIDEO.mp4 \
  -c copy -f mpegts pipe:1 2>/dev/null | \
  srt-live-transmit "file://con" "srt://127.0.0.1:9002?mode=caller&latency=200" &

# VLC players
/Applications/VLC.app/Contents/MacOS/VLC "rtp://@:5004" --network-caching=500 &
/Applications/VLC.app/Contents/MacOS/VLC "rtp://@:5005" --network-caching=500 &
/Applications/VLC.app/Contents/MacOS/VLC "rtp://@:5007" --network-caching=500 &
/Applications/VLC.app/Contents/MacOS/VLC "rtp://@:5008" --network-caching=500 &
```

### Step 6: Verify

```bash
# Check all edge stats
for PORT in 8080 8081 8082 8083; do
  echo "=== Edge on :${PORT} ==="
  curl -s "http://localhost:${PORT}/api/v1/stats" | python3 -m json.tool
done

# Check manager dashboard
open http://localhost:8443
# Login: admin / Admin123!
```

## Expected Results

| Flow Path | Transport | Expected Bitrate | VLC Window |
|-----------|-----------|-----------------|------------|
| Remote → Edge-1 → VLC-1 | SRT in, RTP out | ~3-10 Mbps | VLC-1 (:5004) |
| Remote → Edge-1 → Edge-2 → VLC-2 | SRT→SRT→RTP | ~3-10 Mbps | VLC-2 (:5005) |
| Remote → Edge-1 → Edge-3 → VLC-3 | SRT→RTP→RTP | ~3-10 Mbps | VLC-3 (:5007) |
| AO_VIDEO → Edge-4 → VLC-4 | SRT in, RTP out | ~7-9 Mbps | VLC-4 (:5008) |
| AO_VIDEO → Edge-4 → Native SRT | SRT in, SRT out | ~7-9 Mbps | (srt-live-transmit) |

## What This Tests

1. **SRT caller input** — Edge-1 connects to remote SRT, Edge-4 receives from ffmpeg
2. **SRT listener input** — Edge-4 accepts connection from srt-live-transmit
3. **SRT listener output** — Edge-1 serves data to Edge-2 (Rust-to-Rust)
4. **SRT caller output** — Edge-4 sends to native srt-live-transmit (Rust-to-C++)
5. **RTP output** — All edges output to VLC via RTP/UDP
6. **RTP input** — Edge-3 receives RTP from Edge-1
7. **Fan-out** — Edge-1 has 3 simultaneous outputs from 1 input
8. **Manager monitoring** — All 4 nodes visible in dashboard with live stats
9. **Auto-reconnect** — Edge-1 retries remote SRT with exponential backoff
10. **Interop** — Rust SRT ↔ C++ libsrt (srt-live-transmit, ffmpeg)

## Teardown

```bash
pkill -f "bilbycast" ; pkill -f "srt-live" ; pkill -f "ffmpeg" ; pkill -f VLC
```
