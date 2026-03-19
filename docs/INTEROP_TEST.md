# bilbycast-edge Interoperability Test Procedure

## Quick Start

Paste this to Claude to set up the test:

> Set up the bilbycast interop test as described in `bilbycast-edge/docs/INTEROP_TEST.md`.
> Use `AO_VIDEO.mp4` from the dev_claude folder. The remote SRT source at 192.168.50.186:9000
> may or may not be running — Edge-3 should keep retrying until it comes up. Start the
> bilbycast-manager to monitor all nodes.

## Topology

```
Source A: ffmpeg -c copy /tmp/ao_video_long.ts → srt-live-transmit → SRT :9000
Source B: Remote SRT at 192.168.50.186:9000 (may be offline, edge retries)

[Source A] ──SRT caller──► [Edge-1 :9000 listener]
                              ├──SRT caller :9100──► [srt-live-transmit] ──UDP :7771──► [ffplay PLAYER-1]
                              └──SRT listener :9001──► [Edge-2 caller] (Rust-to-Rust)

[Source B] ──SRT caller──► [Edge-3 :9002 caller → 192.168.50.186:9000]
                              └──SRT listener :9003──► [Edge-4 caller]
                                                         ├──SRT caller :9101──► [srt-live-transmit] ──UDP :7772──► [ffplay PLAYER-2]
                                                         └──UDP TS :5008──► (available for VLC/ffplay)

bilbycast-manager on :8443 monitors all 4 nodes
```

## What This Tests

| # | Test | Transport |
|---|------|-----------|
| 1 | C++ srt-live-transmit → Rust SRT listener input | C++ caller → Rust listener |
| 2 | Rust SRT caller output → C++ srt-live-transmit | Rust caller → C++ listener |
| 3 | Rust SRT listener output → Rust SRT caller input | Rust-to-Rust (Edge-1→Edge-2) |
| 4 | Rust SRT listener output → Rust SRT caller input | Rust-to-Rust (Edge-3→Edge-4) |
| 5 | Fan-out: 1 input → 2 SRT outputs | Edge-1: caller + listener |
| 6 | Fan-out: 1 input → SRT + UDP outputs | Edge-4: SRT caller + UDP |
| 7 | 2-hop SRT chain with video playback | Edge-3 → Edge-4 → ffplay |
| 8 | SRT auto-reconnect on source unavailable | Edge-3 retries 192.168.50.186 |
| 9 | Manager monitoring all nodes | WebSocket dashboard |
| 10 | C++ interop (no SEQUENCE DISCREPANCY) | ISN=0 fix verified |

## Ports Used

| Port | Service |
|------|---------|
| 8443 | bilbycast-manager web UI + API |
| 8080-8083 | Edge 1-4 API servers |
| 8090-8093 | Edge 1-4 monitor dashboards |
| 9000 | Edge-1 SRT listener input |
| 9001 | Edge-1 SRT listener output (for Edge-2) |
| 9002 | Edge-3 SRT caller input (→ 192.168.50.186:9000) |
| 9003 | Edge-3 SRT listener output (for Edge-4) |
| 9100 | srt-live-transmit receiver for Edge-1 output |
| 9101 | srt-live-transmit receiver for Edge-4 output |
| 5008 | Edge-4 UDP TS output |
| 7771 | UDP relay: srt-live-transmit → ffplay PLAYER-1 |
| 7772 | UDP relay: srt-live-transmit → ffplay PLAYER-2 |

## Prerequisites

- macOS with bilbycast-edge and bilbycast-manager built
- `srt-live-transmit` installed (`brew install srt`)
- `ffmpeg` and `ffplay` installed (`brew install ffmpeg`)
- Test video: `/Users/rezarahimi/Development/dev_claude/AO_VIDEO.mp4`

## Prepare Long Test File

The 43-second AO_VIDEO.mp4 must be pre-concatenated into a long .ts file.
Do NOT use `-stream_loop` or `-c:v libx264` re-encoding — both cause
timestamp issues that degrade playback over time.

