# Installing bilbycast-edge

Two release variants are published on each tag. Pick the one that
matches your transcoding needs:

| Variant         | H.264 / H.265 transcoding | Licence of binary            |
|-----------------|---------------------------|------------------------------|
| **`*-linux`**   | No (pass-through only)    | AGPL-3.0-or-later            |
| **`*-linux-full`** | Yes (libx264 + libx265 + NVENC) | AGPL-3.0-or-later combined work (bundles GPL-2.0+ libx264 / libx265) |

Both ship for Linux x86_64 (amd64) and Linux ARM64 (aarch64). See
[README.md](../README.md#choosing-a-release-binary) for the full
picture.

---

## Linux system prerequisites

What to install on a Linux host before running or building bilbycast-edge,
broken down by release variant and architecture. Tested on Ubuntu 24.04 LTS
(glibc 2.39+); equivalent packages exist on Debian 12+.

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

The full binary dynamically links libx264 + libx265. NVENC (NVIDIA) and QSV
(Intel) use `dlopen()` so they don't add build-link dependencies, but they
do require driver / runtime packages on the host.

**All architectures (x86_64 + aarch64) — software encoders:**

```bash
sudo apt-get update
sudo apt-get install -y libx264-dev libx265-dev libnuma1
```

> The `*-dev` metapackages depend on the matching runtime `.so` packages,
> so they cover both build and runtime use. If you want runtime-only
> packages, replace them with the versioned names on your release
> (`apt-cache search libx264` — e.g. `libx264-164`, `libx265-199` on
> Ubuntu 24.04).

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
                        libasound2-dev

# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

The full variant additionally needs the encoder development headers:

```bash
# All architectures: x264 + x265 + libnuma (x265's link-time dep)
sudo apt-get install -y libx264-dev libx265-dev libnuma-dev

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

**Full variant** — install the libx264 / libx265 runtime libraries
first (NVENC works automatically on NVIDIA hosts; no extra packages
required beyond the NVIDIA driver):

```bash
sudo apt update
sudo apt install libx264-dev libx265-dev

VER=vX.Y.Z
ARCH=x86_64   # or aarch64

curl -LO https://github.com/<org>/bilbycast-edge/releases/download/${VER}/bilbycast-edge-${VER#v}-${ARCH}-linux-full.tar.gz
tar xzf bilbycast-edge-${VER#v}-${ARCH}-linux-full.tar.gz
cd bilbycast-edge-${VER#v}-${ARCH}-linux-full
./bilbycast-edge --config /path/to/config.json
```

> **Why `-dev` packages for a runtime install?** The `-dev` metapackages
> depend on the matching runtime `.so` packages and guarantee you get
> the version the binary was built against. If you prefer runtime-only
> packages, check `apt-cache search libx264` for the specific versioned
> name on your Ubuntu release (e.g. `libx264-163` on 22.04,
> `libx264-164` on 24.04).

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

### Running on an NVIDIA host

The `*-full` binary includes NVENC support. At runtime, it
`dlopen`s `libnvidia-encode.so.1`, which ships with the NVIDIA
proprietary driver — you don't need to install anything beyond the
driver.

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

The `*-x86_64-linux-full` binary also includes Intel QuickSync
(QSV) via Intel oneVPL. The `*-aarch64-linux-full` binary does not
— Intel iGPUs are x86_64-only.

Install the runtime libraries and grant device access:

```bash
sudo apt install libvpl2 intel-media-va-driver-non-free
# (or `intel-media-driver` for the open-source upstream variant)
sudo usermod -aG render "$USER"
# log out + back in for the group change to take effect
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
                  make libssl-dev
```

Rust toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

For the full variant, also install the video encoder development
packages:

```bash
# All architectures: x264, x265, libnuma (x265's link-time dep)
sudo apt install libx264-dev libx265-dev libnuma-dev
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

# Full build — matches the `*-linux-full` release:
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
ldd bilbycast-edge | grep -E 'libx264|libx265'
# Hits → this is a `*-full` (AGPL+GPL combined) binary.
# No hits → this is a `*-default` (AGPL only) binary.
```

The `NOTICE` file inside each tarball contains the authoritative
bundled-library manifest.

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
