# bilbycast-edge

Media transport edge node supporting SRT, RTP/UDP, RTMP, HLS, and WebRTC protocols. Each node runs one or more flows, where a flow consists of a single input fanning out to multiple outputs, with support for SMPTE 2022-1 FEC and SMPTE 2022-7 hitless redundancy.

Supports NMOS IS-04 (Discovery & Registration) and IS-05 (Connection Management) for integration with broadcast control systems. Exposes Prometheus metrics for monitoring.

## Supported Protocols

| Protocol | Input | Output | Notes                                          |
|----------|-------|--------|-------------------------------------------------|
| SRT      | Yes   | Yes    | Caller, listener, rendezvous modes; AES encryption; 2022-7 redundancy |
| RTP/UDP  | Yes   | Yes    | Unicast and multicast; SMPTE 2022-1 FEC; DSCP QoS marking |
| RTMP     | No    | Yes    | H.264/AAC only; supports RTMPS (TLS)           |
| HLS      | No    | Yes    | Segment-based ingest; supports HEVC/HDR         |
| WebRTC   | No    | Yes    | WHIP signaling; H.264 video; Opus audio passthrough |

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

2. **Create a config file** with the `auth` section under `server`:
   ```json
   {
     "version": 1,
     "server": {
       "listen_addr": "0.0.0.0",
       "listen_port": 8080,
       "auth": {
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
     },
     "flows": []
   }
   ```

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
   ```json
   {
     "version": 1,
     "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
     "manager": {
       "enabled": true,
       "url": "wss://manager-host:8443/ws/node",
       "registration_token": "<token-from-manager>"
     },
     "flows": []
   }
   ```

4. **Start the node** -- it connects to the manager, authenticates with the registration token, and receives a permanent `node_id` and `node_secret`.

5. **The node appears** in the manager dashboard and can be configured and monitored remotely. Commands from the manager (create flow, delete flow, add/remove output) are executed automatically.

6. **Credentials are saved automatically** to the config file after registration. On subsequent starts, the node reconnects using `node_id` and `node_secret` (the `registration_token` field is cleared). The connection uses exponential backoff (1s to 60s) for automatic reconnection.

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

- [Configuration Reference](docs/CONFIGURATION.md) -- full config file documentation with all fields and examples

## License

See [LICENSE](LICENSE).
