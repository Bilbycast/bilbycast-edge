// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Asset manifests: a persisted, content-derived description of one
//! media-library file.
//!
//! Phase 1 of `MEDIA_PLAYER_HOLISTIC_REDESIGN_PLAN.md`. The manifest is the
//! authoritative answer to "what is this file, really?" — the source **kind
//! is detected from the bytes**, never from the filename extension, so a
//! `.mov` that is really an MP4 resolves to `Mp4`, and a `.ts` that is
//! actually a JPEG cannot masquerade as a transport stream. Everything later
//! in the programme (the playlist planner, prepared-source caching, the
//! manager's detected-kind badge) reads these facts rather than re-probing.
//!
//! ## Detection order
//!
//! Definitive-magic containers first, the heuristic one last (see the note
//! in [`probe_asset`] — leading with the TS sync heuristic mis-detects MP4):
//!
//! 1. MP4 / MOV — a parseable ISO Base Media / QuickTime header with at
//!    least one track the player supports.
//! 2. Still image — a raster format the `image` crate recognises by magic.
//! 3. MPEG-TS — a confirmed sync-byte stride with a plausible PAT.
//! 4. Otherwise `Unsupported` — we refuse to guess.
//!
//! ## Cost
//!
//! Probing is bounded and lives only in the non-real-time upload / rescan
//! path. It reads a head window (for TS sync + image magic + fingerprint), a
//! tail window (fingerprint), and — for MP4 — parses the `moov` header only,
//! never the sample payloads. It does **not** decode video. No libavformat is
//! involved (see the plan's §4.1 decision): MP4 facts come from the pure-Rust
//! `mp4` crate's header parse.
//!
//! ## Persistence
//!
//! Manifests are cached as sidecar JSON under `<media_dir>/.manifests/`. That
//! directory is a dotfile, so [`super::MediaLibrary::list`] (which skips
//! dotfiles and non-files) never surfaces sidecars to the operator and the
//! library quota never counts them. A sidecar is invalidated — and the file
//! re-probed — whenever the file's size or mtime changes, or the manifest
//! schema version moves.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::{media_dir, SCAN_PROBE_BYTES, TAIL_PROBE_BYTES};

/// Bumped whenever the manifest shape changes in a way that makes an
/// on-disk sidecar from an older edge unsafe to trust. A mismatch forces a
/// re-probe (see [`AssetManifest::is_fresh_for`]).
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// The three source kinds the media player actually supports, detected from
/// content. Serializes to the same `snake_case` tags as the config
/// `MediaPlayerSource` (`ts` / `mp4` / `image`) so a manifest's
/// `detected_kind` can be compared directly against a legacy configured kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectedKind {
    Ts,
    Mp4,
    Image,
}

impl DetectedKind {
    /// Operator-facing product label used by the manager badge.
    pub fn product_label(self) -> &'static str {
        match self {
            DetectedKind::Ts => "TS File",
            DetectedKind::Mp4 => "MP4/MOV",
            DetectedKind::Image => "Still Image",
        }
    }
}

/// Outcome of a probe attempt. Stored so a file that cannot be inspected is
/// remembered as such and not re-probed on every list/scan tick (plan §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    /// Successfully inspected; `detected_kind` and `streams` are populated.
    Ready,
    /// The bytes matched none of the three supported kinds.
    Unsupported,
    /// The probe itself errored (I/O, or a structurally broken file of an
    /// otherwise-recognised kind). `error_code` carries the reason.
    Error,
}

/// Stable, machine-readable reasons a probe did not yield a `Ready` manifest.
/// These are the codes the manager surfaces and the planner keys off; keep
/// them stable across releases.
pub mod error_code {
    /// Content matched none of MP4/MOV, TS File, or Still Image.
    pub const UNSUPPORTED: &str = "media_asset_kind_unsupported";
    /// The file could not be opened or read.
    pub const IO: &str = "media_asset_probe_io";
    /// Recognised as MP4 but structurally unusable (fragmented, no
    /// supported track, corrupt `moov`).
    pub const MP4_UNUSABLE: &str = "media_asset_mp4_unusable";
}

/// A bounded, stable identity for the file the manifest describes. Size plus
/// mtime is sufficient to detect the edge's own atomic-rename replacement;
/// `content_fingerprint` is optional here (plan §6.1) and becomes mandatory
/// only once central distribution needs cross-node hash verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub size_bytes: u64,
    pub modified_unix_ms: u64,
    /// Bounded content hash: SHA-256 over `size ‖ head(≤64 KiB) ‖ tail(≤64
    /// KiB)`. Cheap and stable; not a whole-file digest. `None` only when
    /// the windows could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_fingerprint: Option<String>,
}

/// One elementary stream inside the asset. Fields are optional because what
/// is knowable differs by kind and codec; absent facts are `None` rather than
/// invented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestStream {
    pub index: u32,
    pub media_type: ManifestMediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
    /// Frames per second as a rational, so 29.97 (30000/1001) is exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate_num: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate_den: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// SHA-256 (truncated) over the codec configuration bytes (SPS+PPS for
    /// H.264). Two streams with equal fingerprints are decoder-compatible
    /// without a reinit — the planner uses this in Phase 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extradata_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestMediaType {
    Video,
    Audio,
}

