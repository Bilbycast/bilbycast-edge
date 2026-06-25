# Installing bilbycast-edge

## Quick start (curl-pipe-bash)

For a fresh Linux node, the fastest path is the published installer
script. It downloads the latest signed release, verifies the Sigstore
signature, lays out `/opt/bilbycast/edge/`, creates the `bilbycast`
service user, installs the systemd unit, and registers the node with
your manager:

```bash
curl -fsSL https://github.com/Bilbycast/bilbycast-edge/releases/latest/download/install-edge.sh | \
    sudo bash -s -- \
        --manager wss://manager.example.com:8443 \
        --registration-token <token-from-manager-ui>
```

Optional flags: `--channel stable|nightly|beta`, `--variant default|full`,
`--output-nics <nic1,nic2>` (enable SO_TXTIME wire pacing), `--upgrade-installer`
(refresh the script + service unit without touching config or installed
binaries).

### Manager with a self-signed / untrusted TLS cert

The edge connects to the manager over `wss://` and validates its certificate
against the public CA roots. For a **lab or dev** manager using a self-signed
cert, pass `--accept-self-signed`:

```bash
curl -fsSL https://github.com/Bilbycast/bilbycast-edge/releases/latest/download/install-edge.sh | \
    sudo bash -s -- \
        --manager wss://manager.lab.internal:8443 \
        --registration-token <token-from-manager-ui> \
        --accept-self-signed
```

This writes both halves of the safety guard the edge requires —
`manager.accept_self_signed_cert = true` in `config.json` **and**
`BILBYCAST_ALLOW_INSECURE=1` in `/etc/bilbycast/edge.env` — so the node
registers without hand-patching after install.

> **Security:** `--accept-self-signed` disables **all** certificate validation,
> exposing the manager link to man-in-the-middle attacks. Use it only on
> trusted lab networks. For production behind a private CA, install the CA in
> the system trust store instead; to pin a specific cert, set
> `manager.cert_fingerprint` (SHA-256) — both keep validation on.

The installer's post-start check waits up to 60 s for the node to *register*
(not merely for the local HTTP server to answer). If the service is running but
registration hasn't completed in that window, it exits 0 with the manager-link
state and the likely cause (expired token, unreachable manager URL, or an
untrusted cert needing `--accept-self-signed`) rather than a misleading failure.

Once registered, the manager UI's per-node **Upgrade** button drives
future upgrades remotely — see [`docs/upgrade.md`](upgrade.md) for the
full lifecycle and trust model, and [`docs/security.md`](security.md)
for the threat model behind the Sigstore-keyless signing the installer
verifies.

The remainder of this document covers the manual install path
(air-gapped sites, sites that prefer to manage systemd themselves, and
operators who want to inspect every step before running it).

---

## Manual install

Two release variants are published on each tag. Pick the one that
matches your transcoding needs:

| Variant         | H.264 / H.265 transcoding | Licence of binary            |
|-----------------|---------------------------|------------------------------|
| **`*-linux`**   | No (pass-through only)    | AGPL-3.0-or-later            |
| **`*-linux-full`** | Yes (libx264 + libx265 + NVENC + VAAPI; QSV on x86_64 only — Intel iGPU is x86_64-only) | AGPL-3.0-or-later combined work (bundles GPL-2.0+ libx264 / libx265) |

