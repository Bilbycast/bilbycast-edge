# SMPTE RP 2129 Trust Boundary Compliance

This document describes how bilbycast-edge implements the requirements from
SMPTE RP 2129 (Inter Entity Trust Boundary, 2022). Each requirement is listed
with its current status and a brief description of the implementation.

---

## Status Legend

| Symbol | Meaning |
|--------|---------|
| **met** | Fully implemented and functional |
| **partial** | Some aspects implemented; gaps noted |
| **missing** | Not implemented |

---

## Core Requirements

| ID | Requirement | Status | Implementation |
|----|-------------|--------|----------------|
| C1 | Trusted / Untrusted interface zones | missing | The system does not model interface trust zones. Flows are configured per-endpoint; zone separation must be enforced by network topology. |
| C2 | Deny-all on untrusted ingress | missing | No OS-level firewall integration. Operators should pair bilbycast-edge with host firewall rules (iptables / nftables / pf). |
| C3 | Deny-all on trusted egress | missing | Same as C2. |
| C4 | UDP-only traffic | **met** | All transports are UDP-based: raw RTP/UDP and SRT (which runs over UDP). No TCP listeners are exposed on the media path. |
| C5 | L3/L4 source IP filtering | **met** | `allowed_sources` on `RtpInputConfig` accepts a list of IP addresses. Packets from unlisted sources are dropped before entering the broadcast channel. The allow-list is pre-parsed into a `HashSet<IpAddr>` for O(1) per-packet lookup. Dropped packets are counted in the `packets_filtered` stat. |
| C6 | Per-flow NAT (address rewriting) | missing | The system forwards packets byte-for-byte; no SNAT/DNAT capability. |
| C7 | Per-flow ingress rate limiting | **met** | `max_bitrate_mbps` on `RtpInputConfig` enables a token-bucket rate limiter. Excess packets are dropped before the broadcast channel. The token bucket uses integer arithmetic with a 10 ms burst allowance. Dropped packets are counted in `packets_filtered`. |
| C8 | RTP header preservation | **met** | RTP packets are forwarded as raw `Bytes` from input to output. The header is parsed read-only for sequence number and timestamp extraction but is never modified. Output tasks call `socket.send_to(&packet.data, ...)` on the original bytes. |
| C9 | IGMP/MLD multicast joins | **met** | `bind_udp_input` in `src/util/socket.rs` detects multicast destinations and calls `join_multicast_v4` (IGMP) or `join_multicast_v6` (MLD). The optional `interface_addr` config field controls which network interface the join is performed on. |
| C10 | QoS / DSCP marking on egress | **met** | `dscp` on `RtpOutputConfig` (default: 46, Expedited Forwarding per RFC 4594). The DSCP value is applied once at socket creation via `set_tos_v4` (IPv4) or `set_tclass_v6` (IPv6). All packets sent on that socket inherit the marking automatically with zero per-packet overhead. |

---

## Use Case 1: UDP/RTP Flows

| ID | Requirement | Status | Implementation |
|----|-------------|--------|----------------|
| U1 | SMPTE standard RTP payloads (ST 2022-2, ST 2022-6, ST 2110-20/-30) | **met** | The forwarding path is payload-agnostic. Any valid RTP packet (version 2) is accepted and forwarded regardless of payload type. |
| U2 | Per-flow monitoring: seq gaps, IAT, PDV, bitrate | **met** | Sequence gaps tracked in the input loop (`packets_lost`). Bitrate computed via `ThroughputEstimator`. IAT (min/max/avg) and PDV jitter (RFC 3550) computed on the TR-101290 analyzer task, an independent broadcast subscriber that cannot affect the media path. All metrics exposed via REST API, WebSocket, Prometheus, and dashboard. |
| U3 | DPP Live IP flow profiles | missing | No DPP-specific profile validation. |
| U4 | RTP payload type filtering | **met** | `allowed_payload_types` on `RtpInputConfig` accepts a list of PT values (0-127). Packets with non-matching PTs are dropped. The check is a single byte comparison (`data[1] & 0x7F`) performed after the `is_likely_rtp` validation. |
| U5 | ST 2110 essence timing adjustment | missing | Would require per-packet timestamp manipulation on the hot path. |
| U6 | ST 2110 packet pacing | missing | Would require per-packet timing manipulation on the hot path. |
| U7 | ST 2022-7 flow duplication | **met** | SRT outputs support dual-leg duplication via the `redundancy` config. Each packet is sent to both SRT legs independently using non-blocking `try_send`. Implementation in `src/engine/output_srt.rs`. |
| U8 | ST 2022-7 hitless merge | **met** | SRT inputs support dual-leg merge via the `redundancy` config. `HitlessMerger` in `src/redundancy/merger.rs` performs sequence-based deduplication per SMPTE 2022-7. Redundancy leg switches are tracked in stats. |
| U9 | Alarm-based flow protection switching | partial | `FlowHealth` enum (Healthy / Warning / Error / Critical) is derived from monitoring metrics during the 1/sec stats snapshot. Automatic protection switching is not yet implemented. |
| U10 | Payload standard translation | missing | Would require deep packet manipulation on the hot path. |

---

## Use Case 2: FEC Flows

| ID | Requirement | Status | Implementation |
|----|-------------|--------|----------------|
| F1 | SMPTE 2022-1 FEC support | **met** | `fec_decode` on `RtpInputConfig` enables column/row FEC packet recovery. `fec_encode` on `RtpOutputConfig` generates FEC repair packets. Parameters: `columns` (L, 1-20) and `rows` (D, 4-20). Recovered packet counts are tracked in `packets_recovered_fec`. Implementation in `src/fec/`. |

---