/// Container-level facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerInfo {
    /// Short label, e.g. `"mp4"`, `"mpeg-ts"`, `"jpeg"`.
    pub format_name: String,
    /// Duration in 90 kHz ticks, when derivable. `None` for stills and for
    /// TS files where no PCR span was observable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_90khz: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// True for fragmented MP4 (`moof`), which the player cannot address —
    /// such files probe `Ready = false` with `MP4_UNUSABLE`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fragmented: bool,
    /// On-wire TS packet stride (188 / 192 / 204) when `detected_kind == Ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_packet_bytes: Option<u16>,
}

/// Playability summary the manager renders and the planner consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Compatibility {
    /// True when the file can be played on the native remux path with no
    /// transcode. False routes the operator to normalised mode (Phase 6) or
    /// a rejection.
    pub native_playable: bool,
    /// Build features the native path needs (e.g. `media-codecs`, `fdk-aac`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_features: Vec<String>,
    /// Human-readable, non-fatal notes (e.g. "audio-only: no video PID").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The persisted, versioned manifest for one library file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetManifest {
    pub schema_version: u32,
    pub name: String,
    pub file_identity: FileIdentity,
    pub probe_state: ProbeState,
    /// The detected kind. `None` when `probe_state != Ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_kind: Option<DetectedKind>,
    /// Product label mirroring `detected_kind` for UI convenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_kind: Option<String>,
    /// Stable reason when `probe_state != Ready` (see [`error_code`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<ManifestStream>,
    pub compatibility: Compatibility,
}

impl AssetManifest {
    /// Whether this cached manifest still describes the file identified by
    /// `size` / `modified_unix_ms`. A schema bump, a size change, or an mtime
    /// change all force a re-probe.
    pub fn is_fresh_for(&self, size: u64, modified_unix_ms: u64) -> bool {
        self.schema_version == MANIFEST_SCHEMA_VERSION
            && self.file_identity.size_bytes == size
            && self.file_identity.modified_unix_ms == modified_unix_ms
    }

    fn unsupported(name: String, id: FileIdentity, code: &str) -> Self {
        AssetManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            name,
            file_identity: id,
            probe_state: if code == error_code::UNSUPPORTED {
                ProbeState::Unsupported
            } else {
                ProbeState::Error
            },
            detected_kind: None,
            product_kind: None,
            error_code: Some(code.to_string()),
            container: None,
            streams: Vec::new(),
            compatibility: Compatibility::default(),
        }
    }
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

// ── Sidecar persistence ─────────────────────────────────────────────────

/// Directory holding manifest sidecars. A dotfile, so it is excluded from
/// the operator library listing and the quota tally for free.
fn manifest_dir() -> PathBuf {
    media_dir().join(".manifests")
}

/// Sidecar path for `name`. `name` is a validated bare filename (no path
/// components) by the time it reaches here, so `format!` is safe.
fn sidecar_path(name: &str) -> PathBuf {
    manifest_dir().join(format!("{name}.json"))
}

