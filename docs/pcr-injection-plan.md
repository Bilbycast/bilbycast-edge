# Regular-interval PCR injection in `ts_video_replace`

> **Status:** plan, not yet implemented. Companion to `engine::wire_emit`
> (committed 0ba7d23). Wire pacing alone cannot fix this; the bitstream
> PCR placement has to change first.

## Bug

`engine::ts_video_replace::packetize_ts` places exactly one PCR per
PES, on the PUSI start packet (line 1186 of `ts_video_replace.rs`).
One PES = one encoded frame. Frame sizes vary by 5–15× under CBR with
VBV (I ≈ 60 KB, P ≈ 8 KB, B ≈ 4 KB at 1080p). PCR _value_ spacing is
uniform (PTS step × 300 − preroll); PCR _byte_ spacing equals the
emitted frame size. At wire-time these decouple: with a 6 Mbps pipe,
60 KB takes 80 ms and 4 KB takes 5 ms while the PCR values both claim
40 ms.

`tsp pcrverify` measures `|Δpcr_value − Δbyte/bitrate|` and reports
the full variance. Live capture on edge1 (h264_qsv 1080p CBR + aac_lc):
**0 PCR OK, 197 with jitter > 100 µs** — and the same numbers from
offline pcrverify on a captured TS file (no network involved),
confirming the bitstream is at fault, not the wire path.

The bug is universal across every encoder backend (x264, x265, NVENC,
QSV, VAAPI, software). Every backend produces variable frame sizes
under rate control; every backend feeds the same `packetize_ts`.

## Fix shape

Inject **PCR-only TS packets** (adaptation field with `PCR_flag = 1`,
no PES payload — full 184-byte AF, mostly stuffing) at regular byte
intervals targeting ~25–30 ms wallclock cadence at the declared
bitrate. PCR value at each insertion advances linearly with byte
offset:

```text
target_byte_interval = declared_bitrate_bps × 30e-3 / 8
pcr_value_at_offset = pcr_anchor + byte_offset × 8 × 27e6 / declared_bitrate_bps
```

This decouples PCR cadence from frame-size variance. Combined with
`engine::wire_emit` (committed) the receiver sees uniform PCR cadence
regardless of which backend transcoded.

## Module map

| File | Change |
|------|--------|
| `engine/ts_video_replace.rs` | New `PcrInjector` struct + integrate into the PES-emit loop. Stop placing PCR on PES start (or keep it for the *first* PES of a stream as the anchor). |
| `engine/ts_video_replace.rs` | New helper `build_pcr_only_ts_packet(pid, cc, pcr_27mhz) -> [u8; 188]`. AF length = 183, PCR field at offset 6..12, fill 8..184 with 0xFF. CC must continue the PID's running counter. |
| `engine/ts_audio_replace.rs` | If audio output is on a different PID and PCR_PID points to video PID (current behaviour), audio is unaffected. If we ever route PCR to a dedicated PID, audio replacer needs the same injection. Confirm scope. |
| `config/models.rs` (optional) | Per-output `pcr_interval_ms: Option<u8>` (10..=40, default 25). Off-by-default behind `transcode_pcr_at_byte_interval: bool` if we want a feature flag. |

## State to add to `TsVideoReplacer`

```rust
struct PcrInjector {
    pcr_pid: u16,
    declared_bitrate_bps: u64,
    target_interval_bytes: u64,   // recomputed when bitrate changes
    pcr_pid_cc: u8,               // continuity counter for PCR-only packets
    bytes_since_last_pcr: u64,    // cumulative byte offset on the PCR PID
    pcr_anchor_27mhz: u64,        // value of the most recently emitted PCR
}
```

Increment `bytes_since_last_pcr` by 188 for every TS packet emitted on
`pcr_pid`. When it crosses `target_interval_bytes`, emit a PCR-only
packet, advance `pcr_anchor_27mhz` by `bytes_since_last_pcr × 8 × 27e6 / declared_bitrate_bps`,
reset `bytes_since_last_pcr` to 0.

## Discontinuity

On flow switch (forwarder signals `force_idr`): reset `pcr_anchor_27mhz` to
the new flow's first source PTS-derived PCR, reset
`bytes_since_last_pcr` to 0. Same hooks already exist for the encoder.

## CC continuity

PCR-only packets share the video PID's CC counter (since PCR_PID =
video PID in our muxer). `packetize_ts` mutates `cc` per packet; the
injector must increment it whenever it emits a PCR-only packet so the
receiver's CC check (TR-101290 P1.3) doesn't fire.

## Tests

Unit-level (in `ts_video_replace.rs::tests`):

1. **PCR cadence is uniform** — feed N frames of varying sizes through
   the muxer with declared 6 Mbps; assert every consecutive PCR-bearing
   packet pair satisfies `|Δpcr_value − Δbyte × 8 × 27e6 / 6e6| < 1ms`.
2. **CC counter monotonic across PES + PCR-only packets** — PIDFilter
   walks the resulting bytes, asserts CC step = 1 between any two
   consecutive packets on the video PID.
3. **PCR-only packet structure valid** — single packet contains
   AF_length=183, AF_flags=0x10 (PCR_flag), 6 PCR bytes, 176 bytes of
   0xFF stuffing, payload bit clear.
4. **Discontinuity reset** — feed a flow-switch boundary; assert PCR
   anchor resets and the next emission lands at the new anchor.
5. **No bitrate hint** — when `declared_bitrate_bps == 0`, fall back
   to PES-tied PCR (the legacy path), tested for byte-equivalence
   against the current behaviour so existing regression tests stay
   green.

## Live validation

Same as wire_emit's plan:

```bash
# Hot-swap appear-tap to encoded mirror via REST.
# Then:
tsp -I ip 6010 --reuse-port --local-address 127.0.0.1 -P pcrverify --jitter-max 100 -O drop
# Expect: ≥ 95 % PCRs ≤ 100 µs (target).
# Capture too:
tsp -I ip 6010 --reuse-port --local-address 127.0.0.1 -O file /tmp/encoded.ts &
sleep 8 && kill $!
tsp -I file /tmp/encoded.ts -P pcrverify --jitter-max 100 -O drop
# Expect the offline result to match — both should now pass.
```

## Once the bitstream PCRs are uniform, wire_emit ships next

Re-integrate `engine::wire_emit` into `output_udp.rs` and
`output_rtp.rs` (the integration was reverted in 0ba7d23 because it
exposed the bitstream PCR bug). Validate end-to-end:

- Passthrough: 0 jitter > 1 ms (unchanged).
- Encoded: ≥ 95 % PCRs ≤ 100 µs (the original tier-1 target).
- 0 try_send drops on the encoder ↔ wire mpsc.