## Use Case 3: ARQ/NACK Flows

| ID | Requirement | Status | Implementation |
|----|-------------|--------|----------------|
| A1 | ARQ (NACK) packet recovery | **met** | SRT provides ARQ-based retransmission natively. SRT stats expose `pkt_retransmit_total`. |
| A2 | Hybrid FEC + ARQ | partial | FEC is available on RTP inputs/outputs; ARQ is available on SRT inputs/outputs. Using both simultaneously on the same flow (e.g., RTP input with FEC feeding an SRT output with ARQ) is supported. |
| A3 | Authenticated and encrypted flows | **met** | SRT inputs and outputs support AES-128/192/256 encryption via `passphrase` and `aes_key_len` config fields. Validation enforces passphrase length (10-79 chars) and key length (16/24/32). |

---

## Monitoring Requirements

| ID | Metric | Status | Implementation |
|----|--------|--------|----------------|
| M1 | RTP sequence gaps | **met** | `packets_lost` in `InputStats`. Computed via wrapping arithmetic on 16-bit sequence numbers in the input loop. Gaps > 1000 are ignored (source restart heuristic). |
| M2 | RTP inter-arrival time (IAT) | **met** | `iat` in `FlowStats` provides `min_us`, `max_us`, `avg_us` over a running window. Computed on the TR-101290 analyzer task using `recv_time_us` deltas between consecutive packets. |
| M3 | Packet delivery variation (PDV) | **met** | `pdv_jitter_us` in `FlowStats`. Implements the RFC 3550 exponential moving average: `J = J + (\|D\| - J) / 16`. Computed on the TR-101290 analyzer task. |
| M4 | Packet/bit rate | **met** | `bitrate_bps` per flow and per output. Computed by `ThroughputEstimator` using 1-second sampling intervals with caching. |
| M5 | Packets dropped by filters | **met** | `packets_filtered` in `InputStats`. Counts packets dropped by source IP filter, payload type filter, and rate limiter combined. Per-output drops tracked in `packets_dropped`. |
| M6 | Flow health/alarm state | **met** | `health` in `FlowStats` as `FlowHealth` enum (Healthy/Warning/Error/Critical). Derived during the 1/sec stats snapshot from bitrate, packet loss, and TR-101290 priority 1/2 status. Displayed as a color-coded badge on the dashboard. |

---

## Network Topology Requirements

| ID | Requirement | Status | Notes |
|----|-------------|--------|-------|
| N1 | Private IP ranges on inter-entity links | n/a | Network design concern, not enforced by software. |
| N2 | UDP ports > 1024 | n/a | Config accepts any port; operators should follow this guideline. |
| N3 | NAT for address isolation | missing | No NAT capability in the system. |
| N4 | SSM (Source-Specific Multicast) | partial | Multicast joins are (*,G) style. SSM (S,G) filtering is not yet supported. The `allowed_sources` filter provides equivalent protection at the application layer. |
| N5 | Point-to-point over routed L3 | n/a | Network design concern. |
| N6 | Dual links for redundancy only | n/a | Network design concern. SMPTE 2022-7 support ensures redundant paths are used for protection, not load-sharing. |

---

## Summary

| Section | Met | Partial | Missing | Total |
|---------|-----|---------|---------|-------|
| Core (C1-C10) | 6 | 0 | 4 | 10 |
| Use Case 1 (U1-U10) | 5 | 1 | 4 | 10 |
| Use Case 2 (F1) | 1 | 0 | 0 | 1 |
| Use Case 3 (A1-A3) | 2 | 1 | 0 | 3 |
| Monitoring (M1-M6) | 6 | 0 | 0 | 6 |
| Network (N1-N6) | 0 | 1 | 1 | 6 |
| **Total** | **20** | **3** | **9** | **36** |

All monitoring requirements (M1-M6) are fully met. The missing items are either
architectural concerns (interface zones, deny-all firewall) that require OS-level
enforcement, or features that would require per-packet manipulation on the media
hot path (NAT, timing adjustment, pacing, payload translation) which were excluded
to preserve QoS.

---

## Configuration Examples

### Source IP filtering + rate limiting + DSCP

```json
{
  "input": {
    "type": "rtp",
    "bind_addr": "239.1.1.1:5000",
    "interface_addr": "10.0.1.100",
    "allowed_sources": ["10.0.2.50", "10.0.2.51"],
    "allowed_payload_types": [33, 96],
    "max_bitrate_mbps": 50.0
  },
  "outputs": [
    {
      "type": "rtp",
      "id": "out-1",
      "name": "Trusted Output",
      "dest_addr": "192.168.1.100:5004",
      "dscp": 46
    }
  ]
}
```

### Encrypted SRT with 2022-7 redundancy

```json
{
  "input": {
    "type": "srt",
    "mode": "listener",
    "local_addr": "0.0.0.0:9000",
    "latency_ms": 120,
    "passphrase": "my-secure-passphrase",
    "aes_key_len": 32,
    "redundancy": {
      "mode": "listener",
      "local_addr": "0.0.0.0:9001",
      "latency_ms": 120,
      "passphrase": "my-secure-passphrase",
      "aes_key_len": 32
    }
  }
}
```

### Monitoring API response (excerpt)

```json
{
  "health": "Healthy",
  "iat": { "min_us": 890.0, "max_us": 1520.0, "avg_us": 1105.0 },
  "pdv_jitter_us": 42.3,
  "input": {
    "packets_received": 50000,
    "packets_lost": 0,
    "packets_filtered": 12,
    "bitrate_bps": 25000000
  },
  "tr101290": {
    "priority1_ok": true,
    "priority2_ok": true,
    "ts_packets_analyzed": 350000
  }
}
```