/// Load a cached manifest, returning `None` if absent or unreadable/corrupt
/// (a corrupt sidecar is treated as absent — we re-probe rather than trust
/// it, per plan §6.3).
pub fn load_sidecar(name: &str) -> Option<AssetManifest> {
    let bytes = std::fs::read(sidecar_path(name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomically write a manifest sidecar. Best-effort: a failure to persist is
/// logged and swallowed — the manifest is still returned to the caller, it
/// just won't be cached this time.
pub fn store_sidecar(m: &AssetManifest) {
    if let Err(e) = store_sidecar_inner(m) {
        tracing::warn!(name = %m.name, error = %e, "failed to persist asset manifest sidecar");
    }
}

fn store_sidecar_inner(m: &AssetManifest) -> Result<()> {
    let dir = manifest_dir();
    std::fs::create_dir_all(&dir).map_err(|e| anyhow!("create {}: {e}", dir.display()))?;
    let final_path = sidecar_path(&m.name);
    // Unique temp name: two probes of the SAME file can run concurrently (a
    // post-upload probe racing a manifest() read), and a shared `.<name>.tmp`
    // let one rename out from under the other's write — an `os error 2` on
    // rename. Qualify the temp with pid + a monotonic counter so each write
    // has its own scratch file.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.{seq}.tmp", m.name, std::process::id()));
    let json = serde_json::to_vec_pretty(m)?;
    std::fs::write(&tmp, &json).map_err(|e| anyhow!("write {}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp); // don't leak scratch on failure
        return Err(anyhow!("rename {}: {e}", final_path.display()));
    }
    Ok(())
}

/// Remove a file's cached manifest (called when the file is deleted).
/// Best-effort.
pub fn remove_sidecar(name: &str) {
    let _ = std::fs::remove_file(sidecar_path(name));
}

// ── Probing ─────────────────────────────────────────────────────────────

/// Probe `path` from scratch and build a manifest. Synchronous and
/// potentially disk-heavy (opens the file, parses the MP4 `moov` or decodes
/// an image header) — callers run it under `spawn_blocking`. `name` is the
/// bare library filename; `size` / `modified_unix_ms` come from the caller's
/// already-taken `metadata()` so identity matches exactly what invalidation
/// will later compare against.
pub fn probe_asset(name: &str, path: &Path, size: u64, modified_unix_ms: u64) -> AssetManifest {
    // Read the head window (TS sync + image magic) and tail window
    // (fingerprint). Failure to read at all is an I/O error, not
    // "unsupported".
    let (head, tail) = match read_windows(path) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(name, error = %e, "asset probe: window read failed");
            let id = FileIdentity { size_bytes: size, modified_unix_ms, content_fingerprint: None };
            return AssetManifest::unsupported(name.to_string(), id, error_code::IO);
        }
    };

    let fingerprint = Some(fingerprint(size, &head, &tail));
    let id = FileIdentity { size_bytes: size, modified_unix_ms, content_fingerprint: fingerprint };

    // Detection order: the two *definitive-magic* containers first, the
    // *heuristic* one last.
    //
    // The plan (§6.2) lists TS first, but that assumed TS sync detection is
    // reliable. It is not reliable enough to lead with: TS detection is a
    // heuristic (a run of 0x47 bytes at a 188/192/204 stride plus a
    // plausible PAT), and a modest MP4 carries enough incidental 0x47 bytes
    // at 188 spacing to satisfy it — a live probe of an H.264/AAC MP4 was
    // mis-detected as `ts`. MP4 (`ftyp`/`moov`) and the image formats have
    // unambiguous magic, so we try those first and fall the heuristic TS
    // scan through only for files that are provably neither. A genuine TS has
    // no `ftyp` and no image magic, so it still resolves correctly; the
    // reorder only removes false positives.
    //
    // 1. MP4 / MOV — parseable ISO-BMFF header (opens the file, moov only).
    match probe_mp4(name, path, id.clone()) {
        Mp4Probe::Manifest(m) => return m,
        Mp4Probe::NotMp4 => {}
        Mp4Probe::Unusable(code) => {
            return AssetManifest::unsupported(name.to_string(), id, code);
        }
    }
    // 2. Still image — a raster format the decoder recognises by magic.
    if let Some(m) = probe_image(name, path, &head, id.clone()) {
        return m;
    }
    // 3. MPEG-TS — confirmed stride + a PAT in the head window (heuristic).
    if let Some(m) = probe_ts(name, &head, id.clone()) {
        return m;
    }
    // 4. None matched.
    AssetManifest::unsupported(name.to_string(), id, error_code::UNSUPPORTED)
}

/// Read up to [`SCAN_PROBE_BYTES`] from the head and [`TAIL_PROBE_BYTES`]
/// from the tail. On a file shorter than head+tail the windows may overlap;
/// that is harmless for detection and fingerprinting.
fn read_windows(path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    let len = f.metadata()?.len();

    let head_len = SCAN_PROBE_BYTES.min(len as usize);
    let mut head = vec![0u8; head_len];
    f.read_exact(&mut head)
        .map_err(|e| anyhow!("read head {}: {e}", path.display()))?;

    let tail_len = TAIL_PROBE_BYTES.min(len as usize);
    let mut tail = vec![0u8; tail_len];
    if tail_len > 0 {
        f.seek(SeekFrom::End(-(tail_len as i64)))
            .map_err(|e| anyhow!("seek tail {}: {e}", path.display()))?;
        f.read_exact(&mut tail)
            .map_err(|e| anyhow!("read tail {}: {e}", path.display()))?;
    }
    Ok((head, tail))
}

/// Bounded content fingerprint: SHA-256 over `size_le ‖ head ‖ tail`,
/// hex-encoded. Stable across reads of an unchanged file; changes whenever
/// either window or the size changes.
fn fingerprint(size: u64, head: &[u8], tail: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(size.to_le_bytes());
    // Cap the windows so a huge SCAN_PROBE_BYTES can't make this expensive.
    let cap = 64 * 1024;
    h.update(&head[..head.len().min(cap)]);
    h.update(&tail[..tail.len().min(cap)]);
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// TS detection: a **run-confirmed** sync stride AND a PAT carrying at least
/// one real program in the head window. Both halves are load-bearing.
///
/// `detect_ts_stride` is not usable here — its tail fallback accepts a single
/// 0x47 anywhere in the 512 KiB probe window, which every binary file of any
/// size satisfies. Neither is `MediaScanResult::is_ts`: `scan_programs_in_buf`
/// reports `is_ts: true` on its empty-PAT early return, so it echoes the
/// stride finder's verdict rather than adding independent evidence. Gating on
/// either alone made this function accept an MKV, a WAV or a PDF as a `Ready`,
/// `native_playable` TS asset with no warnings — which in turn made step 4 of
/// [`probe_asset`] (the `Unsupported` verdict) unreachable for any file over
/// 188 bytes that is not an image or ISO-BMFF.
fn probe_ts(name: &str, head: &[u8], id: FileIdentity) -> Option<AssetManifest> {
    let (stride, _) = super::detect_ts_stride_confirmed(head)?;
    let scan = super::scan_programs_in_buf(head);
    if !scan.is_ts || scan.programs.is_empty() {
        return None;
    }

    let mut streams = Vec::new();
    let mut idx = 0u32;
    for prog in &scan.programs {
        for v in &prog.video_streams {
            streams.push(ManifestStream {
                index: idx,
                media_type: ManifestMediaType::Video,
                codec: Some(v.codec.clone()),
                profile: None,
                width: v.width.map(|w| w as u16),
                height: v.height.map(|h| h as u16),
                frame_rate_num: None,
                frame_rate_den: None,
                sample_rate: None,
                channels: None,
                language: None,
                extradata_fingerprint: None,
            });
            idx += 1;
        }
        for a in &prog.audio_streams {
            streams.push(ManifestStream {
                index: idx,
                media_type: ManifestMediaType::Audio,
                codec: Some(a.codec.clone()),
                profile: None,
                width: None,
                height: None,
                frame_rate_num: None,
                frame_rate_den: None,
                sample_rate: None,
                channels: None,
                language: a.language.clone(),
                extradata_fingerprint: None,
            });
            idx += 1;
        }
    }

    let container = ContainerInfo {
        format_name: "mpeg-ts".to_string(),
        duration_90khz: None,
        bitrate_bps: None,
        fragmented: false,
        ts_packet_bytes: Some(stride as u16),
    };

    let has_video = streams.iter().any(|s| s.media_type == ManifestMediaType::Video);
    let warnings = if !has_video && !streams.is_empty() {
        vec!["audio-only transport stream: no video PID".to_string()]
    } else {
        Vec::new()
    };

    Some(AssetManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        name: name.to_string(),
        file_identity: id,
        probe_state: ProbeState::Ready,
        detected_kind: Some(DetectedKind::Ts),
        product_kind: Some(DetectedKind::Ts.product_label().to_string()),
        error_code: None,
        container: Some(container),
        streams,
        compatibility: Compatibility {
            // The `ts` path is a straight remux — no codec build features.
            native_playable: true,
            requires_features: Vec::new(),
            warnings,
        },
    })
}

enum Mp4Probe {
    Manifest(AssetManifest),
    NotMp4,
    Unusable(&'static str),
}

/// MP4/MOV detection via the pure-Rust `mp4` crate's header parse. Reads the
/// `moov` only — no sample payloads, no decode. Mirrors the support envelope
/// of the actual playout path in `mp4_demux.rs`: unfragmented, H.264 video
/// and/or AAC audio.
fn probe_mp4(name: &str, path: &Path, id: FileIdentity) -> Mp4Probe {
    use mp4::{MediaType, TrackType};

    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Mp4Probe::NotMp4,
    };
    let size = id.size_bytes;
    let reader = std::io::BufReader::new(f);
    let mp4 = match mp4::Mp4Reader::read_header(reader, size) {
        Ok(m) => m,
        // Not ISO-BMFF, or a broken header — let the next detector try.
        Err(_) => return Mp4Probe::NotMp4,
    };

    // Fragmented MP4 is recognised-but-unusable: the demuxer cannot address
    // moof samples (see mp4_demux.rs). Report it as such rather than letting
    // it fall through to "unsupported" with no explanation.
    let fragmented = mp4.tracks().values().any(|t| !t.trafs.is_empty());
    if fragmented {
        return Mp4Probe::Unusable(error_code::MP4_UNUSABLE);
    }

    let mut streams = Vec::new();
    let mut idx = 0u32;
    let mut have_supported_track = false;
    let mut duration_90khz = None;

    // Whole-movie duration in 90 kHz ticks from the first track that has a
    // real duration (all tracks share the movie timeline closely enough for
    // a UI hint).
    for track in mp4.tracks().values() {
        let d = track.duration();
        if !d.is_zero() {
            duration_90khz = Some((d.as_secs_f64() * 90_000.0) as u64);
            break;
        }
    }

    for track in mp4.tracks().values() {
        let (tt, mt) = match (track.track_type(), track.media_type()) {
            (Ok(tt), Ok(mt)) => (tt, mt),
            _ => continue,
        };
        match (tt, mt) {
            (TrackType::Video, MediaType::H264) => {
                have_supported_track = true;
                let profile = track.video_profile().ok().map(|p| format!("{p:?}"));
                let extra = mp4_extradata_fingerprint(track);
                let (num, den) = frame_rate_from_track(track.sample_count(), track.duration());
                streams.push(ManifestStream {
                    index: idx,
                    media_type: ManifestMediaType::Video,
                    codec: Some("h264".to_string()),
                    profile,
                    width: Some(track.width()),
                    height: Some(track.height()),
                    frame_rate_num: num,
                    frame_rate_den: den,
                    sample_rate: None,
                    channels: None,
                    language: non_empty(track.language()),
                    extradata_fingerprint: extra,
                });
                idx += 1;
            }
            (TrackType::Audio, MediaType::AAC) => {
                have_supported_track = true;
                let sr = track.sample_freq_index().ok().map(|s| s.freq());
                let ch = track.channel_config().ok().map(channel_count);
                streams.push(ManifestStream {
                    index: idx,
                    media_type: ManifestMediaType::Audio,
                    codec: Some("aac_lc".to_string()),
                    profile: None,
                    width: None,
                    height: None,
                    frame_rate_num: None,
                    frame_rate_den: None,
                    sample_rate: sr,
                    channels: ch,
                    language: non_empty(track.language()),
                    extradata_fingerprint: None,
                });
                idx += 1;
            }
            // Non-H.264 video / non-AAC audio: recognised container, but a
            // track the player can't use. Not fatal on its own — the file is
            // usable if *another* track is supported.
            _ => {}
        }
    }

    if !have_supported_track {
        // A valid MP4 with only HEVC/VP9/AC-3/etc. The container is right but
        // nothing here plays on the native path.
        return Mp4Probe::Unusable(error_code::MP4_UNUSABLE);
    }

    let container = ContainerInfo {
        format_name: "mp4".to_string(),
        duration_90khz,
        bitrate_bps: None,
        fragmented: false,
        ts_packet_bytes: None,
    };

    let has_video = streams.iter().any(|s| s.media_type == ManifestMediaType::Video);
    let warnings = if !has_video {
        vec!["audio-only MP4/MOV: no video track".to_string()]
    } else {
        Vec::new()
    };

    Mp4Probe::Manifest(AssetManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        name: name.to_string(),
        file_identity: id,
        probe_state: ProbeState::Ready,
        detected_kind: Some(DetectedKind::Mp4),
        product_kind: Some(DetectedKind::Mp4.product_label().to_string()),
        error_code: None,
        container: Some(container),
        streams,
        compatibility: Compatibility {
            native_playable: true,
            // The MP4 path decodes nothing but does remux H.264→Annex-B and
            // AAC→ADTS through the shared muxer; it needs the media-codecs
            // build. AAC ADTS wrapping needs fdk-aac.
            requires_features: vec!["media-codecs".to_string(), "fdk-aac".to_string()],
            warnings,
        },
    })
}

/// Actual channel count for an AAC channel configuration. The `mp4` crate's
/// `ChannelConfig` discriminant is the AAC config *index*, which equals the
/// channel count only up to 5 — `FiveOne` (5.1) is 6 channels and `SevenOne`
/// (7.1) is 8, not 6 and 7. Map explicitly so the manifest reports real
/// channel counts.
fn channel_count(c: mp4::ChannelConfig) -> u16 {
    use mp4::ChannelConfig::*;
    match c {
        Mono => 1,
        Stereo => 2,
        Three => 3,
        Four => 4,
        Five => 5,
        FiveOne => 6,
        SevenOne => 8,
    }
}

/// Truncated SHA-256 over a track's SPS+PPS, used to tell decoder-compatible
/// H.264 streams apart from ones that would need a reinit at a splice.
fn mp4_extradata_fingerprint(track: &mp4::Mp4Track) -> Option<String> {
    use sha2::{Digest, Sha256};
    let sps = track.sequence_parameter_set().ok()?;
    let pps = track.picture_parameter_set().ok()?;
    let mut h = Sha256::new();
    h.update(sps);
    h.update(pps);
    let full = hex_lower(&h.finalize());
    Some(full[..16].to_string())
}

/// Exact frame rate for a video track, as a rational.
///
/// **Do not use `mp4::Mp4Track::frame_rate()` for this.** It computes
/// `(sample_count * 1000) / duration_ms` in *integer* arithmetic and only then
/// casts to `f64`, so every 1001-based broadcast rate is truncated before it
/// can ever be recognised: 29.97 arrives as exactly `29.0`, 23.976 as `23.0`,
/// 59.94 as `59.0`. Dividing the sample count by the track's own media
/// duration (which the crate exposes at microsecond precision) keeps the
/// fractional part, so `30 samples / 1.001 s` is `29.97002997` and snaps to
/// 30000/1001 as intended.
fn frame_rate_from_track(sample_count: u32, duration: Duration) -> (Option<u32>, Option<u32>) {
    let secs = duration.as_secs_f64();
    if sample_count == 0 || !secs.is_finite() || secs <= 0.0 {
        return (None, None);
    }
    frame_rate_rational(sample_count as f64 / secs)
}

/// Turn a measured frame rate into an exact rational so 29.97 round-trips as
/// 30000/1001 rather than a lossy float. Unknown rates fall back to
/// `(round(fps), 1)`.
fn frame_rate_rational(fps: f64) -> (Option<u32>, Option<u32>) {
    if !fps.is_finite() || fps <= 0.0 {
        return (None, None);
    }
    const KNOWN: &[(f64, u32, u32)] = &[
        (24000.0 / 1001.0, 24000, 1001),
        (24.0, 24, 1),
        (25.0, 25, 1),
        (30000.0 / 1001.0, 30000, 1001),
        (30.0, 30, 1),
        (50.0, 50, 1),
        (60000.0 / 1001.0, 60000, 1001),
        (60.0, 60, 1),
    ];
    // Snap to the *nearest* well-known rate, not the first one inside the
    // tolerance window. The 1001-based rates sit only 0.03 (29.97 vs 30) and
    // 0.024 (23.976 vs 24) away from their integer neighbours — narrower than
    // the window — so a first-match scan snapped genuine 24 fps and 30 fps
    // assets onto 24000/1001 and 30000/1001 respectively.
    let mut best: Option<(f64, u32, u32)> = None;
    for &(rate, num, den) in KNOWN {
        let delta = (fps - rate).abs();
        if delta < 0.05 && best.is_none_or(|(best_delta, _, _)| delta < best_delta) {
            best = Some((delta, num, den));
        }
    }
    if let Some((_, num, den)) = best {
        return (Some(num), Some(den));
    }
    (Some(fps.round() as u32), Some(1))
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t == "und" {
        None
    } else {
        Some(t.to_string())
    }
}

/// Still-image detection. The magic bytes in the head window pick the format
/// cheaply; dimensions come from a header-only decode (`image` reads just the
/// header for `into_dimensions` on the formats we enable).
fn probe_image(name: &str, path: &Path, head: &[u8], id: FileIdentity) -> Option<AssetManifest> {
    let format = image::guess_format(head).ok()?;

    // Dimensions: prefer a cheap header-only read straight from disk.
    let (width, height) = match image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.into_dimensions().ok())
    {
        Some((w, h)) => (Some(w as u16), Some(h as u16)),
        None => (None, None),
    };

    let format_name = format!("{format:?}").to_lowercase();

    Some(AssetManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        name: name.to_string(),
        file_identity: id,
        probe_state: ProbeState::Ready,
        detected_kind: Some(DetectedKind::Image),
        product_kind: Some(DetectedKind::Image.product_label().to_string()),
        error_code: None,
        container: Some(ContainerInfo {
            format_name,
            duration_90khz: None,
            bitrate_bps: None,
            fragmented: false,
            ts_packet_bytes: None,
        }),
        streams: vec![ManifestStream {
            index: 0,
            media_type: ManifestMediaType::Video,
            codec: Some("raster".to_string()),
            profile: None,
            width,
            height,
            frame_rate_num: None,
            frame_rate_den: None,
            sample_rate: None,
            channels: None,
            language: None,
            extradata_fingerprint: None,
        }],
        compatibility: Compatibility {
            native_playable: true,
            // The slate path encodes H.264 and (optionally) AAC silence.
            requires_features: vec!["media-codecs".to_string(), "fdk-aac".to_string()],
            warnings: Vec::new(),
        },
    })
}