Both ship for Linux x86_64 (amd64) and Linux ARM64 (aarch64). The
`install-edge.sh` script defaults to `--variant full` on Linux (full is
what most operators want); pass `--variant default` explicitly for the
AGPL-only pass-through binary. See
[README.md](../README.md#choosing-a-release-binary) for the full
picture.

---

## Linux system prerequisites

What to install on a Linux host before running or building bilbycast-edge,
broken down by release variant and architecture. Tested on Ubuntu 24.04 LTS
(glibc 2.39+); equivalent packages exist on Debian 12+.

> **Kernel tuning:** the installer drops
> `/etc/sysctl.d/90-bilbycast-edge.conf` (from
> `packaging/90-bilbycast-edge.conf`) setting
> `net.core.rmem_max = 67108864` — the `SO_RCVBUF` ceiling the
> ST 2110-20 ingest needs for burst headroom at uncompressed-video
> rates (distro defaults clamp it to 4 MB ≈ 8 ms at 2160p50). Manual
> installs should apply the same file; see
> [st2110.md → Host kernel tuning](st2110.md#host-kernel-tuning-receive-side).

### Default variant (`*-linux`) — runtime

The default binary statically bundles SRT, AAC (fdk-aac), and the FFmpeg
video decoder used for thumbnails. It also includes the **local-display
output** (`display` Cargo feature, HDMI / DisplayPort + ALSA confidence
monitor playout), which dynamically links libasound2 at runtime
(`drm-rs` is pure-Rust ioctls over `/dev/dri/cardN`, and connector
enumeration walks `/sys/class/drm` via std::fs — neither libdrm nor
libudev are in the link graph):

```bash
sudo apt-get update
# Ubuntu 24.04+: the package was renamed to libasound2t64 (time_t
# transition). On Ubuntu 22.04 / Debian 12 / older the name is plain
# libasound2.
sudo apt-get install -y libasound2t64 || sudo apt-get install -y libasound2
```

This library is part of every modern Linux base install and already
present on Ubuntu / Debian / Fedora / Arch by default. On a strictly
headless server with no `/dev/dri/cardN` it causes no runtime
side-effects — `enumerate_displays()` returns empty, the `"display"`
capability isn't advertised on the WS heartbeat, and the manager UI
hides the option per-node. To run a confidence monitor, plug an HDMI /
DisplayPort cable into the box and configure a `display` output via
the manager.

### Full variant (`*-linux-full`) — runtime

The full binary **statically links libx264 + libx265** (the software
H.264 / HEVC encoders), so it needs **no `libx264` / `libx265` runtime
package** and is *not* tied to any distro's ABI-versioned
`libx264.so.<build>` / `libx265.so.<build>` SONAME. Those SONAMEs are the
upstream build number and bump on every distro release — Ubuntu 24.04
ships `libx264.so.164` / `libx265.so.199`, 26.04 ships `.165` / `.215`,
which are ABI-incompatible (x264 even bakes the build number into its
exported symbol names, so a soname symlink fails too) — so a
dynamically-linked build only runs on the exact release it was built on.
Static linking removes that fragility entirely.

NVENC (NVIDIA) and NVDEC `dlopen()` the proprietary driver at runtime, so
they add no link-time dependency either.

The libraries that **do** remain dynamically linked all have **stable
major SONAMEs** (`.2`) that don't churn across distro releases, so the
plain package name resolves on Ubuntu 22.04 → 26.04+:

**All architectures — VAAPI HW encode/decode + the display output:**

```bash
sudo apt-get update
# libasound2: Ubuntu 24.04+ renamed it libasound2t64 (time_t transition).
sudo apt-get install -y libva2 libva-drm2 libasound2t64 \
  || sudo apt-get install -y libva2 libva-drm2 libasound2
```

> The `curl … | install-edge.sh` bundle installs these automatically for
> the variant it lays down; the list above is for plain tarball installs.
> A working VAAPI *driver* (`mesa-va-drivers` for AMD, `intel-media-va-driver`
> for Intel) is still needed to actually *use* VAAPI — without it the
> runtime probe just skips the backend; the binary still starts.

**x86_64 only — Intel QuickSync (QSV):**

```bash
sudo apt-get install -y libvpl2 intel-media-va-driver-non-free
# (or `intel-media-driver` for the open-source upstream variant)
sudo usermod -aG render "$USER"
# log out + back in for the group change to take effect
```

QSV needs a 5th-gen (Broadwell) Intel Core CPU or newer for H.264; HEVC
needs 7th-gen (Kaby Lake) or newer.

**Both architectures — NVIDIA NVENC:**

No apt packages required. The binary `dlopen`s `libnvidia-encode.so.1`,
which ships with the proprietary NVIDIA driver. Install the driver via
your distribution's standard mechanism (e.g. `nvidia-driver-550` on
Ubuntu) and reboot once.

### Building from source — additional prerequisites

In addition to whichever runtime set above matches your variant, building
from source needs the base toolchain plus Rust. The default build also
needs the local-display dev header (`libasound2-dev`) since the
`display` feature is on by default in every release variant:

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake make clang \
                        libclang-dev pkg-config libssl-dev g++ \
                        libasound2-dev nasm

# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

The full variant additionally needs the encoder development headers. The
build **statically links libx264 + libx265** (so the resulting binary has
no `libx264.so` / `libx265.so` runtime dependency):

```bash
# x265: libx265-dev ships libx265.a (multilib 8/10/12-bit) — statically
# linked directly. libnuma-dev satisfies x265's `-lnuma` at static link.
sudo apt-get install -y libx265-dev libnuma-dev

# x264: Debian/Ubuntu's libx264-dev ships only libx264.so (no .a), so build
# a static libx264.a from source and put its prefix on PKG_CONFIG_PATH.
# build.rs prefers a static libx264.a; if none is found it falls back to a
# DYNAMIC link and prints a loud warning (the binary then won't run on a
# distro with a different x264 build — see "Full variant runtime" above).
git clone --depth 1 https://code.videolan.org/videolan/x264.git /tmp/x264
( cd /tmp/x264 && ./configure --prefix="$HOME/x264-static" \
    --enable-static --enable-pic --disable-cli && make -j"$(nproc)" && make install )
export PKG_CONFIG_PATH="$HOME/x264-static/lib/pkgconfig:$PKG_CONFIG_PATH"

# x86_64 only: Intel oneVPL (QSV)
sudo apt-get install -y libvpl-dev

# NVENC headers — Ubuntu 24.04 doesn't package nv-codec-headers, so
# install from source pinned to the FFmpeg 7.1.x-compatible tag:
git clone --depth 1 --branch n12.2.72.0 \
  https://github.com/FFmpeg/nv-codec-headers.git /tmp/nv-codec-headers
sudo make -C /tmp/nv-codec-headers PREFIX=/usr/local install
```

This matches the package set the GitHub Actions release workflow uses to
build `*-linux` and `*-linux-full` artefacts.

---

## Running a pre-built binary

### Ubuntu 22.04+ / Debian 12+ (x86_64 or aarch64)

Replace `vX.Y.Z`, `<org>` and the architecture suffix below to match
the GitHub Release you want (`x86_64` or `aarch64`).

**Default variant** — no runtime codec libraries needed beyond
glibc:

```bash
VER=vX.Y.Z
ARCH=x86_64   # or aarch64

curl -LO https://github.com/<org>/bilbycast-edge/releases/download/${VER}/bilbycast-edge-${VER#v}-${ARCH}-linux.tar.gz
tar xzf bilbycast-edge-${VER#v}-${ARCH}-linux.tar.gz
cd bilbycast-edge-${VER#v}-${ARCH}-linux
./bilbycast-edge --config /path/to/config.json
```

**Full variant** — libx264 / libx265 are statically linked, so **no
codec runtime packages are required**. Install the remaining
stable-SONAME runtime libraries (HW-accel + display); NVENC works
automatically on NVIDIA hosts with just the driver:

```bash
sudo apt update
sudo apt install libva2 libva-drm2 libasound2t64 \
  || sudo apt install libva2 libva-drm2 libasound2
sudo apt install libvpl2     # x86_64 only — Intel QSV

VER=vX.Y.Z
ARCH=x86_64   # or aarch64

curl -LO https://github.com/<org>/bilbycast-edge/releases/download/${VER}/bilbycast-edge-${VER#v}-${ARCH}-linux-full.tar.gz
tar xzf bilbycast-edge-${VER#v}-${ARCH}-linux-full.tar.gz
cd bilbycast-edge-${VER#v}-${ARCH}-linux-full
./bilbycast-edge --config /path/to/config.json
```

> **Ubuntu 26.04+ note.** Earlier releases dynamically linked libx264 /
> libx265, which broke on each new Ubuntu LTS because the ABI-versioned
> `libx264.so.<build>` / `libx265.so.<build>` SONAME bumps every release
> (24.04 `.164` / `.199` → 26.04 `.165` / `.215`, ABI-incompatible). The
> current full binary statically links both, so this class of failure is
> gone — you no longer need to hunt for a matching `libx264-NNN` package
> or pull an older one from a previous release's pool. The packages above
> all carry stable `.2` SONAMEs available on every supported Ubuntu /
> Debian release.

Verify checksums:

```bash
sha256sum -c bilbycast-edge-${VER#v}-${ARCH}-linux-full.tar.gz.sha256
```

### Other Linux distributions

The binaries are built on Ubuntu 24.04 and require glibc 2.39+. For
older distributions (CentOS 7/8, Ubuntu 20.04), build from source
— see below.

### macOS / Windows

Pre-built binaries are not yet provided for these platforms. Build
from source.

### Running on an NVIDIA host (NVENC)

The `*-full` binary compiles in the FFmpeg → NVENC bridge. At runtime
it `dlopen`s **`libnvidia-encode.so.1`** and **`libcuda.so.1`**, both
of which ship inside the NVIDIA proprietary driver — there is **no
separate package** for the encoder itself.

#### What you need installed

| Component | Package | Why |
|---|---|---|
| NVIDIA proprietary driver | `nvidia-driver-XXX` (Ubuntu) or `nvidia-driver` (Debian non-free) | Ships `libnvidia-encode.so.1`, `libcuda.so.1`, the GPU kernel module |
| User device access | membership in the `video` group (auto-handled by the driver) | `/dev/nvidia*` permissions |

The Nouveau open-source driver does **not** support NVENC — only the
proprietary driver exposes the NVENC engine.

```bash
# Ubuntu 22.04 / 24.04 — install the recommended driver branch
sudo ubuntu-drivers autoinstall
# or pin a specific branch:
sudo apt install nvidia-driver-580        # desktop / workstation
sudo apt install nvidia-driver-580-server # headless servers
sudo reboot                                # required after first install

# Debian 12+ — non-free repo must be enabled
sudo apt install nvidia-driver
sudo reboot
```

Verify the install reached userspace:

```bash
nvidia-smi                                          # should list the GPU
ldconfig -p | grep -E 'libnvidia-encode|libcuda\.'  # both should appear
```

To use NVENC in a flow config, set `codec: "h264_nvenc"` or
`"hevc_nvenc"` on the `video_encode` block:

```json
"video_encode": {
  "codec": "h264_nvenc",
  "bitrate_kbps": 4000,
  "preset": "medium"
}
```

On a host without an NVIDIA driver, select `codec: "x264"` or
`"x265"` instead. The same binary supports every compiled-in
encoder; the choice is per-flow.

### Running on an Intel host (QuickSync / QSV)

The `*-x86_64-linux-full` binary also compiles in the FFmpeg →
oneVPL bridge for Intel QuickSync — covering both the QSV **encoder**
(per-flow `codec: "h264_qsv"` / `"hevc_qsv"`) and the QSV
**decoder** (the local-display output's `hw_decode: "qsv"` path).
The `*-aarch64-linux-full` binary does **not** include QSV — Intel
iGPUs are x86_64-only.

#### What you need installed

oneVPL is a thin **dispatcher** library — it contains zero GPU
encoding code. It `dlopen`s an Intel-shipped *backend* runtime at
session create. Without a backend installed, the dispatcher returns
`MFX_ERR_NOT_FOUND` (-9) and the encoder fails to open. You need
**all three** of the following on the host:

| Component | Package | Why |
|---|---|---|
| oneVPL dispatcher | `libvpl2` | Implements `MFXLoad`. Linked dynamically by the binary. |
| Intel VPL GPU runtime | **`libmfx-gen1.2`** | The actual hardware encoder backend (`libmfx-gen.so.1.2`). Without this, MFX has nothing to dispatch to. **This is the most-commonly-missed package.** |
| Intel media VAAPI driver | `intel-media-va-driver-non-free` (or `intel-media-va-driver` for the open-source build) | Provides `iHD_drv_video.so` for the VAAPI fallback path that `libmfx-gen` falls back on for some pixel-format conversions and zero-copy paths. |

```bash
sudo apt update
sudo apt install libvpl2 libmfx-gen1.2 intel-media-va-driver-non-free
sudo usermod -aG render "$USER"
# log out + back in for the group change to take effect
```

Verify:

```bash
ls /usr/lib/x86_64-linux-gnu/libmfx-gen.so.1.2   # must exist
ls /usr/lib/x86_64-linux-gnu/dri/iHD_drv_video.so # must exist
ls /dev/dri/                                      # should list card* + renderD*
vainfo 2>/dev/null | head -20                     # optional: confirms VAAPI sees the iGPU
```

To use QSV in a flow config, set `codec: "h264_qsv"` or
`"hevc_qsv"` on the `video_encode` block:

```json
"video_encode": {
  "codec": "h264_qsv",
  "bitrate_kbps": 4000,
  "preset": "medium"
}
```

QSV requires a 5th-gen (Broadwell) or newer Intel Core CPU for
H.264; HEVC requires 7th-gen (Kaby Lake) or newer. On a host
without an Intel iGPU + media driver, select `codec: "x264"` /
`"x265"` (or `"h264_nvenc"` / `"hevc_nvenc"` if NVIDIA is also
present) instead.

#### Why the runtime libraries are mandatory

Both NVENC and QSV are **dispatcher architectures**: the encoder
implementation that actually programs the GPU lives in vendor-shipped
runtime libraries (`libnvidia-encode.so.1` for NVENC,
`libmfx-gen.so.1.2` for QSV). bilbycast cannot statically link them
in — they are GPU-architecture-specific binaries that Intel and
NVIDIA distribute as part of their driver stacks. This is the same
model OBS, GStreamer, FFmpeg, and every other QSV/NVENC consumer
follows.

If you skip the runtime install, hardware encoding fails at session
creation; CPU encoding (`x264` / `x265`) continues to work because
those libraries are statically linked into the `*-full` binary.

---

## Wire pacing

PCR-anchored / PTP-raster-anchored wire emission runs on every output
that owns a UDP socket directly: UDP, RTP (single-leg, FEC, 2022-7
dual-leg), 302M, ST 2110-20/-23/-30/-31/-40. SRT, RIST, RTMP, HLS,
CMAF, and WebRTC are unchanged — they have their own protocol-layer
pacing.

**The default release tier is `clock_nanosleep` on a SCHED_FIFO thread
(tier 4 below) — no qdisc, no PTP, no HW-PTP NIC, no env var required.**
That path handles compressed TS through at least 2 Gbps with sub-3 ms
PCR_AC max on commodity Linux. The kernel-paced **`SO_TXTIME` + ETF
qdisc** path (tiers 1–2) is opt-in via `BILBYCAST_ENABLE_TXTIME=1` and
only worth enabling for ST 2110-21 narrow profile, T-STD-strict
contribution receivers (Appear X10 / Cobalt 9202 / Cisco D9824 with
`PCR_AC` alarms enabled), or sustained CPU contention pushing tier-4
p99 above ~30 ms.

| Tier | Mechanism | Inter-packet jitter | Requires |
|---|---|---|---|
| 1 | SO_TXTIME + ETF qdisc + NIC HW offload | Sub-µs | Linux ≥ 4.19, ETF qdisc, PTP-disciplined HW-PTP NIC, `CAP_NET_ADMIN`, `BILBYCAST_ENABLE_TXTIME=1` |
| 2 | SO_TXTIME + software ETF qdisc | ~1–10 µs | Linux ≥ 4.19, ETF qdisc, `CAP_NET_ADMIN`, `BILBYCAST_ENABLE_TXTIME=1` |
| 4 ⭐ | `clock_nanosleep` on SCHED_FIFO | ~50–500 µs typical, ms-tail under load | systemd unit (`LimitRTPRIO=99`). **Default — no setup required.** |
| 5 | `clock_nanosleep` on SCHED_OTHER | ~1–5 ms | None — non-Linux, or Linux without the SCHED_FIFO grant |

Tier is logged on every output startup
(`wire-emit '<id>': starting (anchor=..., tier=...)`)
and surfaced on `OutputStats.wire_pacing_tier`.

The pre-2026-05-16 default of "try SO_TXTIME first, fall back to
clock_nanosleep" was reverted. On a host without ETF qdisc the kernel
accepts the `SO_TXTIME` setsockopt and the `SCM_TXTIME` cmsg silently
but emits each packet immediately on `sendmsg`, so the wire-emit thread
degenerated into a producer-paced loop while telemetry still reported
tier `so_txtime`. The inverted default removes that silent-degradation
trap.

### Enabling the SO_TXTIME tier (opt-in, four steps)

Only do this if you have one of the cases above. Enabling SO_TXTIME
without the full prerequisite stack produces *silent degradation*
worse than the default.

**Step 1: install the ETF qdisc on the egress NIC**

```bash
sudo bash packaging/setup-etf-qdisc.sh enp1s0   # name your egress NIC
```

The script installs `mqprio` at root + `etf clockid CLOCK_TAI delta
200000 offload skip_sock_check on` on the prioritized class. `offload`
enables NIC HW pacing on supported NICs (Mellanox CX-6/CX-7, Intel
E810, Intel i210); falls back to software ETF (~1–10 µs jitter) on
unsupported ones. `skip_sock_check on` is non-negotiable — without it
ARP / DHCP / ssh / every default UDP socket on the host gets dropped
at the qdisc.

Verify:

```bash
tc -s qdisc show dev enp1s0    # look for "etf" in the output
```

**Step 2: persist the qdisc across reboots via the shipped systemd
unit**

The one-shot `tc` call doesn't survive a reboot. Install the templated
`bilbycast-etf-qdisc@.service` so the qdisc lands at boot before the
edge starts:

```bash
sudo install -m 0644 packaging/bilbycast-etf-qdisc@.service \
    /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now bilbycast-etf-qdisc@enp1s0
```

Removal: `sudo systemctl disable --now bilbycast-etf-qdisc@enp1s0`.

**Step 3: PTP for tier-1 (ST 2110-21 + multi-edge coherence)**

Tier 1 requires `ptp4l` + `phc2sys` against a PTP grandmaster on a
HW-PTP NIC. Tier 2 (software ETF, no HW-PTP NIC, no PTP) caps out
around 1–10 µs jitter and works without this step.

**No `sudo systemctl ptp4l@…` commands needed in normal operation.**
`install-edge.sh` provisions a `bilbycast-ptp-helper` daemon with the
right capabilities; operators flip the node between PTP roles (Auto /
Grandmaster / Slave-only / Off) from the manager UI's per-node
**Time** page (`/nodes/{id}/time`) or by hand-editing
`/var/lib/bilbycast/ptp.conf`. Full operator runbook + security
analysis: [`ptp.md`](ptp.md).

The only thing the manual `apt` install gets you here is the
`linuxptp` packages the helper execs:

```bash
sudo apt install linuxptp
```

Then pick **Slave only** on the Time page and point it at your
grandmaster's interface. The helper applies the config within ~1 s
of the file mtime advancing. Use `phc2sys -w` (the helper's default)
not `-O 0`.

**Step 4: opt the edge in to SO_TXTIME**

Set `BILBYCAST_ENABLE_TXTIME=1` in the edge's environment. Production
via the env file consumed by the systemd unit:

```bash
# /etc/bilbycast/edge.env
BILBYCAST_ENABLE_TXTIME=1
```

Then `sudo systemctl restart bilbycast-edge`. The startup log should
now show `wire-emit '<id>': starting (anchor=Pcr, tier=so_txtime)`.
If it shows `tier=clock_nanosleep` instead, the setsockopt probe
failed — typically because `CAP_NET_ADMIN` isn't granted. The shipped
`packaging/bilbycast-edge.service` already includes the cap; standalone
runs need `sudo setcap cap_net_admin,cap_sys_nice+ep <binary>`.

### SCHED_FIFO grant (tier 4 default)

The shipped `packaging/bilbycast-edge.service` already includes
`RestrictRealtime=false` + `LimitRTPRIO=99`. The kernel allows
unprivileged `SCHED_FIFO` whenever the requested priority is at or
below `RLIMIT_RTPRIO`, so no capability grant is required when
running under systemd — this is what gives the default tier its
~50–500 µs jitter envelope. For `cargo run` / dev binary:

```bash
sudo setcap cap_sys_nice+ep target/debug/bilbycast-edge
# or run as root
```

Verify:

```bash
ps -eLo pid,tid,class,rtprio,comm | grep bilbycast-edge
# wire-emit-* threads should show class=FF, rtprio=50
```

Without the grant, threads run at `SCHED_OTHER` (tier 5) and a
debug-level log line shows the failure. Pacing still works at coarser
jitter (~1–5 ms typical), still inside the broadcast tier-2 envelope
on a lightly-loaded box.

For the full reference — anchor strategies, per-codec PCR_AC
expectations, FEC + 2022-7 dual-leg integration, the decision matrix
for when ETF actually earns its keep, and the per-symptom diagnostic
table — see [`wire-pacing.md`](wire-pacing.md).

---

## Securing the setup wizard

A fresh edge defaults to `setup_enabled: true` and exposes
`POST /setup` until the first successful manager registration
(after which the flag flips to `false` and the wizard returns
the disabled-page).

On first boot the node auto-generates a one-shot bearer token,
persists it (encrypted) into `secrets.json`, and prints it once
to **stdout** — look for the banner:

```
=== bilbycast-edge first-boot setup token ===
<64-hex-character token>
...
=================================================
```

The token is required for any `POST /setup` arriving from a
**non-loopback** address (LAN or internet). Loopback callers
(`http://localhost:<port>/setup`, `127.0.0.1`, `[::1]`) bypass
the check — an operator who is already on the box has
authenticated by other means (SSH, local console).

Re-print the token any time before registration:

```bash
bilbycast-edge --config /etc/bilbycast/edge.json --print-setup-token
```

Use it from a remote browser by pasting it into the wizard's
**Setup Token** field. From the command line:

```bash
curl -k -X POST https://<node>:8443/setup \
     -H "Authorization: Bearer <printed-token>" \
     -H "Content-Type: application/json" \
     -d '{ "manager_urls": ["wss://manager.example.com:8443/ws/node"], … }'
```

The token is cleared automatically once the node successfully
registers with the manager; `/setup` itself starts returning
the disabled-wizard page from that point on.

**Known limitation:** the bypass classifies the request by its
TCP source IP. Reverse proxies / iptables NAT rules that rewrite
remote requests so they arrive on `127.0.0.1` will defeat the
gate. Avoid such setups in front of `/setup` on internet-facing
nodes.

---

## Building from source

### Linux (Ubuntu / Debian)

Common build-time dependencies:

```bash
sudo apt update
sudo apt install build-essential pkg-config cmake clang libclang-dev \
                  make libssl-dev nasm
```

Rust toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

For the full variant, also install the video encoder development
packages. libx264 + libx265 are **statically linked** (no runtime
`.so` dependency on the resulting binary):

```bash
# x265: libx265-dev ships libx265.a; libnuma-dev covers x265's `-lnuma`.
sudo apt install libx265-dev libnuma-dev
# x264: libx264-dev has no static .a on Debian/Ubuntu — build one from
# source and expose it on PKG_CONFIG_PATH (else build.rs warns and falls
# back to a non-portable dynamic link):
git clone --depth 1 https://code.videolan.org/videolan/x264.git /tmp/x264
( cd /tmp/x264 && ./configure --prefix="$HOME/x264-static" \
    --enable-static --enable-pic --disable-cli && make -j"$(nproc)" && make install )
export PKG_CONFIG_PATH="$HOME/x264-static/lib/pkgconfig:$PKG_CONFIG_PATH"
# x86_64 only: Intel oneVPL (QSV)
sudo apt install libvpl-dev

# NVENC headers — Ubuntu 24.04 doesn't ship nv-codec-headers as a package;
# install from source, pinned to the FFmpeg 7.1.x-compatible tag:
git clone --depth 1 --branch n12.2.72.0 \
  https://github.com/FFmpeg/nv-codec-headers.git /tmp/nv-codec-headers
sudo make -C /tmp/nv-codec-headers PREFIX=/usr/local install
```

Clone the repositories (bilbycast-edge plus its path-dependency
siblings) and build:

```bash
git clone https://github.com/<org>/bilbycast-edge.git
git clone https://github.com/<org>/bilbycast-libsrt-rs.git
git clone https://github.com/<org>/bilbycast-fdk-aac-rs.git --recurse-submodules
git clone https://github.com/<org>/bilbycast-ffmpeg-video-rs.git --recurse-submodules
git clone https://github.com/<org>/bilbycast-rist.git

cd bilbycast-edge
# Default build (no software video encoders) — matches the `*-linux` release:
cargo build --release

# Full build — matches the `*-linux-full` release. Bundles every
# video codec backend the edge knows about: encoders (x264 + x265 +
# NVENC + QSV) and HW decoders for the display output (NVDEC +
# QSV-decode). The runtime probe activates only the backends the host
# can actually open.
cargo build --release --features video-encoders-full
```

### macOS (development host only)

Dependencies:

```bash
xcode-select --install
brew install cmake pkg-config
# For the full variant (x264 / x265 only — no NVENC on macOS):
brew install x264 x265
```

Install the Rust toolchain the same way as Linux, then:

```bash
cargo build --release                                     # default
cargo build --release --features video-encoder-x264,video-encoder-x265   # full (no NVENC on mac)
```

NVENC is Linux-only. macOS hardware transcoding via VideoToolbox is
not yet wired — tracked as a future work item in
[docs/transcoding.md](transcoding.md).

### Windows

Pre-built libx264 / libx265 via MSYS2 + `pacman -S
mingw-w64-x86_64-x264 mingw-w64-x86_64-x265`, or use vcpkg. Detailed
Windows build documentation is out of scope for this release.

### Local-display output (`display` feature, Linux only)

The `display` feature lets a flow render its decoded video + audio to
a physical Linux KMS connector (HDMI / DisplayPort) plus an ALSA
audio device — a confidence monitor at the stadium / OB truck / MCR
without an external decoder appliance.

**Included by default in every Linux release variant** (`*-linux` and
`*-linux-full`, both x86_64 and aarch64). The runtime library
(`libasound2`) ships with every modern Linux base install (`drm-rs`
is pure-Rust ioctls and connector enumeration uses `/sys/class/drm`
directly, so neither libdrm nor libudev are in the link graph). On
a headless host the feature stays dormant — `enumerate_displays()`
returns empty, the `"display"` capability isn't advertised, and the
manager UI hides the option per-node. So one binary works for both
confidence-monitor and headless deployments.

Build from source (Debian / Ubuntu):

```sh
sudo apt install libasound2-dev
cargo build --release           # display is on by default in release
```

**Device permissions.** The display output opens `/dev/dri/*` (KMS +
VAAPI) and `/dev/snd/*` (ALSA), which are group-gated on every Linux
distribution (`video` / `render` / `audio`). The packaged service
install handles this — `bilbycast-edge.service` carries
`SupplementaryGroups=video render audio` plus class-based
`DeviceAllow=` entries, and `install-edge.sh` adds the `bilbycast`
user to all three groups on every run. **If you run the binary
manually** (dev box, testbed, custom unit), the running user needs
the same membership:

```sh
sudo usermod -aG video,render,audio <user>   # takes effect on next login
```

Without `audio`, the failure is deceptive: desktop logins often work
anyway because systemd-logind grants a temporary ACL on `/dev/snd`
to the active local seat — which vanishes on reboot or when only SSH
sessions exist. Video keeps rendering (DRM is a separate path) while
every ALSA open fails, and alsa-lib reports the misleading
`ALSA lib confmisc.c: (snd_config_get_card) Cannot get card index for N`
— that is EACCES on `/dev/snd/controlCN`, not a missing sound card.
The edge raises Critical `display_audio_open_failed` events and, after
10 consecutive failures, `display_audio_disabled_persistent_failure`
(see the event catalogue).

The schema is unconditional on every host (configs round-trip
cleanly), but the runtime spawner is `cfg(all(feature = "display",
target_os = "linux"))`. macOS dev builds reject `display` outputs at
`start_output()` with `display_device_invalid`, so the manager UI
surfaces the offending field clearly.

`display-vaapi` / `display-nvdec` are placeholders for v2 hardware
decode and currently fail at compile time (so the manager UI can
advertise the future feature without us shipping unfinished code).

Configuration reference:
[`docs/configuration-guide.md`](configuration-guide.md#display-output-hdmi--displayport--alsa).
Event catalogue:
[`docs/events-and-alarms.md`](events-and-alarms.md#display-output-events-display).

---

## Verifying the licence profile of a binary

Knowing which variant you have matters for redistribution — the
`*-full` binary is an AGPL+GPL combined work, the default binary is
AGPL-only.

```bash
file bilbycast-edge
# libx264/libx265 are STATICALLY linked into the full variant, so `ldd`
# can no longer tell the variants apart (the full binary has no
# libx264.so/libx265.so dependency — that's the whole point). Check for the
# baked-in encoder symbols instead:
nm bilbycast-edge 2>/dev/null | grep -qE ' [Tt] x26[45]_encoder_open' \
  && echo 'full (AGPL+GPL combined — libx264/libx265 statically linked in)' \
  || echo 'default (AGPL only — no software video encoders)'
# (On a stripped binary `nm` shows nothing — fall back to the bundled NOTICE.)
```

The `NOTICE` file inside each tarball is the authoritative bundled-library
manifest: the full variant ships `NOTICE.full` (lists libx264 / libx265 +
the GPL terms), the default variant ships the AGPL-only `NOTICE`.

---

## Licensing summary

- Source code: [AGPL-3.0-or-later](../LICENSE).
- Commercial licence for OEMs / SaaS integrators: see
  [LICENSE.commercial](../LICENSE.commercial) — contact
  `contact@bilbycast.com`.
- **Commercial licence scope**: the Softside commercial licence
  covers the bilbycast source code only. It does not relicense
  libx264 / libx265, which remain GPL-2.0-or-later inside the
  `*-full` variant regardless of what commercial licence you hold
  for bilbycast. Commercial deployments that need to avoid GPL
  copyleft entirely should use the default (`*-linux`) variant or
  build with `--features video-encoder-nvenc` and/or `--features
  video-encoder-qsv` only (NVENC's and QSV's API layers are both
  LGPL-compatible; NVIDIA / Intel cover the H.264 / H.265 patent
  pools at the hardware / driver layer).
- H.264 / H.265 patent licensing (MPEG-LA, Access Advance, Velos
  Media) is **your responsibility** in commercial deployments,
  regardless of which variant you choose. NVENC on NVIDIA hardware
  and QSV on Intel hardware are often the lowest-friction paths
  because NVIDIA / Intel pay the pools at the hardware layer.
