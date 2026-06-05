# MXL implementation handoff — next session

> **Historical handoff doc (2026-05-18).** This was the next-session
> prompt written at the end of the MXL scaffolding session. MXL has
> since **shipped behind the `mxl` feature (default OFF)**. The **video
> bridges are implemented** (`src/engine/mxl_video_io.rs` — input
> encode via `ScaledVideoEncoder`, output decode-to-grain via
> `TsDemuxer` + `VideoDecoder`/`VideoScaler`). The **audio bridges
> remain stubbed** (`src/engine/mxl_io.rs` — audio input emits the
> `mxl_audio_no_encode_set` Warning with a TODO; audio output drains
> the broadcast channel via `drain_until_cancel` and emits
> `mxl_audio_decode_pending`). The **audio codec bridge is the
> remaining gap** — the audio-in / audio-out steps in Part 2 below are
> the live work; the V210 / video steps are done. Kept for reference.

Self-contained operator setup + next-session prompt for finishing the MXL
(Media eXchange Layer) integration. Generated 2026-05-18 at the end of
the scaffolding session; current state is documented in
`bilbycast-edge/docs/mxl-integration-plan.md` and in the user's memory
file `project_mxl_integration_deferred.md`.

## Part 1 — Operator setup (do this before the AI session)

### Step 1: Verify build prereqs are still in place

```bash
command -v clang cmake ninja bison flex ld.lld pkg-config && \
ls ~/vcpkg/vcpkg
```

If any are missing, reinstall per `bilbycast-mxl-rs/CLAUDE.md`.

### Step 2: Build bilbycast-edge with --features mxl

```bash
cd ~/Development/bilbycast/bilbycast-edge
export CC=clang CXX=clang++
cargo build --features mxl
```

First build is 5–10 min (vcpkg cold cache); subsequent builds ~30 s.

### Step 3: Locate the built libmxl.so and make it discoverable

After `cargo build --features mxl` completes, libmxl.so lands inside Cargo's
build directory. Find it:

```bash
find ~/Development/bilbycast/bilbycast-edge/target -name 'libmxl.so*' \
  -not -path '*/deps/*' 2>/dev/null
```

Expected output looks like:

```
.../target/debug/build/mxl-sys-<hash>/out/lib/libmxl.so
.../target/debug/build/mxl-sys-<hash>/out/lib/libmxl.so.1
.../target/debug/build/mxl-sys-<hash>/out/lib/libmxl.so.1.0.1
```

Pick one of the two options below:

**Option A — env var (no sudo, recommended for development):**

```bash
export BILBYCAST_LIBMXL_SO=$(find ~/Development/bilbycast/bilbycast-edge/target \
  -name 'libmxl.so' -not -path '*/deps/*' 2>/dev/null | head -1)
echo "BILBYCAST_LIBMXL_SO=$BILBYCAST_LIBMXL_SO"
```

Then run bilbycast-edge in this same shell.

**Option B — system install (needs sudo, persistent):**

```bash
LIBMXL_SRC=$(find ~/Development/bilbycast/bilbycast-edge/target \
  -name 'libmxl.so' -not -path '*/deps/*' 2>/dev/null | head -1)
sudo cp -P "${LIBMXL_SRC}"* /usr/local/lib/
sudo ldconfig
```

### Step 4: Verify the probe succeeds

```bash
./target/debug/bilbycast-edge --config testbed/configs/mxl-loop-edge1.json 2>&1 | \
  head -50 | grep -E '^.*mxl:'
```

Expected log line:

```
mxl: probe succeeded — libmxl loaded from /…/libmxl.so; /dev/shm tmpfs check = OK
```

If you see "probe found no libmxl.so", recheck Step 3.

### Step 5: Install an MXL test source (one-time)

For audio-in / video-in testing, install `mxl-gst-testsrc` from the upstream
build:

```bash
cd ~/Development/bilbycast/bilbycast-mxl-rs/vendor/mxl
cmake --preset Linux-Clang-Release -DBUILD_TOOLS=ON
cmake --build build/Linux-Clang-Release --target mxl-gst-testsrc
ls build/Linux-Clang-Release/tools/mxl-gst/mxl-gst-testsrc
```

This produces a CLI you can run alongside bilbycast-edge to publish a test
flow onto a shared-memory domain.

### Step 6: PTP setup (for any non-trivial gate testing)

```bash
sudo apt install -y linuxptp
sudo ptp4l -i eth0 -m -S          # against a hardware grandmaster
# OR for bench testing without a grandmaster:
sudo ptp4l -i eth0 -m -S --masterOnly=1
```

Not strictly needed for codec-bridge development — needed for gate execution.

---

## Part 2 — Prompt for the next Claude Code session

Paste everything below into a new Claude Code session in
`~/Development/bilbycast/`:

> Pick up the MXL (Media eXchange Layer) integration where the previous
> session left off. Memory file `project_mxl_integration_deferred.md` has
> the full current state — read it first.
>
> **Current state:** all 6 milestones shipped scaffolding-clean; ANC
> pass-through works end-to-end. The four codec-conversion bridges are
> the load-bearing remainder. Each is flagged in code with a clearly-
> named `error_code` Warning event the operator sees at flow start.
>
> **Work to do this session, in order:**
>
> 1. **Verify `AV_PIX_FMT_V210` support in `bilbycast-ffmpeg-video-rs`'s
>    `VideoScaler`.** Check `bilbycast-ffmpeg-video-rs/video-engine/src/`.
>    If V210 isn't a supported input/output pixel format, add it — should
>    be a small addition since libswscale already supports it natively.
>    Run `cargo check` in `bilbycast-ffmpeg-video-rs` after the change.
>
> 2. **Implement audio-in codec bridge** in
>    `bilbycast-edge/src/engine/mxl_io.rs::audio_input_blocking_loop`.
>    Replace the TODO block (~line 157) with:
>    - Copy `samples.channel_data(ch).0 + .1` (two slices for ring wrap)
>      into a planar `Vec<Vec<f32>>` buffer (interpret bytes as Float32 LE)
>    - If `config.transcode` set, run through
>      `engine::audio_transcode::PlanarAudioTranscoder`
>    - Encode per `config.audio_encode.codec`:
>      - `aac_lc` / `he_aac_v1` / `he_aac_v2` → `aac_audio::AacEncoder`
>      - `mp2` / `ac3` → `video_engine::AudioEncoder` (libavcodec)
>    - Mux to MPEG-TS via `engine::rtmp::ts_mux::TsMuxer`
>    - Generate PCR + PTS from the flow's master clock
>    - Wrap as `RtpPacket { is_raw_ts: true, ... }` and broadcast send
>    - Remove the `mxl_audio_no_encode_set` Warning emission when
>      `audio_encode` is set
>    - Mirror `engine::input_pcm_encode::spawn_pcm_encoder` very closely
>
> 3. **Implement video-in codec bridge** in
>    `bilbycast-edge/src/engine/mxl_video_io.rs::video_input_blocking_loop`.
>    Replace the TODO block (~line 135) with:
>    - V210 unpack via libswscale `AV_PIX_FMT_V210` →
>      `AV_PIX_FMT_YUV422P10LE` planar
>    - Feed planar 10-bit YUV to `VideoEncoder` per `config.video_encode`
>    - `TsMuxer` to produce MPEG-TS, generate PCR + PTS from master clock
>    - Wrap as `RtpPacket { is_raw_ts: true, ... }` and broadcast send
>    - Remove the `mxl_video_encode_pending` Warning emission
>    - Mirror `engine::st2110_video_io::run_st2110_20_input`
>
> 4. **Implement audio-out codec bridge** in
>    `bilbycast-edge/src/engine/mxl_io.rs::run_mxl_audio_output`.
>    Replace the `drain_until_cancel` placeholder with:
>    - On each broadcast `RtpPacket`: feed TS bytes to `engine::ts_demux::TsDemuxer`
>    - Extract audio PES per `config.transcode` / inferred from broadcast
>    - Decode to planar f32 via `aac_audio::AacDecoder` / `video_engine::AudioDecoder`
>    - Optionally transcode (channel/sample-rate adapt via `PlanarAudioTranscoder`)
>    - Write to `samples_writer.open_samples(index, count).channel_data_mut(ch)`
>    - `commit()`, advance `index` by `count`
>    - Remove the `mxl_audio_decode_pending` Warning emission
>
> 5. **Implement video-out codec bridge** in
>    `bilbycast-edge/src/engine/mxl_video_io.rs::run_mxl_video_output`.
>    - `TsDemuxer` → video PES → `VideoDecoder` to planar 10-bit YUV
>    - Scale to target resolution if needed
>    - Pack planar → V210 via libswscale `AV_PIX_FMT_V210` output
>    - `grain_writer.open_grain(index).payload_mut()` ← V210 bytes
>    - `commit(total_slices)`, advance `index` by 1
>    - Remove `mxl_video_decode_pending` Warning
>    - Mirror `engine::st2110_video_io::run_st2110_20_output`
>
> 6. **Verify**:
>    - `cargo check --features mxl` clean, zero warnings
>    - `cargo check` (no feature) clean, zero warnings
>    - Run `cargo build --features mxl` and confirm libmxl.so is reachable
>      via `BILBYCAST_LIBMXL_SO` env var (path from operator setup Step 3)
>    - Start `mxl-gst-testsrc` publishing a test flow
>    - Start `bilbycast-edge --config testbed/configs/mxl-loop-edge1.json`
>      configured to consume that flow
>    - Confirm broadcast packets actually flow (check stats or run a
>      downstream output to verify)
>
> 7. **Update memory file** `project_mxl_integration_deferred.md` to
>    reflect new state — codec bridges shipped, scaffold-mode Warning
>    events removed, gate execution is the open remainder.
>
> 8. **Don't commit yet.** Leave working tree uncommitted for review.
>
> **Reference files** (don't modify, read for pattern):
> - `engine/input_pcm_encode.rs` — audio-in mirror
> - `engine/st2110_video_io.rs` — video-in/out mirror
> - `engine/st2110_io.rs` — audio framing mirror
> - `engine/audio_transcode.rs` — transcoder API
> - `bilbycast-mxl-rs/vendor/mxl/lib/include/mxl/mxl.h` — C API reference
>
> **Don't do** in this session: actual broadcast-quality gate execution
> (needs hardware + a real receiver, separate workstream), manager UI
> gating (separate repo), release-matrix `mxl` entry (waits for gates).
>
> **Stop and ask if:** `AV_PIX_FMT_V210` turns out to need substantial
> work in `bilbycast-ffmpeg-video-rs`, or libmxl's flow_def JSON shape
> doesn't accept the audio_flow / video_flow / data_flow templates we
> wrote (would mean upstream changed the schema since v1.0.1).