/// Convert a `metadata().modified()` into unix-epoch milliseconds.
pub fn mtime_unix_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> FileIdentity {
        FileIdentity { size_bytes: 10, modified_unix_ms: 20, content_fingerprint: None }
    }

    #[test]
    fn detected_kind_product_labels_are_stable() {
        assert_eq!(DetectedKind::Ts.product_label(), "TS File");
        assert_eq!(DetectedKind::Mp4.product_label(), "MP4/MOV");
        assert_eq!(DetectedKind::Image.product_label(), "Still Image");
    }

    #[test]
    fn detected_kind_serializes_like_config_kind() {
        assert_eq!(serde_json::to_string(&DetectedKind::Mp4).unwrap(), "\"mp4\"");
        assert_eq!(serde_json::to_string(&DetectedKind::Ts).unwrap(), "\"ts\"");
        assert_eq!(serde_json::to_string(&DetectedKind::Image).unwrap(), "\"image\"");
    }

    #[test]
    fn manifest_json_round_trips() {
        let m = AssetManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            name: "clip.mov".to_string(),
            file_identity: FileIdentity {
                size_bytes: 483920111,
                modified_unix_ms: 1784512345000,
                content_fingerprint: Some("deadbeef".to_string()),
            },
            probe_state: ProbeState::Ready,
            detected_kind: Some(DetectedKind::Mp4),
            product_kind: Some("MP4/MOV".to_string()),
            error_code: None,
            container: Some(ContainerInfo {
                format_name: "mp4".to_string(),
                duration_90khz: Some(5405400),
                bitrate_bps: Some(7800000),
                fragmented: false,
                ts_packet_bytes: None,
            }),
            streams: vec![ManifestStream {
                index: 0,
                media_type: ManifestMediaType::Video,
                codec: Some("h264".to_string()),
                profile: Some("High".to_string()),
                width: Some(1920),
                height: Some(1080),
                frame_rate_num: Some(30000),
                frame_rate_den: Some(1001),
                sample_rate: None,
                channels: None,
                language: None,
                extradata_fingerprint: Some("0123456789abcdef".to_string()),
            }],
            compatibility: Compatibility {
                native_playable: true,
                requires_features: vec!["media-codecs".to_string()],
                warnings: Vec::new(),
            },
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: AssetManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn freshness_tracks_identity_and_schema() {
        let m = AssetManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            name: "x".to_string(),
            file_identity: FileIdentity {
                size_bytes: 100,
                modified_unix_ms: 200,
                content_fingerprint: None,
            },
            probe_state: ProbeState::Ready,
            detected_kind: Some(DetectedKind::Ts),
            product_kind: Some("TS File".to_string()),
            error_code: None,
            container: None,
            streams: Vec::new(),
            compatibility: Compatibility::default(),
        };
        assert!(m.is_fresh_for(100, 200));
        assert!(!m.is_fresh_for(101, 200), "size change must invalidate");
        assert!(!m.is_fresh_for(100, 201), "mtime change must invalidate");

        let mut stale = m.clone();
        stale.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        assert!(!stale.is_fresh_for(100, 200), "schema bump must invalidate");
    }

    #[test]
    fn unsupported_bytes_probe_unsupported() {
        // Deliberately NOT a 0x47-free buffer. The old fixture was
        // `vec![0x11u8; 4096]`, which passed vacuously: it contained no sync
        // byte at all, so it could not exercise the single-sync-byte tail
        // fallback that made probe_ts accept essentially every binary file.
        let head = pseudo_fill(64 * 1024);
        assert!(
            head.contains(&0x47),
            "fixture must contain a stray sync byte or it tests nothing"
        );
        assert!(
            super::probe_ts("f", &head, ident()).is_none(),
            "a stray 0x47 must not classify a file as a transport stream"
        );
        assert!(image::guess_format(&head).is_err());
    }

    /// An EBML/Matroska header must probe Unsupported, not TS. This is the
    /// concrete file class Phase 0 exists to reject: the `mp4` crate is
    /// ISO-BMFF only and never parsed Matroska, so an .mkv that classified as
    /// a playable "TS File" would be exactly the air-time failure Phase 0's
    /// contract change was written to prevent.
    #[test]
    fn matroska_probes_unsupported_not_ts() {
        let mut head = vec![0x1A, 0x45, 0xDF, 0xA3]; // EBML magic
        head.extend_from_slice(&pseudo_fill(64 * 1024));
        assert!(super::probe_ts("clip.mkv", &head, ident()).is_none());
        let m = probe_asset("clip.mkv", Path::new("/nonexistent"), head.len() as u64, 1);
        // The path does not exist, so this asserts the classification arm we
        // can reach without a fixture on disk: it must not come back Ready/Ts.
        assert_ne!(m.detected_kind, Some(DetectedKind::Ts));
    }

    /// Deterministic pseudo-random bytes — a cheap LCG, no dev-dependency.
    /// Statistically certain to contain 0x47 at this length, which is the
    /// whole point: real-world binaries do too.
    fn pseudo_fill(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push((x >> 24) as u8);
        }
        out
    }

    #[test]
    fn unsupported_manifest_has_stable_code() {
        let m = AssetManifest::unsupported("f".to_string(), ident(), error_code::UNSUPPORTED);
        assert_eq!(m.probe_state, ProbeState::Unsupported);
        assert_eq!(m.error_code.as_deref(), Some("media_asset_kind_unsupported"));
        assert!(m.detected_kind.is_none());
    }

    #[test]
    fn io_error_maps_to_error_state_not_unsupported() {
        let m = AssetManifest::unsupported("f".to_string(), ident(), error_code::IO);
        assert_eq!(m.probe_state, ProbeState::Error);
        assert_eq!(m.error_code.as_deref(), Some("media_asset_probe_io"));
    }

    #[test]
    fn frame_rate_snaps_to_broadcast_rationals() {
        assert_eq!(frame_rate_rational(29.97), (Some(30000), Some(1001)));
        assert_eq!(frame_rate_rational(23.976), (Some(24000), Some(1001)));
        assert_eq!(frame_rate_rational(25.0), (Some(25), Some(1)));
        assert_eq!(frame_rate_rational(59.94), (Some(60000), Some(1001)));
        // Unknown rate falls back to (round, 1).
        assert_eq!(frame_rate_rational(48.0), (Some(48), Some(1)));
        // Nonsense yields nothing.
        assert_eq!(frame_rate_rational(0.0), (None, None));
        assert_eq!(frame_rate_rational(f64::NAN), (None, None));
    }

    /// The integer neighbours of the 1001-based rates must NOT be snapped onto
    /// them. 24.0 sits 0.024 from 23.976 and 30.0 sits 0.03 from 29.97 — both
    /// inside the tolerance window — so a first-match scan mislabelled every
    /// genuine 24 fps and 30 fps asset as fractional.
    #[test]
    fn integer_rates_are_not_snapped_onto_their_1001_neighbours() {
        assert_eq!(frame_rate_rational(24.0), (Some(24), Some(1)));
        assert_eq!(frame_rate_rational(30.0), (Some(30), Some(1)));
        assert_eq!(frame_rate_rational(60.0), (Some(60), Some(1)));
        assert_eq!(frame_rate_rational(50.0), (Some(50), Some(1)));
    }

    /// Exact rates as they actually arrive from a real track — the full
    /// fractional expansion, not the 4-digit label.
    #[test]
    fn exact_1001_rates_snap() {
        assert_eq!(
            frame_rate_rational(30000.0 / 1001.0),
            (Some(30000), Some(1001))
        );
        assert_eq!(
            frame_rate_rational(24000.0 / 1001.0),
            (Some(24000), Some(1001))
        );
        assert_eq!(
            frame_rate_rational(60000.0 / 1001.0),
            (Some(60000), Some(1001))
        );
    }

    /// Guards the `mp4` crate's integer-division trap without needing ffmpeg
    /// on the runner. These are the exact `(sample_count, duration)` pairs a
    /// one-second clip at each rate produces; `Mp4Track::frame_rate()` would
    /// return 23 / 29 / 59 for the fractional three, which then fall outside
    /// every snap window and land as `(23,1)` / `(29,1)` / `(59,1)`.
    #[test]
    fn frame_rate_from_track_survives_the_integer_division_trap() {
        // 29.97: 30 samples across 1.001 s.
        assert_eq!(
            frame_rate_from_track(30, Duration::from_micros(1_001_000)),
            (Some(30000), Some(1001))
        );
        // 23.976: 24 samples across 1.001 s.
        assert_eq!(
            frame_rate_from_track(24, Duration::from_micros(1_001_000)),
            (Some(24000), Some(1001))
        );
        // 59.94: 60 samples across 1.001 s.
        assert_eq!(
            frame_rate_from_track(60, Duration::from_micros(1_001_000)),
            (Some(60000), Some(1001))
        );
        // Integer rates stay integer.
        assert_eq!(
            frame_rate_from_track(25, Duration::from_secs(1)),
            (Some(25), Some(1))
        );
        assert_eq!(
            frame_rate_from_track(30, Duration::from_secs(1)),
            (Some(30), Some(1))
        );
        // Degenerate tracks yield nothing rather than dividing by zero.
        assert_eq!(frame_rate_from_track(0, Duration::from_secs(1)), (None, None));
        assert_eq!(frame_rate_from_track(30, Duration::ZERO), (None, None));
    }

    #[test]
    fn fingerprint_is_stable_and_size_sensitive() {
        let head = vec![1u8, 2, 3, 4];
        let tail = vec![9u8, 8, 7];
        let a = fingerprint(1000, &head, &tail);
        let b = fingerprint(1000, &head, &tail);
        assert_eq!(a, b, "same inputs → same fingerprint");
        let c = fingerprint(1001, &head, &tail);
        assert_ne!(a, c, "size change → different fingerprint");
        assert_eq!(a.len(), 64, "sha-256 hex is 64 chars");
    }

    #[test]
    fn non_empty_filters_placeholders() {
        assert_eq!(non_empty("eng"), Some("eng".to_string()));
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("und"), None);
        assert_eq!(non_empty("  "), None);
    }

    /// End-to-end image probe on a real synthesized PNG — exercises
    /// `probe_asset` → `probe_image` → identity/fingerprint without any
    /// external fixture. Uses the `image` crate (already a dependency) to
    /// write a tiny file to a unique temp path.
    #[test]
    fn probe_asset_detects_a_real_png() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bilby_manifest_test_{}.png", std::process::id()));
        // 4×2 RGBA image.
        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([10, 20, 30, 255]));
        img.save_with_format(&path, image::ImageFormat::Png).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let size = meta.len();

        let m = probe_asset("still.png", &path, size, 12345);
        let _ = std::fs::remove_file(&path);

        assert_eq!(m.probe_state, ProbeState::Ready);
        assert_eq!(m.detected_kind, Some(DetectedKind::Image));
        assert_eq!(m.product_kind.as_deref(), Some("Still Image"));
        assert!(m.file_identity.content_fingerprint.is_some());
        assert_eq!(m.file_identity.size_bytes, size);
        let s = &m.streams[0];
        assert_eq!(s.width, Some(4));
        assert_eq!(s.height, Some(2));
        assert!(m.container.as_ref().unwrap().format_name.contains("png"));
    }

    /// A file whose extension lies (a PNG named `.ts`) must be detected by
    /// content as an image, not a transport stream — the whole point of
    /// content-based detection (plan §6.2). Renaming cannot trick it.
    #[test]
    fn extension_cannot_spoof_detection() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bilby_spoof_test_{}.ts", std::process::id()));
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        img.save_with_format(&path, image::ImageFormat::Png).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();

        let m = probe_asset("liar.ts", &path, size, 1);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            m.detected_kind,
            Some(DetectedKind::Image),
            "content wins over the .ts extension"
        );
    }

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Regression for the detection-order bug a live probe caught: a real
    /// H.264/AAC MP4 carries enough incidental 0x47 bytes at a 188-byte
    /// stride to satisfy the heuristic TS sync scan, so leading with TS
    /// mis-detected it as `ts`. Detection must try the definitive MP4 magic
    /// first. Gated on a host `ffmpeg` (skips cleanly in a bare CI), matching
    /// the idiom in `engine::thumbnail`.
    #[test]
    fn real_mp4_detects_as_mp4_not_ts() {
        if !ffmpeg_available() {
            eprintln!("skipping real_mp4_detects_as_mp4_not_ts: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bilby_mp4_probe_{}.mp4", std::process::id()));
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30000/1001:duration=1",
                "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=1",
                "-c:v", "libx264", "-profile:v", "high", "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-ac", "2",
                "-movflags", "+faststart",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&path);
            eprintln!("skipping real_mp4_detects_as_mp4_not_ts: ffmpeg could not encode (no libx264?)");
            return;
        }

        let size = std::fs::metadata(&path).unwrap().len();
        let m = probe_asset("clip.mp4", &path, size, 1);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            m.detected_kind,
            Some(DetectedKind::Mp4),
            "H.264/AAC MP4 must detect as mp4, not ts (detection-order regression)"
        );
        assert_eq!(m.probe_state, ProbeState::Ready);

        let video = m
            .streams
            .iter()
            .find(|s| s.media_type == ManifestMediaType::Video)
            .expect("video stream");
        assert_eq!(video.codec.as_deref(), Some("h264"));
        assert_eq!(video.width, Some(320));
        assert_eq!(video.height, Some(240));
        // The 30000/1001 rate must round-trip exactly, not as a lossy float.
        assert_eq!(video.frame_rate_num, Some(30000));
        assert_eq!(video.frame_rate_den, Some(1001));
        assert!(video.extradata_fingerprint.is_some());

        let audio = m
            .streams
            .iter()
            .find(|s| s.media_type == ManifestMediaType::Audio)
            .expect("audio stream");
        assert_eq!(audio.sample_rate, Some(48000));
        assert_eq!(audio.channels, Some(2));
    }
}
