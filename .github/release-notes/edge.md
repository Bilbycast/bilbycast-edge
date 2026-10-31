Release of bilbycast-edge version {{VERSION}}.

## Changes

{{CHANGES}}

## Upgrading from v0.116.0 or earlier

<!-- Static text: remove once no supported upgrade starts at v0.116.0 or earlier. -->
- **Upgrade every edge with an RTP input carrying `fec_decode` before any edge with an RTP output carrying `fec_encode`.** SMPTE 2022-1 FEC from an RTP output now protects the datagrams it sends; before this release it protected almost nothing outside a 2022-7 output forwarding RTP, so receivers rarely rebuilt a packet. An older single-leg receiver fed by an upgraded sender publishes each rebuilt packet as soon as it has it, after newer ones: continuity errors, and a PCR that steps back. An upgraded single-leg receiver publishes only repairs that are in order. A 2022-7 input without `path_differential_ms` publishes repairs as they come on any version, so give it one. An older sender feeding an upgraded receiver behaves as before. See [configuration-guide.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/configuration-guide.md#smpte-2022-1-fec-configuration).
- **FEC covers at most 8 columns and 8 rows.** The FEC header names a column or row in 3 bits, so an output configured with `columns` or `rows` above 8 sends no FEC for columns or rows from 8 up (a packet in both is unprotected) and logs a warning at start (except `columns: 1`, which sends no row FEC and so loses nothing). `columns: 1` now sends column FEC only. `8 x 8` is the largest shape every column and row of which is protected.
- **New Warning `egress_pcr_accuracy_exceeded` on UDP and RTP outputs that re-encode video.** Such an output stays on `forward` egress pacing under auto, so it sends packets as its encoder delivers them. Running outputs whose PCR send jitter (`pcr_trust` p99) is above 30 ms start raising this Warning a few minutes after the upgrade. On the test rig that was most video transcodes. Nothing changes on the wire. The message names `egress_pacing: "pcr"` as the fix and its latency cost: about 50 ms, rising to the longest encoder stall it absorbs. Switching restarts the output, and its encoder's open counts as such a stall — most of a second on some hardware encoders. Past about 1 s the egress residence cap sheds packets instead (`egress_shed`). A lighter encode keeps the latency down. An output pinned to `forward` raises it too, and can only acknowledge it. Check that the receiver copes with `pcr` pacing before relying on it. See [configuration-guide.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/configuration-guide.md#outputs-that-re-encode-video).
- **SRT socket-group bonding applies each member's source and weight.** Before this release `local_addr`, `interface_binding` and `weight` on bonding endpoints were accepted and ignored: every member left by the default route, and libsrt chose the backup-mode primary. Now each member binds its source, so a source that is not an address of this host (a typo, or a static IP the DHCP server or carrier has since changed), or one the network will not route, leaves that member unable to connect; if every member is like that, the bond does not connect at all. Check each pinned member before upgrading with `ip -br addr` and `ip route get <peer> from <source>`. In backup mode the lowest configured weight is now the primary. A member that breaks is re-added while another one carries, where before it stayed gone until every member had failed. See [configuration-guide.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/configuration-guide.md#native-libsrt-srt-bonding-socket-groups).
- **Creating or deleting a tunnel and `PUT /api/v1/ptp` need the `admin` role**, like every other write on `/api/v1`. With auth on, a `monitor` token is refused 403 (`admin role required`) there; releases up to 0.116.0 accepted any valid JWT on those three routes. A script that makes these calls with a `monitor` token needs an `admin` one. See [api-security.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/api-security.md).
- **HEVC CMAF outputs open one new init generation at their first start after the upgrade.** The `hvcC` an HEVC CMAF output writes, passthrough and `video_encode` alike, now reads its profile, level, chroma format and bit depths from the SPS; it took them from the wrong bytes, so a Main 10 level 4.1 source was described as profile byte 0x01, level 0, 8-bit. The changed init opens a new generation once: a running DVR stream shows one `#EXT-X-DISCONTINUITY` with a per-row `#EXT-X-MAP`, and a DVR clip straddling it is cut exactly from the recording, or refused where the exact cut falls back to whole segments. H.264 outputs are unaffected. See [cmaf.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/cmaf.md).
- **RTMP output: passthrough video and the FLV timeline change on the wire.** An HEVC source whose slices name more than one PPS now decodes at the receiver: the sequence header carries every parameter set the source has sent, and HEVC tags keep theirs in-band, the `hvcC` marking no array complete (`array_completeness` 0), as FFmpeg writes it. An H.264 intra picture carrying a recovery-point SEI (an IDR-less open-GOP source) is flagged a keyframe, with the sequence header re-sent ahead of it. Every tag goes through one timeline per connection, which continues across the 33-bit PTS wrap, loops and input switches instead of stepping back to 0, gives a picture with no PTS its predecessor's time, and re-anchors transcoded audio at each seam; `silent_fallback` silence now carries real timestamps rather than 0, and the AAC sequence header goes out with the first encoded frame, at its timestamp. A pause of a video-only source is kept up to 10 s, and an outage of both tracks past 3 s is closed. See [supported-protocols.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/supported-protocols.md#rtmprtmps).
- **SMPTE 2022-7 SRT inputs and outputs carry on either leg, and re-acquire each leg on its own.** Each leg is accepted or dialled independently from the start, and a lost leg is re-acquired about 1 s later while the other carries on. Before this release a leg 1 down at start kept the pair off air on a healthy leg 2, and once leg 2 had blipped or missed its 15 s, a later loss of leg 1 alone took the pair off air (on an input with a false Critical `Both redundant legs lost`). The events change, so update any Routine or saved filter on the old texts. On an output, the leg-lost message becomes `SRT output '{id}' 2022-7 leg {n} lost; reconnecting` (same `error_code`, `redundancy_output_leg_lost`), and `details.other_leg` reads only `carrying` or `down`: `connecting` is gone. The `... did not connect within 15s; running single-leg` Warnings are replaced by `... 2022-7 leg {n} did not connect within 15s; still trying`, now with details `{ mode, leg, after_ms, other_leg }` (`reason` / `budget_ms` are gone). On an output it covers either leg in either mode. On an input it is raised for listener legs only. A caller leg on an input latches the 5-attempt `SRT input connection failed` instead, raised by leg 2 as well as leg 1. That event is a Warning while the other leg is connected, and Critical otherwise. The input's per-leg connected Info now reads `SRT input connected (mode={Mode}, redundant leg {n})` for both legs, and leg 1's `details.mode` is the mode it uses, spelled `Listener` / `Caller` / `Rendezvous` like leg 2's: it used to read `listener` whatever the mode. `Redundant leg {n} lost` keeps its text and gains `{ leg, mode, other_leg }`. One known limit: a leg that returns *ahead* of the leg carrying (a shorter path, or a lower `recv_latency_ms`) is not hitless. For about its lead the output interleaves two stream positions (a lead of 1024 packets or more is skipped), and neither shows in `packets_lost` or raises an event. With receive latencies set apart on purpose, every return of the lower-latency leg repeats it, so keep the legs' paths and latencies matched. See [configuration-guide.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/configuration-guide.md#smpte-2022-7-srt-redundancy).
- **Single-channel DeckLink SDI hosts: refresh the systemd unit.** The packaged unit now grants `/dev/blackmagic/dv0`…`dv7` beside `io0`…`io7`. Before, a host whose cards expose `dvN` nodes (single-channel DeckLink SDI) enumerated no cards under the unit and offered no SDI. A manager-driven upgrade replaces only the binary, so add the grants in a drop-in (`sudo systemctl edit bilbycast-edge`, one `DeviceAllow=/dev/blackmagic/dvN rw` line per node, then `sudo systemctl restart bilbycast-edge`), or refresh the unit with `install-edge.sh --upgrade-installer` (which also moves the node to the latest release) and restart. The boot log's `sdi.decklink` warning now names any device node the edge cannot open. See [docs/sdi.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/sdi.md#checklist-c--grant-the-device-nodes-under-systemd), Checklist C.

## Upgrading from v0.115.0 or earlier

<!-- Static text: remove once no supported upgrade starts at v0.115.0 or earlier. -->
- **`PUT /api/v1/config` keeps the secrets its body leaves out.** `GET /api/v1/config` leaves out every infrastructure secret. Putting that body back used to empty the node secret, `server.auth`, `server.tls` and the tunnel keys, and the next save stranded the node at its restart. Now:
  - a secret the body leaves out is kept, and one it names `null` is removed;
  - a body that names `setup_token` is refused with 400;
  - a tunnel the body adds or changes is refused with 400 without the key its mode needs (relay: `tunnel_encryption_key`, direct: `tunnel_psk`);
  - a body that does not parse is 400, where it was 422.

  A script that removed a secret by leaving it out must now send `null`. See [api-reference.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/api-reference.md#put-apiv1config).
- **A registration survives a restart.** The node keeps the node ID it registered under in `secrets.json` beside its secret, so a registration whose `config.json` write failed still authenticates after a restart, and the setup wizard stays closed. A credential save that fails raises the new Warning event `manager_credentials_not_saved`.
- **Pair with bilbycast-manager v0.90.0 for whole downloads at the retention limit.** With that manager, a Replay recording at its retention limit downloads whole: each chunk is read from where the last one ended, so retention removing segments already delivered no longer fails the download. With an older manager, a recording retention has already pruned is refused at the first chunk, as before. Every pairing works.
- **Recorder and CMAF intake queues are bounded in bytes, per queue and node-wide.** Each recorder and each CMAF output queues at most 32 MiB, within 256 MiB for all recorders and 256 MiB for all CMAF outputs on the node. A node whose replay disk or origin cannot keep up drops past those limits and counts the drops in `packets_dropped`.
- **A recording one config push moves between flows is handed over.** When two flows name one `storage_id`, the flow taking the recording waits for the other to let it go. If the other flow is still running 30 s later, it is refused with `replay_recording_in_use`.

## Upgrading from v0.114.1 or earlier

<!-- Static text: remove once no supported upgrade starts at v0.114.1 or earlier. -->
- **Upgrade the manager to v0.89.0 first** (or together). This edge can answer a flow-assembly swap with `pid_bus_assembly_unconfirmed` (the plan was saved and goes on air once the assembler resumes); the v0.88.1 AI assistant reads that as "nothing changed" and reports the plan rolled back. v0.89.0 reverses and audits it.
- **Seq-aware Hitless slots are now checked.** A `seq_aware` slot with a leg that carries no sequence number (any leg but RTP or RIST, or an RTP/RIST leg behind an ingress transcode, `program_number`, `pid_overrides` or `pid_map`, or on a flow with no assembly without `passthrough_clock: true`) is switched to `primary_preference` at the first start, `config.json` is rewritten, and a Warning `config` event names it (`hitless_seq_aware_leg_unsequenced`). A whole-config push of the manager's cached copy is repaired the same way; a targeted edit that carries such a slot (a flow or input edit, an assembly swap, `add_input`) is refused with that code, so fix the slot in the manager before editing those flows.
- **A seq-aware slot's `reorder_window` now takes effect.** The merger used to run 1024 whatever was configured, so a slot set lower now accepts fewer late gap fills. Review it before upgrading.
- **SRT loss is now counted.** libsrt's too-late drops reach `packets_lost` and a new `srt_recv_dropped` health reason (Warning, Error past 100) while the input is on air, and SMPTE 2022-7 SRT inputs report both legs and the pair's loss for the first time. Flows that were losing packets silently can raise alarms after the upgrade.

## Upgrading from v0.114.0 or earlier

<!-- Static text: remove once no supported upgrade starts at v0.114.0 or earlier. -->
The manager's **Upgrade** button cannot move an edge on v0.114.0 or earlier onto this release. Those edges' upgrade check refuses every genuine release: v0.105.0 to v0.114.0 with `upgrade_identity_not_allowed` (it compared the wrong certificate field with its allowlist), older edges with `upgrade_signature_invalid`. It failed closed, so nothing unverified was ever installed, but the fix is in this binary, so move each such edge onto it **once** by hand:

```bash
curl -fsSL https://github.com/Bilbycast/bilbycast-edge/releases/latest/download/install-edge.sh \
  | sudo bash -s -- --upgrade-installer
sudo systemctl restart bilbycast-edge
```

A plain re-run of `install-edge.sh` on an installed node changes nothing; `--upgrade-installer` installs the latest release and points `current` at it, leaving `config.json` and `secrets.json` alone. From then on the Upgrade button works. See [docs/upgrade.md](https://github.com/Bilbycast/bilbycast-edge/blob/main/docs/upgrade.md).

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