```bash
# Create 14-minute test file (one-time)
CONCAT=/tmp/concat_list.txt
rm -f $CONCAT
for i in $(seq 1 20); do
  echo "file '/Users/rezarahimi/Development/dev_claude/AO_VIDEO.mp4'" >> $CONCAT
done
ffmpeg -y -f concat -safe 0 -i $CONCAT -c copy -f mpegts /tmp/ao_video_long.ts
```

## Step-by-Step Setup

### Step 1: Start manager

```bash
cd /Users/rezarahimi/Development/dev_claude/bilbycast-manager
# If fresh DB needed:
# rm -f bilbycast-manager.db*
# echo -e "admin\nAdmin\nadmin@test.com\nAdmin123!\nAdmin123!" | ./target/debug/bilbycast-manager setup --config config/default.toml
./target/debug/bilbycast-manager serve --config config/default.toml &
```

Register 4 nodes via API if fresh DB:
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

Put the registration tokens into the edge config files below.

### Step 2: Create edge configs

**Edge-1** (`/tmp/edge-1.json`): SRT listener in :9000, SRT caller out :9100, SRT listener out :9001
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8090 },
  "manager": { "enabled": true, "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN>" },
  "flows": [{
    "id": "ingest-1", "name": "Ingest 1", "enabled": true,
    "input": { "type": "srt", "mode": "listener", "local_addr": "0.0.0.0:9000",
      "latency_ms": 200, "peer_idle_timeout_secs": 60 },
    "outputs": [
      { "type": "srt", "id": "srt-caller-out", "name": "SRT to ffplay-1",
        "mode": "caller", "local_addr": "0.0.0.0:0", "remote_addr": "127.0.0.1:9100",
        "latency_ms": 200, "peer_idle_timeout_secs": 60 },
      { "type": "srt", "id": "srt-listener-out", "name": "SRT Listener for Edge-2",
        "mode": "listener", "local_addr": "0.0.0.0:9001",
        "latency_ms": 200, "peer_idle_timeout_secs": 60 }
    ]
  }]
}
```

**Edge-2** (`/tmp/edge-2.json`): SRT caller in from Edge-1 :9001
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8081 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8091 },
  "manager": { "enabled": true, "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN>" },
  "flows": [{
    "id": "chain-from-1", "name": "SRT Chain from Edge-1", "enabled": true,
    "input": { "type": "srt", "mode": "caller", "local_addr": "0.0.0.0:0",
      "remote_addr": "127.0.0.1:9001", "latency_ms": 200, "peer_idle_timeout_secs": 60 },
    "outputs": []
  }]
}
```

**Edge-3** (`/tmp/edge-3.json`): SRT caller to remote 192.168.50.186:9000, SRT listener out :9003
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8082 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8092 },
  "manager": { "enabled": true, "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN>" },
  "flows": [{
    "id": "remote-ingest", "name": "Remote SRT Ingest", "enabled": true,
    "input": { "type": "srt", "mode": "caller", "local_addr": "0.0.0.0:0",
      "remote_addr": "192.168.50.186:9000", "latency_ms": 200, "peer_idle_timeout_secs": 60 },
    "outputs": [
      { "type": "srt", "id": "srt-listener-out", "name": "SRT Listener for Edge-4",
        "mode": "listener", "local_addr": "0.0.0.0:9003",
        "latency_ms": 200, "peer_idle_timeout_secs": 60 }
    ]
  }]
}
```

**Edge-4** (`/tmp/edge-4.json`): SRT caller from Edge-3 :9003, SRT caller out :9101, UDP out :5008
```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8083 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 8093 },
  "manager": { "enabled": true, "url": "ws://127.0.0.1:8443/ws/node",
    "registration_token": "<TOKEN>" },
  "flows": [{
    "id": "chain-from-3", "name": "SRT Chain from Edge-3", "enabled": true,
    "input": { "type": "srt", "mode": "caller", "local_addr": "0.0.0.0:0",
      "remote_addr": "127.0.0.1:9003", "latency_ms": 200, "peer_idle_timeout_secs": 60 },
    "outputs": [
      { "type": "srt", "id": "srt-caller-out", "name": "SRT to ffplay-2",
        "mode": "caller", "local_addr": "0.0.0.0:0", "remote_addr": "127.0.0.1:9101",
        "latency_ms": 200, "peer_idle_timeout_secs": 60 },
      { "type": "rtp", "id": "udp-out", "name": "UDP TS out", "dest_addr": "127.0.0.1:5008" }
    ]
  }]
}
```

### Step 3: Start everything (order matters)

```bash
cd /Users/rezarahimi/Development/dev_claude/bilbycast-edge

