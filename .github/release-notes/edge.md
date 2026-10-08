Release of bilbycast-edge version {{VERSION}}.

## Changes

{{CHANGES}}

## Upgrading from v0.113.0 or earlier

<!-- Static text: remove once no supported upgrade starts at v0.113.0 or earlier. -->
Upgrade every edge that carries a **WebRTC WHIP output** first, then the relays and the edges with **WHIP inputs** it publishes into. A WHIP output on v0.113.0 or earlier panics on the answer a current relay or edge gives it and stops publishing until its flow restarts; a current WHIP output publishing into an older relay or edge works. See [supported-protocols.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/supported-protocols.md#webrtc-whipwhep).

## Binaries

Asset filenames carry no version, so `https://github.com/Bilbycast/bilbycast-edge/releases/latest/download/<asset>` always resolves to the latest release. Each tarball unpacks to a versioned directory, `bilbycast-edge-{{VERSION}}-<artefact>/`, where `<artefact>` is `x86_64-linux-full`, `aarch64-linux-full` or `aarch64-linux-rockchip`.

Each artefact carries the video encoders and hardware decoders its platform can use; the hardware decoders serve both the transcode input path and the local-display output. At start-up the edge probes which of them the host can actually open: NVENC and NVDEC with an NVIDIA driver, QSV (encode and decode) on an Intel iGPU with its media runtime, VAAPI where a VAAPI driver is installed (Mesa radeonsi for AMD, iHD for Intel), and RKMPP on Rockchip. x264 / x265 and libavcodec software decode cover everything else. Every artefact is an **AGPL-3.0-or-later combined work** that bundles GPL-2.0-or-later code — see `NOTICE` inside the tarball.

- `bilbycast-edge-x86_64-linux-full.tar.gz` — Linux x86_64 (amd64). Encoders: x264, x265, NVENC, QSV, VAAPI. Hardware decode: NVDEC, QSV, VAAPI.
- `bilbycast-edge-aarch64-linux-full.tar.gz` — Linux ARM64 (aarch64). Encoders: x264, x265, NVENC, VAAPI. Hardware decode: NVDEC, VAAPI. No QSV: Intel iGPUs are x86_64-only.
- `bilbycast-edge-aarch64-linux-rockchip.tar.gz` — Linux ARM64 for **Rockchip RK3568 / RK3588** boards (NanoPi R5S/R6S, Orange Pi 5, Radxa Rock 5B…). Encoders: **RKMPP** H.264 / HEVC (`h264_rkmpp` / `hevc_rkmpp`, 8-bit 4:2:0 only), with x264 / x265 as the CPU fallback for 10-bit and 4:2:2. Hardware decode: RKMPP, plus the **RGA** hardware frame copy on the local-display path. No NVENC, QSV or VAAPI. **`install-edge.sh` cannot install this artefact**: on a Rockchip board it installs `aarch64-linux-full`, which has no RKMPP and no RGA, so install this one by hand ([installation guide](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/installation.md), "ARM Rockchip SBCs"). Once a node runs it, remote upgrade keeps selecting it (upgrade variant `rockchip`).

**MXL** (Media eXchange Layer) support is compiled in. MXL is a shared-memory IPC protocol — the edge discovers the system-installed `libmxl.so` at runtime via `dlopen` and advertises MXL capabilities only when the library is present. Install MXL system-wide so all MXL-speaking applications on the host share the same library.

**Multiviewer** (mosaic compositor + stream head) is compiled into all three artefacts. A wall composites node-local inputs onto one canvas — up to 1920x1080 in this phase — and publishes it as an ordinary MPEG-TS flow source, so it restreams over SRT / RTP / UDP / WebRTC / CMAF, records, nests inside another wall and produces thumbnails with no extra output configuration. Unlike SDI and MXL there is nothing to install: every artefact already carries the encoder the canvas needs, so nodes running these binaries advertise the `mv-compositor` capability and the manager's wall surfaces appear automatically. Confirm with `bilbycast-edge --print-capabilities`. The canvas is encoded by whichever backend the **host** can actually open: the wall's `codec` (default `h264_auto`) resolves against the probed hardware, hardware first and CPU last, so QuickSync leads on an Intel host, NVENC on NVIDIA, VAAPI on AMD, RKMPP on RK3568/RK3588, and libx264 where there is no hardware encoder. Measured on an Intel Core Ultra 9 285HX: `h264_auto` → `["h264_qsv", "h264_vaapi", "x264"]`. Before v0.106.0 every wall was encoded on CPU libx264 whatever the host had, and the requested codec was ignored (edge #129).

{{SDI_NOTE}}

### Runtime requirements

The binaries need **glibc 2.39 or newer**: Ubuntu 24.04 or later, Debian 13 or later. They will not start on Ubuntu 22.04 or Debian 12.

libx264 and libx265 are **statically linked**, so no `libx264` / `libx265` package is needed and the binary is not tied to a distro's ABI-versioned `libx264.so.<build>` / `libx265.so.<build>`. The binary does link these system libraries, and will not start without them:

| Artefact | Libraries | Packages (Ubuntu / Debian) |
|---|---|---|
| `x86_64-linux-full` | `libva.so.2`, `libva-drm.so.2`, `libdrm.so.2`, `libvpl.so.2`, `libasound.so.2` | `libva2 libva-drm2 libdrm2 libvpl2 libasound2t64` |
| `aarch64-linux-full` | `libva.so.2`, `libva-drm.so.2`, `libdrm.so.2`, `libasound.so.2` | `libva2 libva-drm2 libdrm2 libasound2t64` |
| `aarch64-linux-rockchip` | `librockchip_mpp.so.1`, `librga.so.2`, `libdrm.so.2`, `libasound.so.2` | `libdrm2 libasound2t64`, plus the Rockchip BSP's MPP and RGA libraries |

Every artefact also links `libssl.so.3` (package `libssl3t64`), which a standard install already has. On x86_64, `libvpl2` is needed on every host, Intel or not.

Hardware backends need their drivers as well. Without them the edge starts normally and runs on CPU:

- **NVENC / NVDEC**: the NVIDIA driver. Under the packaged systemd unit, `/dev/nvidia-uvm` must also exist before the edge starts (`systemctl enable --now nvidia-persistenced`, or load `nvidia_uvm` at boot): the unit's `NoNewPrivileges=true` stops the driver creating it on first use.
- **VAAPI**: a driver behind `libva2` — `mesa-va-drivers` for AMD (radeonsi), `intel-media-va-driver` (iHD) for modern Intel. The service user must be in the `render` group to open `/dev/dri/renderD*`.
- **QSV** (x86_64): `libmfx-gen1.2`, the Intel GPU runtime that does the encoding and the package most often missing, plus `intel-media-va-driver-non-free` or `intel-media-va-driver`.
- **RKMPP / RGA** (Rockchip): a Rockchip BSP kernel exposing `/dev/mpp_service` and `/dev/rga`, with the service user in the `video` group. The packaged systemd unit already allows both devices; a custom unit needs `DeviceAllow=/dev/mpp_service rwm` and `DeviceAllow=/dev/rga rwm`.
{{SDI_RUNTIME}}

`install-edge.sh` installs the libraries in the table and, for the `-full` artefacts, `va-driver-all` plus `intel-media-va-driver` on x86_64. It does **not** install `libmfx-gen1.2` or the NVIDIA driver, and `va-driver-all` does not bring in the AMD driver on every release, so install `mesa-va-drivers` yourself on AMD. It adds the service user to the `video`, `render` and `audio` groups.

## Verify checksums
```bash
sha256sum -c *.sha256
```

## Licensing
bilbycast-edge is dual-licensed: AGPL-3.0-or-later for open-source use, commercial licence from Softside Tech Pty Ltd for OEM / SaaS / closed-source integration. See `LICENSE`, `LICENSE.commercial`, and the `NOTICE` file inside each tarball.

**Commercial-licence customers**: the Softside commercial licence covers bilbycast source only — it does not relicense libx264 / libx265, which remain GPL-2.0-or-later inside the binary. To avoid GPL copyleft entirely, build from source with `video-encoder-nvenc` only. See `LICENSE.commercial` for the scope statement.

**Patent notice**: H.264 / H.265 patent licensing (MPEG-LA, Access Advance, Velos Media) is separate from software copyright and is the operator's responsibility in commercial deployments. See `NOTICE` in the tarball.

## Remote upgrade

This release is signed for keyless verification by the edge's auto-upgrade pipeline. Operators using the manager UI's `Upgrade` button consume `manifest.json` + `manifest.sig.bundle` automatically. To verify a release manually:
```bash
cosign verify-blob \
    --bundle manifest.sig.bundle \
    --certificate-identity-regexp '^https://github\.com/Bilbycast/bilbycast-edge/\.github/workflows/nightly-release\.yml@refs/tags/v' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    manifest.json
```
See [docs/upgrade.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/upgrade.md) for the full trust model.
