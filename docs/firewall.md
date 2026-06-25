# Firewall patterns for bilbycast-edge

The installer does **not** configure a host firewall, and that is
deliberate. An edge is a media-transport box: most of its inbound ports
are *operator-defined* listener ports (SRT / RTP / RTMP / RTSP / WebRTC),
created and torn down as flows change. A blanket `ufw default deny`
shipped by the installer would silently break media ingest the first time
an operator adds a listener-mode input. There is no single port policy
that is correct for every deployment.

What *is* universal: **all of the edge's control-plane traffic is
outbound.** The edge dials the manager (WSS) and the relay (QUIC); the
manager never connects in. So management and relay tunnels work behind
any firewall or NAT with **zero inbound rules**. Inbound rules are only
ever about the media you choose to receive.

This guide gives you the three deployment archetypes and a port matrix so
you can write the *right* rules for your case — in whatever tool you use
(ufw, nftables, firewalld, or a cloud security group). To generate the
exact list for a running node, see
[Listing the ports a config needs](#listing-the-ports-a-config-needs).

## The three archetypes

### 1. Caller / push everywhere — zero inbound (most secure)

Configure every input in **caller/connect** mode (SRT caller, RTSP pull,
WebRTC WHEP client, RTMP push *to* a remote, HLS/CMAF push) and every
output as a push. The edge then only ever *originates* connections.

- **Inbound:** none.
- **Outbound:** 443/8443 TCP to the manager, 4433 UDP to the relay, plus
  whatever your sources/destinations listen on.
- Firewall: `default deny incoming`, `default allow outgoing`. Done.

This is the recommended shape for edges on the public internet or in
hostile networks. Prefer it whenever the far end can listen.

### 2. Listener-mode inputs — open only the ports you bind

When a source can only push to you (a camera/encoder doing SRT listener
→ you caller is not always possible), the edge binds a listener port the
source must reach. Open **exactly those ports**, nothing wider.

- **Inbound:** one rule per listener input (the `bind_addr`/port on that
  input). Scope the rule to the source IP/CIDR where you can.
- Everything else stays default-deny.

Don't open a broad range "to be safe" — each open port is attack surface.
Open the specific ports your config actually binds (see the matrix and the
config-listing command below).

### 3. Relay tunnels — inbound lives on the relay, not the edge

With `mode: relay`, two edges meet through a stateless relay. Both edges
dial the relay outbound over QUIC; neither needs an inbound media rule.
The public inbound surface is the relay's, not the edge's.

- **Edge inbound:** none.
- **Edge outbound:** 4433 UDP to the relay (plus manager WSS).
- Use this when both ends are behind NAT. (`mode: direct` instead requires
  *one* side to be reachable — that side needs an inbound rule for
  `direct_listen_addr`.)

## Port matrix

Ports marked *operator-configured* are whatever you set on the
input/output/server config — the values below are the common defaults.

| Direction | Port (default) | Proto | Purpose | When needed |
|-----------|----------------|-------|---------|-------------|
| Outbound | 443 / 8443 | TCP | Edge → manager (WSS) | Always (control plane) |
| Outbound | 4433 | UDP (QUIC) | Edge → relay tunnel | Only with relay/direct tunnels |
| Inbound | *operator-configured* | UDP | SRT **listener** input | Only for SRT listener-mode inputs |
| Inbound | *operator-configured* | UDP | RTP / UDP receiver input | Only for RTP/UDP inputs |
| Inbound | 1935 (operator-configured) | TCP | RTMP ingest | Only for RTMP listener inputs |
| Inbound | 554 (operator-configured) | TCP | RTSP server | Only for RTSP server-mode inputs |
| Inbound | *operator-configured* | TCP+UDP | WebRTC WHIP/WHEP server (signaling + ICE/media) | Only for WebRTC server-mode inputs |
| Inbound | *operator-configured* | TCP | HLS / CMAF pull (edge serves) | Only if clients pull from the edge |
| Inbound | *operator-configured* | UDP (multicast) | ST 2110-20/-23/-30/-31/-40 receive | Only for ST 2110 inputs |
| In/out | 319, 320 | UDP | PTP (IEEE 1588) | Only for ST 2110 / PTP-disciplined setups |
| In/out | 5353 | UDP (mcast) | NMOS / mDNS-SD discovery | Only for NMOS environments |
| Inbound | 8080 (operator-configured) | TCP | Edge local HTTP API + setup wizard | Optional, on-site access only — see note |

> **Local HTTP API (8080).** This is the edge's own REST API + setup
> wizard. It is **not** required for manager control (that's the outbound
> WSS). It binds **loopback only by default** and ships with auth
> disabled, so out of the box it isn't reachable off-box at all. To use
> it from another machine, expose it on a trusted management segment via
> `server.listen_addrs` (or `--bind-addrs 0.0.0.0,[::]`) and turn on
> auth — see [`api-security.md`](api-security.md) for OAuth2/JWT — or
> leave it loopback and reach it over an SSH tunnel / port-forward.

## Listing the ports a config needs

Rather than guessing, derive the inbound ports from the node's own
config. Each listener-mode input carries its bind port; outputs in push
mode need outbound rules to their destination. Until a built-in helper
ships, this `jq` one-liner over `config.json` surfaces the bound ports:

```bash
jq -r '
  .inputs[]
  | select(.mode == "listener" or .type == "rtp" or .type == "udp"
           or .type == "rtmp" or .type == "rtsp")
  | "\(.type)\t\(.mode // "-")\t\(.bind_addr // "0.0.0.0"):\(.port // .bind_port // "?")"
' config.json
```

Open an inbound rule for each line, scoped to the source CIDR where
possible. (The exact field names vary by input type — treat the output as
a checklist, not gospel, and confirm against
[`CONFIGURATION.md`](CONFIGURATION.md).)

## See also

- [`trust-boundary-compliance.md`](trust-boundary-compliance.md) — host
  firewall is an operator responsibility (control C2).
- [`api-security.md`](api-security.md) — locking down the local HTTP API.
- Root [`GETTING_STARTED.md`](../../GETTING_STARTED.md) — venue
  firewall checklist.
