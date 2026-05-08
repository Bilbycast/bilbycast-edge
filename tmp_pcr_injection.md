# PCR injection plan (working notes — not customer-facing)

> Scratch file. Companion to `engine::wire_emit` (committed in 0ba7d23,
> integration deferred). The wire-side pacing module is sound; the
> end-to-end tier-1 jitter target is blocked by the bitstream-side bug
> documented below.

## The bug we found live

`engine::ts_video_replace::packetize_ts` (in
`src/engine/ts_video_replace.rs`) places exactly one PCR per PES, on the
PUSI start packet (look for `if is_first && pcr_27mhz.is_some()`).
One PES = one encoded frame.

- PCR _value_ spacing is uniform: `pts × 300 − preroll`, source PTS is uniform.
- PCR _byte_ spacing equals the frame size in bytes.
- Frame sizes vary 5–15× under CBR with VBV (I ≈ 60 KB, P ≈ 8 KB, B ≈ 4 KB at 1080p).
- At wire-time `Δwall = Δbyte / declared_bitrate` decouples from `Δpcr_value`.

`tsp pcrverify` reports `0 PCR OK / 197 jitter > 100 µs` on a captured
h264_qsv 1080p CBR TS file — same numbers offline as on the network,
proving the bytes are at fault.

**Universal across backends.** x264, x265, NVENC, QSV, VAAPI all feed
the same `packetize_ts`. Switching backend doesn't help — only changes
picture quality and CPU cost. The fix lives in the muxer.

## Fix

Inject **PCR-only TS packets** (adaptation field with `PCR_flag = 1`,
no PES payload — full 184-byte AF, mostly stuffing) at regular byte
intervals targeting ~25–30 ms wallclock cadence at the declared bitrate.
PCR value advances linearly with byte offset:

```text
target_byte_interval = declared_bitrate_bps × 30e-3 / 8
pcr_value_at_offset  = pcr_anchor + byte_offset × 8 × 27e6 / declared_bitrate_bps
```

This decouples PCR cadence from frame-size variance.

## State to add to `TsVideoReplacer`

```rust
struct PcrInjector {
    pcr_pid: u16,
    declared_bitrate_bps: u64,
    target_interval_bytes: u64,
    pcr_pid_cc: u8,
    bytes_since_last_pcr: u64,
    pcr_anchor_27mhz: u64,
}
```

New helper alongside `packetize_ts`:

```rust
fn build_pcr_only_ts_packet(pid: u16, cc: u8, pcr_27mhz: u64) -> [u8; 188]
```

- AF length = 183, AF flags = 0x10 (PCR_flag), PCR field at offset 6..12,
  bytes 12..188 = 0xFF stuffing, payload bit clear in TS header.
- Continuity counter shares the video PID's running counter (since
  `PCR_PID = video_pid` in our muxer). Increment `cc` whenever we emit
  a PCR-only packet so receiver TR-101290 P1.3 doesn't fire.

## Loop sketch

After every TS packet emitted on `pcr_pid`, increment
`bytes_since_last_pcr` by 188. When it crosses `target_interval_bytes`,
emit a PCR-only packet, advance the anchor:

```text
pcr_anchor_27mhz += bytes_since_last_pcr × 8 × 27e6 / declared_bitrate_bps
bytes_since_last_pcr = 0
```

Discontinuity (flow switch via `force_idr`): reset
`pcr_anchor_27mhz` to the new flow's first PTS-derived PCR; reset
`bytes_since_last_pcr` to 0.

## Tests (`ts_video_replace.rs::tests`)

| # | Scenario | Assertion |
|---|----------|-----------|
| 1 | Uniform cadence | feed N frames of varying sizes at declared 6 Mbps; every consecutive PCR pair satisfies `|Δpcr − Δbyte × 8 × 27e6 / 6e6| < 1 ms` |
| 2 | CC monotonic | walk video-PID bytes, CC step = 1 between any two consecutive packets, including PES + PCR-only mix |
| 3 | PCR-only structure | single packet has `AF_length=183`, `AF_flags=0x10`, 6 PCR bytes, 176 bytes of 0xFF stuffing, payload bit clear |
| 4 | Discontinuity | feed flow-switch boundary; PCR anchor resets cleanly |
| 5 | No bitrate hint | `declared_bitrate_bps == 0` → fall back to legacy PES-tied PCR; byte-equivalence with current code so regression tests stay green |

## Live validation

```bash
# Hot-swap appear-tap to encoded mirror via REST PUT; then:
tsp -I ip 6010 --reuse-port --local-address 127.0.0.1 -P pcrverify --jitter-max 100 -O drop
# Target: ≥ 95 % PCRs ≤ 100 µs.

# Capture and offline-verify (must match online numbers):
tsp -I ip 6010 --reuse-port --local-address 127.0.0.1 -O file /tmp/encoded.ts &
sleep 8 && kill $!
tsp -I file /tmp/encoded.ts -P pcrverify --jitter-max 100 -O drop
```

## Once the bitstream is uniform

Re-integrate `engine::wire_emit` into `output_udp.rs` and
`output_rtp.rs` (the integration was reverted in 0ba7d23 because it
exposed this bitstream bug). Module + 12 unit tests already in tree.
Channel cap must be ≥ 1024 to absorb encoder bursts.
