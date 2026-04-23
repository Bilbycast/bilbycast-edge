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
`"x265"` instead. The same binary supports all three encoders; the
choice is per-flow.

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
sudo apt install libx264-dev libx265-dev nv-codec-headers
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
  build with `--features video-encoder-nvenc` (NVENC's API layer is
  LGPL-compatible; NVIDIA covers the H.264/H.265 patent pools at
  the hardware/driver layer).
- H.264 / H.265 patent licensing (MPEG-LA, Access Advance, Velos
  Media) is **your responsibility** in commercial deployments,
  regardless of which variant you choose. NVENC on NVIDIA hardware
  is often the lowest-friction path because NVIDIA pays the pools
  at the hardware layer.
