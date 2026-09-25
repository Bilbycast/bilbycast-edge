# Clocking and A/V sync

Every flow on bilbycast-edge runs against a per-flow **master clock**.
PCR generation, output emission timing, and lipsync trim all bottom out
on the same `MasterClock::now_27mhz()` call. Source-PCR PLL recovers the
upstream 27 MHz; PTP slaves the local realtime clock to a grandmaster;
Wallclock is the degraded fallback.

## Why a master clock

Before this work landed, every output stage owned its own emission
timing. Output PCR was derived from PES PTS (`pts × 300 − preroll`),
which means PCR jitter mirrored the encoder pipeline depth. On a
transcoded SRT→RTP flow we measured **30–50 ms** of residual A/V drift
even after fixing every other PCR / PTS bug along the way.

A single per-flow clock fixes this:

- **PCR is anchored on the master clock, not sampled from it per
  packet.** In muxer mode (`engine::ts_pts_rewriter`) the master clock
  places the first PCR at `master_now − preroll`; every later PCR is
  `anchor + (src_pcr − anchor_src)`, free-running on the *source*
  clock. On a transcoded path the output PCR is the **input's own PCR
  timeline delayed by one measured transcode allowance**
  (`engine::ts_pcr_remux` — see
  [Transcoded output PCR](#transcoded-output-pcr-the-remux-model)). Two
  earlier designs are gone: one sampled the master inside the encoder
  pipeline while the packet's wire time was set by send pacing
  (professional decoders flagged it as PCR jitter, measured stdev
  176 ms); the next derived PCR from the re-encoded video PTS
  (`pts × 300 − 80 ms`, floored on a decaying audio lag), a clock that
  ran ~15 500 ppm fast against the PTS it described.
  `AvSyncPacer::pcr_27mhz_for_emit`, the only function that samples the
  master per PCR, is `#[allow(dead_code)]` and has no production
  caller. Both generators are deterministic in the input bytes, so
  multiple outputs of the same flow still emit identical PCR sequences —
  until one of them has to raise its transcode delay.
- **PTS still flows from the source** via `src_pts_queue` in the audio
  + video replacers, so A/V offset versus source is preserved.
- **Cross-edge coherence is not free**, and sharing a source PCR or a
  PTP grandmaster does not buy it. In muxer mode the anchor is stamped
  when the *first PCR arrives at this node*, so emitted PCR carries that
  node's own ingest latency as a fixed additive term: two edges fed the
  same feed over paths 120 ms apart emit PCRs 120 ms apart — measured,
  with both nodes on one master clock. Within a single flow every output
  is coherent regardless of pipeline depth, which is what 2022-7 dual-leg
  needs; across two nodes it takes `passthrough_clock: true` (or a
  `bonded` input) plus an alignment group — see
  [Cross-node egress alignment (`epoch_lock`)](#cross-node-egress-alignment-epoch_lock).

**The default policy is now `auto` (by flow role).** A flow with no explicit
`master_clock` resolves identically to `master_clock.kind = "auto"`. The
resolution is keyed on **flow role** (in `build_master_clock`), which
overrides the per-input default table:

| Flow role | `auto` resolves to | Notes |
|-----------|--------------------|-------|
| ST 2110-20/-23/-30/-31/-40, MXL | `Ptp` | PTP-domain essence; `ptp4l` `port_state == SLAVE`/`MASTER`, offset within tolerance |
| Single-source **live contribution**: SRT / RTP / UDP / RIST / RTMP / RTSP | cascade **`SourcePcrPll` → `Ptp` → `Wallclock`** | PLL lock criterion: PI loop converges, p99 jitter < 100 µs over 64 samples after ≥ 100 samples; on failure → PTP-if-configured-&-healthy, else wallclock |
| PID-bus assembled flows | cascade (same as live contribution) | the PLL recovers the designated `pcr_source` |
| **Multi-input switcher** (more than one `input_id`) | `Wallclock` | stable clock keeps cuts seamless; genlocking to the active source would step the clock on every cut and re-acquire lock for seconds |
| File / `media_player` / `replay` / WebRTC / `test_pattern` / `rtp_audio` / `bonded` | `Wallclock` | no live source clock to recover; the source plays at wallclock rate |
| `AudioMaster` | (reserved) | not yet implemented; falls through to Wallclock |

**The explicit `master_clock.kind` values** an operator can pin (snake_case
on the wire, `MasterClockKindConfig` in `src/config/models.rs`):
`source_pcr_pll`, `contribution`, `ptp`, `audio_master`, `wallclock`,
`auto`, plus two more:

- **`passthrough`** — "this flow doesn't need a recovered source clock".
  Output PCR comes from source bytes (passthrough) or source PTS
  (transcoded); the runtime backend is `wallclock` but no PLL spawns and no
  fallback alarm fires. The implicit default for most
  contribution-to-distribution flows when the operator hasn't pinned
  `source_pcr_pll` / `contribution` explicitly.
- **`sender_timestamp`** (SRT / RIST inputs only) — recover rate from the
  SRT/RIST sender's per-packet timestamp instead of the MPEG-TS PCR sampled
  from the bytes. Useful for internet-contribution paths where SRT's
  latency buffer makes PCR-from-bytes look jittery to the PLL but the
  underlying libsrt `srctime` reflects the sender's clock cleanly.
  **Framework only today** — the srctime extraction is wired through in a
  follow-up. Config validation rejects this kind on a non-SRT / non-RIST
  input.

**Why the cascade tries the PLL first then PTP, not wallclock.** The
PLL often won't lock on contribution sources that carry per-source-restart
PCR discontinuities — `ffmpeg -re -stream_loop -1 -c copy` on a 30-second
file, looping playout, SCTE-35 splices, encoder restarts. Once the PLL
has failed, source-tracking is already lost, so the fallback prefers the
node's PTP clock (clean, cross-edge-coherent) over bare wallclock, with
`Wallclock` only as the always-locked floor. The encoder-style PES PTS
regenerators (`engine::ts_pts_rewriter`, `TsAudioReplacer::set_av_sync_pacer`)
anchor against whichever rung is active.

Operators who run on PTP-disciplined or clean-PCR contribution
sources and want cross-edge coherence opt in to the PLL via the new
`master_clock.kind = "contribution"` (preferred — flags intent in
telemetry) or the legacy `master_clock.kind = "source_pcr_pll"`
(retained for back-compat with existing deployments).

Operators can pin a master per-flow with the `master_clock` field in
flow config (overrides the auto-policy). Useful when a plant has PTP
everywhere and the operator wants every flow paced against the
grandmaster regardless of input type.

**Auto by flow role: `master_clock.kind = "auto"`.** A config-level policy
rather than a runtime kind of its own — it picks the right reference for
the flow's role. **ST 2110 / MXL** inputs are PTP-domain essence, so they
resolve straight to **PTP** (no PLL-first). A **single-source live
contribution** flow (SRT/RTP/UDP/RIST/RTMP/RTSP) and a **PID-bus
assembled** flow get the cascade **source PCR PLL → PTP → Wallclock**; a
**multi-input switcher** and **file / replay / WebRTC / test-pattern**
flows resolve straight to **Wallclock** (no genlock to step on cuts / no
live source clock to recover). The cascade
first runs the source-PCR PLL against the selected
input (best — output tracks the source clock, zero source-relative
drift). If the PLL can't lock within `pll_lock_timeout_s` (default 30 s),
the fallback watcher drops to the **PTP** rung when the node has a PTP
role configured (`ptp.conf` mode != off) **and** PTP is healthy (slave
`Locked`, or this node is the grandmaster) — a clean, cross-edge-coherent
reference (genlock-free 2022-7). Otherwise it drops to **wallclock**, the
always-available floor.

The PTP-vs-wallclock choice is **latched** when the PLL gives up, and the
data path only ever **demotes one-way** (PTP → wallclock) if PTP later
loses lock — so the output never paces PCR off an unlocked, undisciplined
`CLOCK_REALTIME`, and never oscillates epochs. The PTP rung polls the
node's `ptp.conf` domain, not the flow's SDP `clock_domain`. If the PLL
re-locks at any point, the master self-heals back to the PLL (best rung).

Telemetry reports `configured_kind = "auto"` with `kind` = the active
rung, so the manager UI renders "Auto → Source PCR PLL" / "Auto → PTP" /
"Auto → Wallclock". This order suits **contribution** workflows: prefer
to track the source, use PTP as a clean catch when the source clock is
unrecoverable, wallclock only as the last resort. Caveats: mid-run PTP
loss demotes to wallclock (a PCR discontinuity, but safe); and the PTP
rung buys cross-edge / absolute coherence, **not** drift versus an
un-genlocked source — for true source-rate tracking the PLL must lock, or
the source must be genlocked.

## Encoder-style PES PTS regeneration

Every TS-carrying ingress runs byte-level PES PTS/DTS regeneration in
muxer mode **by default**; the per-input `passthrough_clock: Option<bool>`
config field set to `true` opts OUT (emits source PCR/PTS bytes
unchanged — relay / transparent-forwarder mode). In muxer mode the
[`engine::ts_pts_rewriter`](../src/engine/ts_pts_rewriter.rs) stage
inside `engine::input_post_process::InputPostProcess` rewrites each
PES header's PTS (and DTS when present) so emitted timestamps come
from the per-flow master clock instead of the source TS bytes.

The model is per-PID **anchor + source-delta**:

```text
On first PES of PID (or on >500 ms source-PTS discontinuity):
    anchor_out_90k = master.now_27mhz()/300 + PCR_PREROLL_90K  (= 7_200, 80 ms)
                     + lipsync_offset_90k (audio PIDs only)
    anchor_src_90k = source PES PTS

On every PES:
    delta_src   = source_pts - anchor_src_90k          (wrapping, 33-bit)
    out_pts     = anchor_out_90k + delta_src
    out_dts     = out_pts - (source_pts - source_dts)  if DTS present
```

This preserves the source's PES inter-arrival timing exactly (no
per-PES master_now jitter injection) while making absolute PTS
values master-clock-derived. DTS preserves the source PTS-DTS delta
so H.264 / HEVC B-frame reorder still decodes correctly.

A **10 s safety check** on the anchor candidate falls back to the
raw source PTS when master and source are wildly uncorrelated
(Wallclock master + small-offset encoder PTS — the common case
today): preserves the existing anchor-to-source behaviour when the
master clock can't help, kicks in only when the two agree to within
10 s (PTP master + PTP-disciplined source, or a locked
`SourcePcrPll` master).

The transcoded path landed the same model in
[`engine::ts_audio_replace::TsAudioReplacer::set_av_sync_pacer`](../src/engine/ts_audio_replace.rs).
### PCR discontinuity bridging (and what the clamp costs)

`engine::ts_pts_rewriter::rewrite_pcr_value` handles a source PCR
discontinuity in three ways, and the third is the one worth knowing about.

* **Backward jump** (source PCR goes back > 500 ms) — re-anchor and bridge
  with master elapsed so output PCR never moves backwards across it. A
  **smaller** backward step is the source's own and passes through as the
  step it is, DI and all when the source set one — so muxer-mode output is
  monotonic except across such a step. That is deliberate for the one
  source that makes it routinely: an ingress transcode raising its PCR
  delay (`ts_pcr_remux`) steps only its PCR back, with DI, and keeps every
  PES timestamp continuous. Bridging that step would keep the output PCR
  monotonic only by moving every PES timestamp on the flow forward by the
  raise — an audio hole and a held frame on every PTS-driven output (HLS,
  CMAF, WebRTC, RTMP, the display). Receivers re-lock on the DI; `wire_emit`
  re-anchors its pacing on any backward step. Pinned by
  `a_small_backward_pcr_step_passes_through_with_its_di` and
  `an_ingress_pcr_delay_raise_keeps_pes_timestamps_continuous`.
* **Forward jump the wall clock witnessed** — pass through, DI=1. A live
  edit point or SCTE-35 splice is a real gap in the content, and passing it
  through preserves PCR_FO rate accuracy (TR 101 290, ±30 ppm). The
  `pcr_jump_signal` tells that input's audio replacer to silence-pad it.
* **Forward jump the wall clock did *not* witness** — bridge it. A file loop
  wrap leaps a whole programme duration in milliseconds of real time; passed
  through, that leap lands in the presentation timeline and the display sheds
  frames indefinitely trying to absorb it (measured decaying from 30 fps
  presented to 10–17 fps on a video-only loop).

Both bridges are clamped to `MAX_BRIDGE_ADVANCE_27MHZ` — 40 ms, the TR 101 290
PCR repetition-rate ceiling. **The clamp does not distinguish an instantaneous
seam from a long genuine outage.** An input that reconnects after 30 s is
charged 40 ms, and the resulting offset between the output timeline and wall
clock persists for the life of that anchor.

That is a deliberate trade. Output stays monotonic and PCR-conformant, which
is what receivers require; the alternative — trusting raw elapsed master time
— was measured injecting each reopen's latency (~38 ms H.264, ~163 ms MPEG-2)
into the timeline every loop, so output PTS crept ahead of its own content
until the frame queue was shedding continuously. What is given up is absolute
wall-clock alignment across a long gap, not stream validity. Pinned by
`bridge_clamp_charges_a_long_outage_only_one_pcr_rr_interval`.

On first PES + every >500 ms source-PTS discontinuity, the audio
replacer anchors via the same `anchor_target` helper with the same
10 s safety. Wired through
[`engine::transcode_chain::build_for_output`](../src/engine/transcode_chain.rs)
and
[`engine::input_transcode::InputTranscoder::set_av_sync_pacer`](../src/engine/input_transcode.rs).
The video replacer takes no pacer: it generates no PCR.

**When does the rewriter actually rewrite?** Only when the
`anchor_target` 10 s safety lets it. On a flow with a `Wallclock`
master (switcher / file / replay / WebRTC / test-pattern) and a
typical encoder-relative source PTS, the safety triggers and the
anchor falls back to source PTS — effectively a no-op (output
PTS == source PTS). To actually see
master-clock-derived PTS output, the flow needs either:

- `master_clock.kind = "ptp"` with PTP-disciplined sources, OR
- `master_clock.kind = "contribution"` (or legacy `source_pcr_pll`)
  AND the PLL has locked.

This matches the existing pre-rewriter behaviour for the safe path,
and unlocks the master-clock anchor path for the genuinely
clock-coherent configurations where it improves output quality.

**Which PIDs are rewritten.** Every PES-bearing ES learned from the
PMT — audio, video, AND other PES carriers (DVB teletext, DVB
subtitles, KLV metadata, ST 2038 ANC, DSM-CC PES). Once the PCR has
been re-anchored to the master clock, *any* PES timestamp left in the
source timebase is dead on arrival downstream, so partial coverage is
not an option. Roles are resolved from the PMT `stream_type` **plus
the ES-info descriptor loop** (`ts_parse::descriptor_audio_kind`):
DVB-style audio carried as `stream_type 0x06` + AC-3 (0x6A) /
E-AC-3 (0x7A) / AAC (0x7C) / DTS (0x7B) / registration descriptor is
classified **audio** and receives the lipsync trim, exactly like its
ATSC (0x81/0x87) siblings; a 0x06 ES without an audio descriptor
(teletext, subtitles, KLV) is re-anchored without lipsync.
Section-carrying stream types (0x05 private sections, 0x0A–0x0D
DSM-CC, 0x86 SCTE-35) are never PES-rewritten — SCTE-35 timing is
handled by the dedicated `pts_adjustment` rewrite. Before 2026-06-05
only bare-`stream_type` audio/video was rewritten, which left
DVB-0x06 AC-3 PES at source PTS hours away from the regenerated PCR —
silent audio on every compliant receiver (the "Network TEN" bug).

**How the PMT is found.** Every packet on a PMT PID goes through a
section assembler, and every section it completes is considered, not
just the one at the pointer_field target. A payload unit may carry
several sections, and a PMT PID may carry other tables: ATSC /
DigiCipher muxes put a short-form 0xC0 section ahead of the PMT in
every PMT-PID packet. The rewriter used to read only the pointer-target
table, never learned a role on such a stream, and so re-anchored every
PCR while leaving every PES timestamp in the source timeline — VH1.ts
came out with a constant −12.6 h PTS−PCR on every output, transcoded or
not. A PMT spanning two packets is learned too — also from a muxer that
never advances the CC on PSI, whose same-CC continuation the assembler
takes CRC-gated. Once the rewriter has stamped a PSI PID (the PAT or a
PMT PID) it owns the CC of **every** packet on it — continuation packets
whether or not the assembler still has their section in flight, and
adaptation-field-only packets, which repeat the last payload CC — so a
PSI PID never mixes rewriter-owned and source CCs.

**PCR is never regenerated for long without roles.** PCR re-anchoring
starts at the first PCR, before the PMT is known. If no PMT has been
learned after 2 s of source-PCR time (four times TR 101 290's 500 ms
PSI repetition), the rewriter stops regenerating PCR and passes the
**source clock** through — PCR and the untouched PES timestamps then
agree — with DI=1 on the first passed-through PCR, and raises the
Warning `clock_rewrite_pmt_not_learned` (input-scoped; see
[`events-and-alarms.md`](events-and-alarms.md)). When a PMT is learned
later, the anchor is re-established on the next PCR, again with DI=1,
and regeneration resumes. A stream with parseable PSI never reaches
this path. The window is not a one-shot per rewriter: when the PAT drops
the PMT PID a PMT was learned from — a media-player playlist item or a
re-muxed upstream moving to a new program, including a spliced PAT that
changes its PMT PID without bumping its version (it counts only when its
CRC verifies) — the rewriter forgets that program's PCR PID and PES roles
and re-arms the window for the new one. While the source clock passes
through, no PES / SCTE-35 timestamp is re-anchored either.

**No PES leaves before the anchor.** A PES that *starts* before the
first PCR has established the anchor — or before a PMT has described the
stream — is dropped whole, its continuation packets included. It used to
pass through with its **source** timestamps on a stream whose PCR is
regenerated: a stream joined mid-GOP, or the media player's first PES
(Sky carries a video PES at packet 14 and its first PCR at packet 23),
reached the wire ~20 550 s off the live timeline and tripped the flow's
source-discontinuity watch into a DI. A dropped packet that carries the
PCR or DI survives adaptation-field-only (on Sky the first PCR rides in
a continuation packet of exactly such a PES), and the CC of every later
packet on that PID is lowered by the number dropped so the sequence stays
continuous. Nothing decodable is lost: the decoder needs PMT + PCR before
anything. In the source-clock fallback above nothing is dropped (PCR and
PES agree there). Pinned by
`a_pes_started_before_the_first_pcr_is_dropped_whole`.

The hold is bounded. Once it has held 2 s of PES time on a PID (forward
steps only, each capped at 1 s) or 2 s of wall time, it gives up waiting:

- **No PCR has established the anchor** — a program whose PCR_PID never
  carries one, like an audio-only RTMP publish (the ingest muxer names the
  absent video PID as PCR_PID), or a PMT with PCR_PID 0x1FFF and no PCR
  anywhere. PES then pass with their **source** timestamps, as they would
  with no rewriter at all — there is no regenerated PCR for them to
  disagree with — and the Warning `clock_rewrite_no_pcr` (input-scoped)
  says so. The first PCR that does arrive on a PCR PID establishes the
  anchor, with DI=1, and regeneration starts from there. Without the bound
  every PES of such a stream was dropped for good and every output of the
  flow went silent.
- **An anchor but no PMT** — the same source-clock fallback the PCR-timed
  window above takes, `clock_rewrite_pmt_not_learned`.

Pinned by `pes_with_no_pcr_on_the_pcr_pid_pass_after_the_hold_window` and
`a_pmt_with_pcr_pid_1fff_and_no_pcr_still_emits_pes`.

## Transcoded output PCR (the remux model)

Every TS transcode chain — each output's `transcode_chain` and each
input's `InputTranscoder` — ends in `engine::ts_pcr_remux::TsPcrRemux`.
The replacers ahead of it only **preserve PCR positions**: every input
PCR on the source PCR_PID (inside a video payload packet too) leaves the
`TsVideoReplacer` as an adaptation-field-only packet on the video PID at
the same stream position, value and DI unchanged; a PCR on the audio PID
leaves the `TsAudioReplacer` the same way on the audio PID (it used to
vanish with the source payload — a radio service transcoded audio-only
had no PCR at all). No re-encoded PES carries a PCR.

The stage rewrites every PCR on the output program's PCR_PID to
`input PCR − D` and checks the decode timestamp of every re-encoded PES
against the input PCR at its position. Every ES therefore keeps its
source T-STD lead, shifted by `D` minus its own pipeline delay, and the
output PCR advances **exactly as the input's did**: no rate error, no zero
or backward steps, and a PCR as often as the input carried one (≤ 31 ms
on the gate sources) — also while the decoder waits for its first
recovery point, which used to leave 527 ms with no PCR at flow start.

What it replaced, measured on the gate captures: a PCR derived from the
re-encoded video PTS and floored on a decaying audio lag advanced
3 656 ticks per 3 600-tick PAFF frame — **+15 556 ppm** against PTS and
wall (TR 101 290 PCR_FO allows 30 ppm) — snapped back with zero and
backward steps (35 zero deltas in 200 s, 7 × −3.73 ms on Spain), appeared
only once per frame (exactly 40 ms at 25 fps, over it at 24 fps), and
wandered by the source's content-dependent mux interleave (89–917 ms).

**`D` is measured.** It starts at 80 ms — the pre-roll the old path kept —
so a pipeline inside that never steps the clock. The first re-encoded PES
of each PID in an epoch latches `D ≥ max(0, lateness) + 80 ms`, where
lateness is how far the PES arrived behind its own decode time
(`input PCR − DTS`); the stage's very first latch may also lower `D`, but
only as far as the residency cap demands. After that a PES that is still
late is the exception path: `D` is raised to its lateness + 80 ms, the
next PCR carries DI = 1, `late_frames` counts it, and the Warning
`transcode_pcr_late` says so (at most once per 10 s). `D` only ever grows
after the first latch — lowering it would be another PCR step — and a
latch that would move it by less than 10 ms leaves it alone (on Sky the
first audio PES asked for 80.24 ms against the initial 80: a DI for a
quarter of a millisecond). A PES more than 5 s late is taken to be stamped
on another timeline and never moves `D`.

**The residency cap.** The margin is cut (never below lateness + 40 ms)
so the largest lead of the program's video observed in the epoch stays
within the 1 s T-STD residency (ISO/IEC 13818-1 §2.4.2.6); when even that
cannot be met the PES is kept on time and
`transcode_pcr_residency_exceeded` says so. Only the video ES of the
program the stage follows count — on an MPTS output another program's
video, whose PCR this stage never shifts, is ignored. The cap is re-checked
every time a larger lead is seen, not only at a latch: before anything
re-encoded has been measured `D` is lowered to it (one PCR step, DI);
after that `D` stays — lowering it would make the PES already measured
late — and the Warning fires instead (its `lateness_ms` is then null). An
audio-only transcode over a long-lead passthrough video is the case: on
Sky the audio latches within the first ~100 ms, against the few hundred
ms of video lead seen so far, and the 943 ms leads come later.

**Gaps are the source's.** Lateness is measured against the input PCR at
the PES's position, and a PCR-per-frame source (the RTMP / RTSP / WebRTC
ingest muxer writes PCR = DTS on each frame's first packet) that pauses —
or runs at a variable frame rate — puts a long PCR step between a frame
and the next one it is emitted after. The part of such a step beyond the
input's usual step (the median of its last eight continuous steps; a gap
is a step over twice that and over 100 ms) is taken off the lateness of
every PES decoded before it. The frames that were in the pipeline when the
source paused still reach the wire late — nothing can undo a pause — but
they no longer raise `D` for every frame after them: a single 900 ms
pause used to add a second of latency for good. A steady low frame rate is
not a gap: at 5 fps a re-encoded frame that leaves after the next frame's
PCR is 200 ms late every time, and `D` latches once to that. Pinned by
`a_source_pause_does_not_ratchet_d` and
`a_pcr_per_frame_source_at_low_frame_rates_is_one_timeline`.

**Nothing re-encoded.** While neither replacer re-encodes — its codec
cannot be decoded, a replacer fell back to passthrough — the stage applies
no `D` at all and the stream passes byte-identical, as it did before the
remux model. Re-encoding starting or stopping steps the PCR by `D`, with
DI.

**Epochs.** An input PCR that steps backward or carries DI starts an
epoch: DI on the next output PCR, every PID latches again (raise-only).
A forward step without DI is the input's own clock however long it is —
5 fps steps 200 ms per frame, 0.5 fps two seconds — and passes as the step
it is; it used to start an epoch past 100 ms, which put DI on every output
PCR of a low-frame-rate source, re-latched `D` up to its largest frame gap,
and past 1 s dropped the frame in flight as stale. On an epoch of more than
1 s the re-encoded PES still in flight from the previous epoch — closer to
the old timeline than to the new one — are dropped, their CC renumbered,
so they neither reach the wire behind the DI nor drive `D`.

**No input PCR.** When a re-encoded video PES on the PCR_PID arrives and
no input PCR has been seen — or none for 100 ms of video decode time — the
stage synthesises PCR from the video: `DTS − D` in an AF-only packet
before every video PES, plus interpolated ones on the observed packet
rate so no two are more than 35 ms apart. Info `transcode_pcr_synthesized`
(once per stage). The next input PCR ends synthesis with DI.

**What changes for a receiver.** The PCR→PTS relationship of every
transcoded TS output moves: a re-encoded ES now leads the PCR by its
source lead plus `D` minus its pipeline delay (Sky 1080i25 through x264:
~0.3–1.05 s of video lead, against 0.4–1.0 s under the PTS-derived PCR).
PES PTS are not moved, so lip-sync is untouched. An audio-only transcode
(video and PCR passing through) is now delayed too: the audio replacer
emits PES *k* only when PES *k + 1* arrives, and against an unshifted PCR
3 930 of Sky's 4 016 re-encoded audio PES were late before any encoder
delay.

**Where it does not reach.** HLS / CMAF / RTMP / WebRTC use PES
timestamps only. A `D` raise on an **ingress** transcode reaches the
input's muxer-mode rewriter as a DI'd backward PCR step, which it passes
through as such (see [PCR discontinuity bridging](#pcr-discontinuity-bridging-and-what-the-clamp-costs))
so PES timestamps stay continuous on every output; at flow start the first
latch usually makes one such step. `epoch_lock` forbids transcoding. On a PID-bus
assembled flow an ingress transcode's `D` reaches the wire only when that
input is the program's `pcr_source`; the assembler re-anchors otherwise —
as before. Wire pacing in the default `auto` (forward) egress mode still
releases each chunk when the codec thread produces it, so PCR arrival
jitter keeps the codec thread's burstiness; `egress_pacing: "pcr"` is what
removes it.

**Telemetry.** `transcode_pcr` on each transcoding output's stats and, for
the active input's ingress transcode, on the flow's: `offset_ms` (the
current `D`), `late_frames`, `offset_raises`, `stale_frames_dropped`,
`synthesized_pcrs`, `epochs`.

## Module map

| Module | What it does |
|--------|--------------|
| `engine/master_clock.rs` | The `MasterClock` trait, `MasterClockKind` enum, `MasterClockHandle` (Arc + tag + clamped lipsync trim), `WallclockMaster`, `SourcePcrPllMaster`, `PtpMasterClock`, and the auto-select policy |
| `engine/pcr_pll.rs` | Software PI-controller PLL recovering source's 27 MHz from incoming PCR samples. PI loop on `(Δpcr_ticks, Δwall_ns)` with re-anchor on every accepted sample. Discontinuity filter mirrors `pcr_trust.rs` (gaps > 500 ms reset the anchor). Sticky lock-state hysteresis (enter at p99 < 100 µs, exit at > 500 µs). `now_27mhz(wall_ns)` projects forward from the anchor at the recovered rate so PCR generation never quantises to the ingress PCR cadence. |
| `engine/pcr_ingress_sampler.rs` | Per-flow ingress PCR sampler. Sibling broadcast subscriber (drop-on-Lagged) that scans every `RtpPacket` for adaptation-field PCRs and feeds the master's PLL. Handles both raw TS and RTP-wrapped TS via best-effort RTP header skip. Passive observer — never blocks the data path. |
| `engine/av_sync_mux.rs` | `AvSyncPacer` — thin wrapper around `MasterClockHandle` that exposes `is_locked()`, the lipsync trim, the `assembler_owned` hand-off, and `pcr_27mhz_for_emit()` (master_now − PCR_PREROLL_27MHZ, modular-aware); that last one is `#[allow(dead_code)]`, called only from tests, and on no production path. It generates no PCR: the transcoded path's old `pcr_for_emit` (`pts × 300 − preroll`) is gone. |
| `engine/ts_pcr_remux.rs` | The trailing PCR stage of every transcode chain: output PCR = input PCR − a measured transcode delay, the lateness guard, epochs, stale-frame drop, PCR synthesis when the input has none. See [Transcoded output PCR](#transcoded-output-pcr-the-remux-model). |
| `engine/ts_pts_rewriter.rs` | Encoder-style byte-level PES PTS/DTS rewriter, per-PID anchor + source-delta model. Plugs into `input_post_process::InputPostProcess` as a fourth optional stage; on by default (muxer mode) unless per-input `passthrough_clock: true` opts out, plus an attached `AvSyncPacer`. See the "Encoder-style PES PTS regeneration" section above for the model. |
| `stats/pcr_trust.rs` | Per-output egress PCR accuracy sampler (4096-sample rotating reservoir, exact percentiles). Sibling consumer of the same PCR sample stream as the ingress PLL, but on the egress side. |
| `engine/wire_emit.rs` | Per-output PCR-anchored wire emission engine. Dedicated `std::thread` (Linux: `SCHED_FIFO` best-effort priority 50) pops TS datagrams off a `std::sync::mpsc::sync_channel(WIRE_CHANNEL_CAP)` — 8192 datagrams (bumped from 1024 to absorb SRT jitter-buffer dumps and ST 2110 frame bursts; ≈ 14 s in flight at 6 Mbps TS, ~3.5 s at 25 Mbps, ~30 ms at 3 Gbps ST 2110), with codec backpressure engaging at 75 % occupancy (`WIRE_CHANNEL_BACKPRESSURE_THRESHOLD` = 6144) — fed by the encoder task. Two release tiers: (1) **`clock_nanosleep(CLOCK_TAI, TIMER_ABSTIME)`** on SCHED_FIFO — the **default**; ~50–500 µs typical jitter, no kernel / NIC / PTP prerequisites. (2) **SO_TXTIME** — kernel-paced via the `etf` qdisc on `CLOCK_TAI`; sub-µs jitter when paired with HW-PTP, ~1–10 µs with software ETF. **Opt-in** via `BILBYCAST_ENABLE_TXTIME=1`; the probe is not attempted by default because on a host without the ETF qdisc the kernel accepts `setsockopt(SO_TXTIME)` and the `SCM_TXTIME` cmsg silently but emits each packet immediately, producing silent degradation. Closed-loop on observed inter-PCR rate (no declared-bitrate parameter — open-loop drifts when the encoder runs above/below its configured target). Discontinuity > 500 ms or any backwards step resets the anchor; a per-emitter monotonic-target guard prevents kernel ETF reorder on PCR discontinuities. **Wired into UDP, RTP (single-leg + FEC + 2022-7 dual-leg), 302M, ST 2110-20/-23/-30/-31/-40.** SRT, RIST, RTMP, HLS, CMAF, WebRTC keep their protocol-layer pacing. The legacy `BILBYCAST_FORCE_NANOSLEEP=1` env var is kept as a no-op alias for back-compat (the default is already nanosleep). Full doc: [`wire-pacing.md`](wire-pacing.md). |

## Data flow

```
                            ┌──────────────────┐
                  ┌────────►│  PcrPll (PLL)    │◄──── 1 Hz telemetry tick →
                  │         └────────┬─────────┘      FlowStatsAccumulator
   Per-input ┌────┴────┐             │                  ↓
   forwarder │broadcast│             │ now_27mhz()     FlowStats.master_clock
       ──────►   _tx   │             ▼                       (over WS)
              └────┬───┘    ┌──────────────────┐
                   │        │ AvSyncPacer      │◄───┐
            ┌──────┴─────┐  │ (handle wrapper) │    │ holds Arc<MasterClockHandle>
            │ PcrIngress │  └──────┬───────────┘    │
            │  Sampler   │         │                │
            └────────────┘         │ sampled ONCE, at the first PCR:
                                   ▼   anchor = master_now-preroll
                           ┌──────────────────┐
                           │ ts_pts_rewriter  │── anchor+(src_pcr−anchor_src) ──→ TS bytes
                           │ (ingress stage 4)│   PES PTS/DTS re-anchored the same way
                           └──────────────────┘

                           ┌──────────────────┐
                           │ TsAudioReplacer  │  input PCRs carried as AF-only packets
                           │ TsVideoReplacer  │  (PTS from source; no PCR generated)
                           └────────┬─────────┘
                                    ▼
                           ┌──────────────────┐
                           │  ts_pcr_remux    │── PCR = input PCR − D (measured) ──→ TS bytes
                           └──────────────────┘                                 ──→ broadcast_tx
```

## PCR pre-roll

Muxer mode places its anchor PCR at `master_now − PCR_PREROLL_27MHZ`,
and the transcode PCR stage starts its delay `D` at the same value (its
latch keeps at least 80 ms of margin over the lateness it measures): the
pre-roll is **80 ms** (2 160 000 ticks) either way. This matches the
ISO/IEC 13818-1 Annex L T-STD model — receivers need PCR to lead PTS by
at least the transport-buffer + CPB pre-roll. Choosing 80 ms also limits
the apparent A/V offset on receivers that don't apply T-STD scheduling
to audio.

The pre-roll is declared in three places:

- `engine::av_sync_mux::PCR_PREROLL_27MHZ` (what the dead-code
  `pcr_27mhz_for_emit` subtracts).
- `engine::ts_pts_rewriter::PCR_PREROLL_27MHZ` — **the copy the default
  muxer-mode path actually anchors from**. `rewrite_pcr_value` seeds the
  anchor with `master_now − PCR_PREROLL_27MHZ`, and that rewriter has two
  live constructors: `engine::input_post_process` (ingress stage 4) and
  `engine::ts_assembler`. The lipsync trim is added by
  `compute_anchored_value` to the anchored PTS this constant seeded — it
  is not folded into the constant.
- `engine::ts_pcr_remux::MARGIN_27MHZ` — the transcode PCR stage's
  initial delay and latch margin.

## Lipsync trim

The handle exposes a per-flow lipsync offset in 90 kHz ticks, bounded
±18 000 (±200 ms). Updates are lock-free (`AtomicI64::store`). Manager
operators nudge it via the WS command `set_master_clock_lipsync` — see
`bilbycast-manager/docs/...` for the REST mirror.

The trim is applied to output PTS: `engine::ts_pts_rewriter` folds
`lipsync_offset_90k` into the anchored PES PTS/DTS **on audio PIDs only**
(`anchor_out_90k = master.now_27mhz()/300 + PCR_PREROLL_90K +
lipsync_offset_90k`), and it is surfaced on `FlowStats.master_clock`. It
shifts audio relative to video; a positive value moves audio later. It
does not retime video PIDs (see the "Encoder-style PES PTS regeneration"
and "Known limitations" sections).

## Telemetry

Every running flow surfaces a `master_clock` block on `FlowStats`:

```json
{
  "master_clock": {
    "kind": "source_pcr_pll",
    "locked": true,
    "rate_offset_ppm": -2.34,
    "jitter_us": 18,
    "lipsync_offset_90k": 0
  }
}
```

The 1 Hz background task in `FlowRuntime::start` mirrors the master's
`telemetry()` snapshot into the stats accumulator. Manager UI renders
the kind label, lock chip, rate offset, p99 jitter, and the trim knob.

## Capability gating

Edges advertise `master_clock` on `HealthPayload.capabilities`. Manager
UI gates the per-flow telemetry card + trim knob on this string. Older
edges (no master-clock work) keep their existing behaviour unchanged
and the manager UI hides the controls automatically.

## Tests

Lib-level tests cover:

- `MasterClockHandle`: construction, lock-state, kind-tag-preservation,
  clamped lipsync, one-shot degraded-warning marker.
- `PcrPll`: convergence on perfect 25 Hz cadence within 5 s, drift
  tracking (100 ppm fast source converges to +100 ppm offset),
  discontinuity filter, modulus wrap, p99 jitter bound, pre-sample
  monotonic fallback.
- `AvSyncPacer`: wallclock pacer always locked, PCR emit trails master
  by pre-roll, modular wrap when master_now < pre-roll.
- `TsPcrRemux`: output PCR = input − D at the input positions, the
  latch and the guard, the residency cap (at a latch and as the video lead
  grows, the followed program's video only), stale frames across an epoch
  jump, a PCR-per-frame source at 5 and 0.5 fps as one timeline, a source
  pause that does not raise `D`, a byte-identical stream with nothing
  re-encoded, synthesis without an input PCR; and the audio-only chain end
  to end (`audio_only_transcode_keeps_every_re_encoded_pes_ahead_of_the_pcr`).
- `PcrIngressSampler`: raw-TS sampling, RTP header skip with CSRC +
  extension, no-sync-byte payload silently dropped.
- `PtpMasterClock`: unavailable defaults, telemetry kind tag.

Run: `cargo test --features video-encoders-full`. All 993 lib tests
pass with the master-clock work in.

## Known limitations

- **AudioMaster** (ALSA local-display master) is reserved but not
  implemented; the kind tag falls through to Wallclock.
- **Lipsync trim** applies to the PES PTS rewriter
  (`engine::ts_pts_rewriter`) and the audio replacer
  (`TsAudioReplacer::set_av_sync_pacer`); it is **not** yet applied
  to the transcoded video replacer's output PTS values.
- **PCR pre-roll** is hard-coded at 80 ms; a future enhancement could
  expose it per-flow for low-latency contribution where 40 ms is
  preferable.
- **PCR rewriting and PCR passthrough are all-or-nothing per input.**
  In default muxer mode `engine::ts_pts_rewriter` rewrites PCR as well
  as PES PTS/DTS: on every learned PCR_PID it recomputes the value with
  `rewrite_pcr_value`, patches the six adaptation-field bytes in place
  (`write_pcr_field_in_packet`), sets DI=1 whenever it re-anchors, and
  injects synthetic PCR_RR carrier packets on sparse-PCR sources — see
  "PCR discontinuity bridging" above. Source PCR bytes reach the wire
  untouched under per-input `passthrough_clock: true` or on a `bonded`
  input, where the rewriter is not in the path at all; opting out to keep
  them therefore also opts out of PES PTS/DTS regeneration. That is the
  precondition the `epoch_lock` table below turns on. There is a third,
  **unasked-for** route: a PAT showing more than one program latches the
  rewriter into verbatim packet-for-packet passthrough permanently
  (`mpts_passthrough_latch`) — the rewriter carries one `ClockAnchor`,
  which is SPTS-only — so an MPTS ingress silently gets neither PCR nor
  PES regeneration. Down-select with `program_number` to get muxer mode
  back. It is not an alignment route either: `epoch_lock` is
  single-program by scope.

## Local PTP grandmaster for testing

ST 2110 and MXL flows refuse to start without a working `ptp4l` —
`PtpStateReporter` reads `/var/run/ptp4l` and the master clock fails to
lock if no grandmaster is present. For development on a workstation
with no broadcast PTP fabric on the wire, the `ptp-gm/` directory at
the monorepo root contains a small helper that runs `ptp4l` (and
`phc2sys` on HW-PTP NICs) as a free-running grandmaster:

```
ptp-gm/
├── bilbycast-ptp-gm.conf   # ptp4l GM config (SMPTE ST 2059-2 timings, domain 127)
├── bilbycast-ptp-gm.sh     # start | stop | status | restart | logs | help
└── README.md               # how-to + tier comparison + troubleshooting
```

It exists so that:

- ST 2110 + MXL bring-up tests work without a hardware PTP grandmaster
  on a separate machine.
- TS flows on the default `master_clock.kind = "wallclock"` get
  NIC-disciplined CLOCK_REALTIME for free when the script is in HW
  mode — `phc2sys -a -r -r` keeps the system clock locked to the NIC
  PHC, so `engine::wire_emit`'s CLOCK_TAI pacing and PCR generation
  inherit that stability.
- Cross-host PCR_AC and 2022-7 hitless measurements have a deterministic
  shared time source even when neither host has GPS.

Tiers (auto-picked from the NIC):

| Tier | Use when | Floor | bilbycast lock |
|---|---|---|---|
| A (software) | proving the master-clock / ST 2110 / MXL code works | ~tens of µs | yes — `PtpStateReporter` reads `/var/run/ptp4l` regardless of timestamping mode |
| B (HW PHC)   | publishing sub-µs claims, narrow VRX, tier-1 PCR_AC | < 1 µs | yes — and `phc2sys` extends the discipline to CLOCK_REALTIME/CLOCK_TAI so TS-wallclock flows benefit too |
| C (GPS)      | compliance demos, UTC traceability | < 1 µs UTC | same — only changes `clockClass` advertised |

Quick start (the script lives outside `bilbycast-edge/`; this doc just
points at it):

```bash
sudo /path/to/monorepo/ptp-gm/bilbycast-ptp-gm.sh start
/path/to/monorepo/ptp-gm/bilbycast-ptp-gm.sh status
```

The full how-to, NIC auto-pick rules, chrony coexistence notes, and
cross-host setup are in `ptp-gm/README.md`. The helper is **for
testing only** — production deployments run `ptp4l` + `phc2sys` from a
distribution package or vendor-supplied unit, locked to a real
grandmaster (Meinberg, ESI, FsPro, or similar).

## See also

- [`wire-pacing.md`](wire-pacing.md) — PCR-anchored / PTP-raster-anchored
  kernel-paced wire emission. Master-clock generates the PCR values
  inside the bitstream; wire pacing ensures those PCR-bearing packets
  hit the wire at the matching wallclock instant. Both are required
  for tier-1 PCR_AC at the receiver.
- [`../../ptp-gm/README.md`](../../ptp-gm/README.md) — local PTP
  grandmaster helper used during development and testbed runs.
- [`../packaging/setup-etf-qdisc.sh`](../packaging/setup-etf-qdisc.sh)
  and [`../packaging/bilbycast-etf-qdisc@.service`](../packaging/bilbycast-etf-qdisc@.service) —
  egress-side ETF qdisc setup for SO_TXTIME wire pacing. Independent of
  the GM helper; production-only, and the userspace
  `clock_nanosleep(CLOCK_TAI)` tier is the default elsewhere.

---

## Cross-node egress alignment (`epoch_lock`)

Two edges receiving the same contribution feed over independent paths do
not, by default, emit the same content at the same wall-clock instant.
Each node's egress phase is set by its own ingest latency, so the two
streams sit apart by the difference between those paths — routinely
hundreds of milliseconds. Cutting between them downstream produces a
visible jump in content and in receiver buffer occupancy.

`epoch_lock` removes that difference. It is configured per compressed
UDP/RTP output and requires a manager-minted group anchor.

### How it works

The wire emitter normally anchors on the first datagram it sees
(`now + PREROLL_NS`). Under epoch lock it instead derives each
PCR-bearing datagram's release instant analytically:

```
release_instant = anchor.unix_ns
                + (pcr − anchor.pcr_27mhz) × 1000/27      # source ticks → ns
                + egress_offset_ms
```

Every member runs identical arithmetic on an identical PCR and an
identical anchor, so alignment falls out of the maths rather than from a
control loop. The derivation reads **no local clock at all** — that is the
property that makes it work, and it is checkable by inspection in
`engine::epoch_lock::unix_ns_from_pcr_anchored`.

### The precondition that actually matters

**The PCR reaching the emitter must be a function of the content, not of
the node.** That is only true where `engine::ts_pts_rewriter` is out of
the path:

| Path | Output PCR | Usable? |
|---|---|---|
| `passthrough_clock: true` on every input | `= src_pcr`, byte-for-byte | **yes** |
| `bonded` input (never builds an `InputPostProcess`) | `= src_pcr` | **yes** |
| Default muxer mode | `T_first_ingest(this node) + Δsrc − 80 ms` | no |
| Transcoded output | inherits the above, minus a per-output measured transcode delay | no |
| PID-bus assembled flow | assembler's own anchor, seeded by this node's slot fan-in order | no |

In muxer mode `rewrite_pcr_value` samples the master clock **once**, at
the first PCR it sees, and free-runs on source deltas after that. The
emitted PCR therefore carries this node's ingest instant as a fixed
additive term — exactly the quantity alignment has to cancel.

**Trade-off:** `passthrough_clock: true` also disables PES PTS
regeneration and the discontinuity bridge on that input. Alignment and
PCR/PTS regeneration are mutually exclusive.

### The group anchor

A free-running source PCR has no relationship to wall time, so the
mapping must come from outside the node. The manager mints one
`(pcr_27mhz, unix_ns)` pair and pushes it byte-identically to every
member via the `set_epoch_anchor` command.

`unix_ns` is a **label**, not the true origination instant. The manager
mints it from the **slowest member's** observed arrival plus a margin, so
each member's required egress dwell is its lead over the slowest — not its
absolute end-to-end latency. That distinction is what keeps a WAN
contribution path clear of the wire-emit residence cap: anchoring on true
origination would demand a dwell equal to the full SRT latency buffer.

Re-mints carry `effective_from_pcr` so every member switches on the same
*packet* rather than at the same moment on its own clock. Without that,
the switch opens a divergence window equal to the whole anchor step.

### Withdrawing the anchor

`set_epoch_anchor` has a second form, `{"output_id": "...", "clear": true}`,
which takes the output back off the group timeline: it drops to the
closed-form inversion (still covered by the plausibility gate) and resumes
publishing mint observations. The manager sends it when a group is deleted,
when a member leaves the roster, and as the **first half of a re-mint**.

That last one is what makes a re-mint mean anything. An armed emitter
publishes no mint observation — its dequeue instant no longer answers "when
did this node have this content" — so the manager cannot derive a fresh
anchor until every member has been withdrawn. Re-minting without withdrawing
first re-derives the *identical* anchor from each member's frozen
first-engagement reading: generation increments, the UI turns green, and
egress phase does not move.

The withdrawal is a **published generation-0 anchor**, not an absent one.
`EpochAnchorCell::load` already returns `None` for a torn read (its bounded
retry giving up), so encoding "withdrawn" as `armed = false` would make it
indistinguishable from a preempted writer — and the emitter would drop a live
anchor mid-air on a transient it is specifically built to tolerate. The
manager mints generations from 1, so 0 can never name a real anchor; the edge
rejects a non-withdrawal command carrying generation 0 rather than let the
sentinel be spoofed.

Correspondingly, the edge **retires the mint pair on adopt**
(`OutputStatsAccumulator::clear_epoch_mint_observation`) so an armed member
reports zeros rather than a stale reading. `EpochLockStats` documented this
behaviour long before anything implemented it.

### Sizing `egress_offset_ms`

Bounded 150–800 ms, and both ends are derived rather than chosen:

- **Floor** — output PCR trails the master by `PCR_PREROLL_27MHZ` (80 ms)
  plus local mux/fan-out depth. Below that no future target exists and the
  node sits at a permanent deficit.
- **Ceiling** — epoch lock forces `egress_pacing: "pcr"`, under which
  `egress_buffer_ms` is rejected, so `DejitterConfig::lossless` pins the
  residence cap at 1000 ms with no operator override. Dwell is measured
  from enqueue, so an offset at or above the cap makes
  `shed_stale_backlog` fire on essentially every datagram — measured at
  84–96 % sustained loss, and bitrate-independent.

The offset must be **identical on every member**. A mismatch misaligns the
group by exactly the difference, with every node reporting healthy.

There is also a bitrate ceiling, because the dwell is held in the
fixed 8192-slot wire channel (`86.2 Mbit` at 1316-byte datagrams):
sustainable rate ≈ `86.2 Mbit / (offset − 80 ms)`. At 250 ms that is
~520 Mbps; at 500 ms ~208 Mbps. Multi-gigabit and a large offset are
mutually exclusive.

### Telemetry

`OutputStats.epoch_lock` carries `group_label`, `egress_offset_us`,
`engaged`, `disengaged`, `deficit_us`, `deficit_max_us`, `clamped`,
`implausible`, `anchor_generation`, and the mint observation pair
`mint_pcr_27mhz` / `mint_unix_ns`.

- `deficit_us` — this node released **late**: it cannot meet the offset.
  Remedy: raise the offset, or fix the slow path.
- `clamped` — this node released **early**: the target exceeded what the
  emitter will hold. Opposite remedy. Reported separately for that reason.
- `disengaged` — the plausibility gate took this output off the analytic
  anchor because the PCR is not on the group timeline. The output is
  running, but it is *not aligned*.
- `anchor_generation` — members on different generations are misaligned by
  the difference between their anchors, which no other field reveals.
- `mint_pcr_27mhz` / `mint_unix_ns` — a source PCR this node saw and the
  wall instant it had that content ready to release. The manager
  normalises every member's pair onto one reference PCR and anchors on
  the **slowest**, which is what makes the required dwell the inter-node
  latency *spread* rather than the absolute end-to-end latency. Both read
  **zero while the output is armed** — retired on adopt via
  `clear_epoch_mint_observation`, because a dwelling emitter's dequeue
  instant no longer answers "when did this node have this content". That
  retirement is what lets the manager tell a fresh observation from a
  frozen one, and therefore why a re-mint must withdraw the anchor first
  rather than re-deriving the identical one.

**This is not an alignment measurement.** A node cannot verify its own
alignment; that needs an external observer timestamping every member
against one clock.

### Scope

Single-input, single-program, non-transcoded, non-assembled UDP/RTP
forwarding. Two independent encoders do not produce the same bitstream, so
transcoded outputs can never be aligned this way. Locally originated
sources (`test_pattern`, `media_player`, `replay`, `sdi`, `webrtc`) are not
carrying the same content as a peer in the first place.

Gives a clean downstream **cut**, not a seamless 2022-7 merge — RTP
sequence numbers remain per-node counters.

### Verification

A single node cannot prove alignment. Before trusting this on air, capture
both members on one host against a PTP-disciplined PHC with hardware
`SO_TIMESTAMPING`, cross-correlate identical TS payload bytes, and report
Δ emission p50/p99/max over ≥ 1 h. Gates 3, 4, 5, 6, 7 and 8 from
`testbed/BROADCAST_QUALITY_GATES.md` all apply — in particular Gate 4
(PCR_AC) with epoch lock on versus off on the identical output, since
`egress_pacing: "pcr"` re-times every datagram.