# Receivers first (SRT listeners for edge caller outputs)
srt-live-transmit "srt://:9100?mode=listener&latency=200" "udp://127.0.0.1:7771" 2>/dev/null &
srt-live-transmit "srt://:9101?mode=listener&latency=200" "udp://127.0.0.1:7772" 2>/dev/null &
sleep 1

# Listener edges first, then caller edges
./target/debug/bilbycast-edge --config /tmp/edge-1.json 2>/dev/null &
./target/debug/bilbycast-edge --config /tmp/edge-3.json 2>/dev/null &
sleep 3
./target/debug/bilbycast-edge --config /tmp/edge-2.json 2>/dev/null &
./target/debug/bilbycast-edge --config /tmp/edge-4.json 2>/dev/null &
sleep 3

# Source for Edge-1 (copy mode, pre-concatenated long file)
ffmpeg -re -i /tmp/ao_video_long.ts -c copy -f mpegts pipe:1 2>/dev/null | \
  srt-live-transmit "file://con" "srt://127.0.0.1:9000?mode=caller&latency=200" 2>/dev/null &
sleep 3

# Players
ffplay -probesize 5000000 -analyzeduration 5000000 \
  -f mpegts "udp://127.0.0.1:7771?localport=7771&overrun_nonfatal=1&fifo_size=50000000" \
  -window_title "PLAYER-1: Edge-1 SRT caller out" -x 640 -y 360 2>/dev/null &

ffplay -probesize 5000000 -analyzeduration 5000000 \
  -f mpegts "udp://127.0.0.1:7772?localport=7772&overrun_nonfatal=1&fifo_size=50000000" \
  -window_title "PLAYER-2: Edge-3→Edge-4 chain" -x 640 -y 360 2>/dev/null &
```

### Step 4: Verify

```bash
# Check all edges
for PORT in 8080 8081 8082 8083; do
  E=$((PORT - 8079))
  echo "--- Edge-$E ---"
  curl -s "http://localhost:$PORT/api/v1/stats" | python3 -m json.tool
done

# Manager dashboard
open http://localhost:8443
# Login: admin / Admin123!
```

### Step 5: Teardown

```bash
pkill -f "bilbycast" ; pkill -f "srt-live" ; pkill -f "ffmpeg" ; pkill -f "ffplay"
```

## Expected Results

| Path | Bitrate | Loss |
|------|---------|------|
| Source A → Edge-1 | ~8-9 Mbps | 0 |
| Edge-1 → srt-live-transmit → PLAYER-1 | ~8-9 Mbps | 0 |
| Edge-1 → Edge-2 (Rust-to-Rust SRT) | ~8-9 Mbps | 0 |
| Remote → Edge-3 (when available) | ~3-10 Mbps | 0 |
| Edge-3 → Edge-4 (Rust-to-Rust SRT) | ~3-10 Mbps | 0 |
| Edge-4 → srt-live-transmit → PLAYER-2 | ~3-10 Mbps | 0 |
| Edge-4 → UDP :5008 | ~3-10 Mbps | 0 |

Edge-3 will show "SRT connection attempt N failed, retrying" until
192.168.50.186:9000 comes online. Once it does, data flows automatically.
Edge-4 and PLAYER-2 will start showing video at that point.

## Important Notes

- **Do NOT re-encode** (`-c:v libx264`): causes timestamp jitter that degrades playback
- **Do NOT use `-stream_loop`**: causes timestamp discontinuities at loop points
- **Pre-concat** the video into a long .ts file instead (see Prepare section)
- **Start order matters**: receivers → listener edges → caller edges → sources → players
- **SRT listener accepts one connection**: if a source dies, restart both the edge and the source
- **VLC needs MPEG-TS over UDP**, not RTP (no SDP). Use `ffplay` for RTP streams.
