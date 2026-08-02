# Bonded relay legs — loopback-free in-process bridge

**Status: IMPLEMENTED.** The loopback-free in-process bonded-relay leg bridge is
shipped in both the bonding library and the edge. The bond exposes a
`BondPathTransportConfig::Relay` path variant; `bonding-transport` carries the
`PathTransport::Attached` config, the `AttachedPath`, and
`BondSocket::sender_attached` / `receiver_attached`; and the edge builds the leg
in-process via `run_native_relay_leg_inproc` / `AttachedChannels` in
`input_bonded.rs` / `output_bonded.rs` — no loopback UDP round-trip and no
second AEAD pass. The sections below describe the design that motivated it.

## Why

A bonded leg over relay is two objects joined only by a loopback port:

1. a bond UDP path `Udp { remote: 127.0.0.1:PORT }` on the bonded I/O, and
2. a separate native-UDP relay tunnel (`transport=udp, mode=relay`,
   `local_addr=127.0.0.1:PORT`) spawned by `TunnelManager`.

Every datagram therefore makes a real loopback UDP round-trip:

```
bond send_on_path → socket.send_to(127.0.0.1:PORT) → [kernel loopback]
  → udp_forwarder::run_egress recv_from(PORT) → tunnel AEAD encrypt
  → encode_udp_datagram (+16B tunnel_id) → relay socket
```

Costs on a high-bitrate / PCR-jitter-sensitive Tier-1 path: +2 syscalls/box,
a loopback round-trip, ~3 allocs/memcpy, and — for an encrypted bonded leg —
a **second** ChaCha20 pass on top of the bond's own `0xBD` AEAD (the bond
already framed + optionally encrypted the datagram).

The 16-byte `tunnel_id` prefix is **kept** so the relay needs **zero** changes
(it still demuxes by prefix and forwards verbatim). Only the edge changes:
the bond leg reads/writes the relay socket directly, in-process, no loopback.

SRT/RIST keep their loopback path **byte-for-byte** (libsrt/RIST own their UDP
sockets, so we can't inject an in-process sink — see the conversation rationale).

## Seam (code-grounded, from the datapath map)

### bilbycast-bonding / bonding-transport
- `src/config.rs:161` — add `PathTransport::Attached { primary_peer: SocketAddr }`.
  Channels travel in a **separate, non-Clone attachments argument** (the enum is
  `Clone+Debug`, so mpsc endpoints can't live in `PathConfig`).
- `src/path/attached.rs` (new) + `src/path/mod.rs:76-240` — add `Path::Attached`
  to every match arm (`id`/`name`/`send_to`/`send`/`wire_overhead`/`take_rx`/
  `primary_peer`). `AttachedPath::send_to` **ignores the `to` arg**, seals `0xBD`
  when a `BondCrypto` is set, then `outbound.try_send` (drop-on-full, NOT await);
  `take_rx` returns the inbound `PathDatagram` receiver; `primary_peer` returns
  `Some(synthetic)` (else `sender.rs:700-703` silently skips the leg's keepalive
  and drops it from aggregation).
- `src/socket.rs:280,487` — add `sender_attached` / `receiver_attached` + an
  attachments map keyed by `PathId`; `build_one_path` gets an `Attached` arm
  pulling the leg channels + the shared `BondCrypto`.

### bilbycast-edge
- `src/config/models.rs:4427` — add `BondPathTransportConfig::Relay { tunnel_id,
  relay_addrs, tunnel_bind_secret?, tunnel_encryption_key?, interface?, source?,
  gateway? }` (the leg→tunnel FK on the leg). Keep `Udp/Rist/Quic` for back-compat.
- `src/config/validation.rs` — **fail-closed**: a relay leg must carry exactly one
  encryption layer (bond `encryption_key` **or** tunnel `tunnel_encryption_key`);
  reject neither (blackhole) and reject both (double-encrypt). gateway ⇒ source.
- `src/tunnel/udp_relay_client.rs:217` — refactor `run_native_relay_tunnel` to share
  the relay-socket lifecycle (connect, Register/keepalive `:303-349`, failover);
  add `run_native_relay_leg_inproc(params, cancel, stats, cipher, to_bond, from_bond)`
  that bridges over the same `PlainUdpLink`: egress drains `from_bond`, conditional
  encrypt, `encode_udp_datagram`, `send_datagram`; ingress `recv_datagram`, decode,
  conditional decrypt, `to_bond.try_send`. (Factor the conditional-AEAD + prefix
  encode/decode out of `udp_forwarder::run_egress/run_ingress` into shared helpers
  — emit **byte-identical** frames so the relay is unchanged.)
- `src/engine/output_bonded.rs` / `input_bonded.rs` — for a `Relay` path: create the
  two channels, pick the conditional cipher (bond key set ⇒ `BondCrypto` seals and
  the bridge cipher is `None`; else the `TunnelCipher` is the single layer), spawn
  the bridge, build via `sender_attached`/`receiver_attached`; map `Relay → Attached`
  in `translate_transport_for_{sender,receiver}`. Keep feeding
  `UdpForwarderStats.decrypt_errors` so `decrypt_skew_watchdog` still alarms.
- `src/tunnel/manager.rs` — **no change** to SRT/RIST/loopback; a bonded `Relay`
  leg never creates a `TunnelManager` loopback tunnel (the bonded modules own the
  relay socket).

## Hard rules / failure modes (verify each)
- mpsc `try_send` drop-on-full **both directions** — never `send().await` (an
  awaiting channel injects backpressure UDP never had → stalls the bond sender).
- conditional-AEAD decision MUST be identical on output + input legs (the manager
  FK stamps both ends together) — a mismatch is a total blackout; surface via
  `decrypt_skew_watchdog`.
- `primary_peer` must be `Some(synthetic)` and `send_to` must ignore `to`, or the
  leg is silently dropped + the NACK/keepalive back-channel breaks.
- relay rotation: the bridge re-Registers on a new socket but the bond leg channels
  **persist** (leg not torn down).
- update `AttachedPath.wire_overhead` (drop the redundant 28B AEAD when bond-keyed)
  and re-check the MTU guard for legs near ~1400 B.
- back-compat: keep the legacy loopback `Udp` bonded leg until the manager FK
  stamps `Relay` on both ends.

## Verification (mandatory before merge)
Run the 12 broadcast quality gates on a single then multi-leg bonded-over-relay
Tier-1 flow on the testbed (`testbed/BROADCAST_QUALITY_GATES.md`): wallclock rate,
decode round-trip, A/V sync, PCR_AC, sample-level content, plus confirm
SRT/RIST-over-relay still locks (untouched loopback path) and the relay binary is
byte-identical.

## Related deferred follow-ups
- **SRT/RIST forwarder jitter**: dedicated SCHED_FIFO/CPU-pinned thread + zero-alloc
  in-place wrap for `udp_forwarder::run_egress/run_ingress` (the 32 MB socket-buffer
  fix already landed). Needs live jitter measurement to validate.
- **Manager — relay-routing editor on the input/output config modal**: the per-card
  read-only "via relay · N legs" E2E indicator already landed (`config/outputs.js`);
  the *editor* (flip a media output direct↔relay from its own modal, re-running the
  wizard plan) is the remaining piece.
