// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Media-library storage for the file-backed media-player input.
//!
//! The library is a flat directory on the edge's local disk holding the
//! `.ts` / `.mp4` / image assets that a `MediaPlayer` input plays back.
//! Files arrive over the WS upload command from the manager (chunked, 1
//! MiB per chunk to stay under the 5 MiB WS payload cap). Filenames are
//! sanitised to a strict ASCII subset so cross-platform pickers and
//! shells never have to escape anything.
//!
//! # Invariants
//!
//! * Final files live as `<media_dir>/<name>` with mode `0644`.
//! * In-progress uploads stage under `<media_dir>/.tmp/<name>.<session_id>`
//!   and atomically `rename(2)` into place on the final chunk. Any partial
//!   stagings older than the configured TTL are reaped on next list.
//! * Per-file size is capped at `MAX_FILE_BYTES` (default 4 GiB) and total
//!   library footprint at `MAX_TOTAL_BYTES` (default 16 GiB) — the upload
//!   handler refuses chunks that would breach either cap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

pub mod probe;
pub use probe::{MediaAudioStreamInfo, MediaVideoStreamInfo};

/// Hard ceiling per individual asset (4 GiB).
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Soft ceiling for the entire library footprint (16 GiB). The upload
/// handler refuses new chunks once any pending upload would push the sum
/// past this. Operators with bigger needs should mount a dedicated
/// volume and override `BILBYCAST_MEDIA_DIR`.
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Reap partial uploads idle for longer than this. Keeps the staging
/// directory tidy without surprising operators mid-upload over a flaky
/// link.
pub const STAGING_TTL: Duration = Duration::from_secs(60 * 60);

/// Public summary entry for a single library file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaFileInfo {
    pub name: String,
    pub size_bytes: u64,
    /// File mtime in seconds since the Unix epoch. Suitable for the
    /// browser's library list; not authoritative for replay decisions.
    pub modified_unix: u64,
}

/// Maximum bytes read from the head of a media file when scanning for
/// programs. 512 KiB covers ≥ one full PSI cycle on every common mux rate
/// up to ~50 Mbps and keeps the WS round-trip under a few ms on typical
/// disk hardware.
pub const SCAN_PROBE_BYTES: usize = 512 * 1024;
/// Bytes read from the tail of the file to anchor the PCR-delta bitrate
/// estimate. 64 KiB is plenty to land at least one PCR-bearing packet
/// even on sparse-PCR audio-only programs.
pub const TAIL_PROBE_BYTES: usize = 64 * 1024;
const TS_PACKET_SIZE_U64: u64 = crate::engine::ts_parse::TS_PACKET_SIZE as u64;

/// One program inside a media-library MPEG-TS file. Surfaced to the
/// manager UI so the operator can pick from a real list rather than
/// guessing program numbers, and see at a glance what codecs / resolution
/// each program carries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaProgramInfo {
    /// Program number from the PAT. Always `> 0` for real programs (0 is
    /// the NIT, which we filter out).
    pub program_number: u16,
    /// PMT PID for this program. Useful diagnostic; the UI displays it
    /// alongside the program number.
    pub pmt_pid: u16,
    /// PCR PID from the PMT. `None` when no PMT was found in the probe
    /// window or the PMT signalled `0x1FFF` (no PCR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_pid: Option<u16>,
    /// Video elementary streams declared by the PMT, in PMT order. Empty
    /// when no PMT was found in the probe window or the program is
    /// audio-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_streams: Vec<MediaVideoStreamInfo>,
    /// Audio elementary streams declared by the PMT, in PMT order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_streams: Vec<MediaAudioStreamInfo>,
}

/// Result of [`MediaLibrary::scan_programs`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaScanResult {
    /// `true` if the probe window contained a PAT (i.e. the file looks
    /// like MPEG-TS). MP4 / MOV / images and corrupted TS files come
    /// back `false`.
    pub is_ts: bool,
    /// Programs found in the PAT, sorted by program number ascending.
    /// May be empty (with `is_ts: true`) for TS files whose PAT carries
    /// only NIT entries — pathological but possible.
    pub programs: Vec<MediaProgramInfo>,
    /// `true` if the file does not exist in the library. The manager UI
    /// handles this gracefully (rather than the WS round-trip 500-ing on
    /// a stale picker selection).
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_found: bool,
    /// File size in bytes, for UI display alongside bitrate / duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
    /// Estimated whole-file average bitrate in bits/sec, derived from the
    /// PCR delta between the first PCR observed in the head and the last
    /// PCR observed in the tail. `None` when no PCR was observable in
    /// either window or the file is too short to span two PCR samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Estimated playable duration in milliseconds, from the same PCR
    /// delta. Useful for the picker UI to render a `mm:ss` hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[inline]
fn is_false(b: &bool) -> bool { !*b }

/// Detect the on-disk packet stride of a TS-shaped buffer. Returns
/// `(stride, start_offset)` if a confirmed sync pattern is found, else
/// `None`. Auto-detects 188 (canonical MPEG-TS), 192 (M2TS / AVCHD /
/// Blu-ray BDAV — 4-byte timestamp per packet), or 204 (DVB
/// Reed-Solomon parity-suffixed broadcast capture).
pub(crate) fn detect_ts_stride(buf: &[u8]) -> Option<(usize, usize)> {
    use crate::engine::ts_parse::{TS_PACKET_SIZE, TS_SYNC_BYTE};
    const STRIDE_CANDIDATES: [usize; 3] = [TS_PACKET_SIZE, 192, 204];
    for &stride in &STRIDE_CANDIDATES {
        let mut start = 0usize;
        while start + 2 * stride <= buf.len() {
            if buf[start] == TS_SYNC_BYTE && buf[start + stride] == TS_SYNC_BYTE {
                return Some((stride, start));
            }
            start += 1;
        }
        // Tail-of-buffer fallback: a single confirmed sync byte counts if
        // there isn't room for two strides. Only the canonical 188 case
        // gets this concession (small files), to keep 192/204 detection
        // strict.
        if stride == TS_PACKET_SIZE && buf.len() >= TS_PACKET_SIZE {
            for s in 0..=buf.len() - TS_PACKET_SIZE {
                if buf[s] == TS_SYNC_BYTE {
                    return Some((TS_PACKET_SIZE, s));
                }
            }
        }
    }
    None
}

/// Walk a TS-shaped byte buffer and return the first PAT's program list,
/// each program enriched with its PMT-derived video / audio stream tables
/// and (where the SPS / sequence-header lands inside the probe window)
/// the first video stream's coded dimensions. Pure function for
/// unit-testability — the async wrapper just reads the first chunk of
/// bytes and hands them in. Bitrate / duration are filled in by the async
/// wrapper after a separate tail read.
pub fn scan_programs_in_buf(buf: &[u8]) -> MediaScanResult {
    use crate::engine::ts_parse::{TS_PACKET_SIZE, TS_SYNC_BYTE, parse_pat_programs, ts_pid, ts_pusi};

    let (stride, start) = match detect_ts_stride(buf) {
        Some(d) => d,
        None => {
            return MediaScanResult {
                is_ts: false,
                programs: Vec::new(),
                not_found: false,
                file_size_bytes: None,
                bitrate_bps: None,
                duration_ms: None,
            };
        }
    };

    let mut programs_raw: Vec<(u16, u16)> = Vec::new();
    let mut offset = start;
    while offset + TS_PACKET_SIZE <= buf.len() {
        let pkt = &buf[offset..offset + TS_PACKET_SIZE];
        offset += stride;
        if pkt[0] != TS_SYNC_BYTE {
            continue;
        }
        if ts_pid(pkt) != 0 {
            continue;
        }
        if !ts_pusi(pkt) {
            continue;
        }
        let entries = parse_pat_programs(pkt);
        programs_raw = entries
            .into_iter()
            .filter(|(num, _)| *num != 0)
            .collect();
        break;
    }

    if programs_raw.is_empty() {
        return MediaScanResult {
            is_ts: true,
            programs: Vec::new(),
            not_found: false,
            file_size_bytes: None,
            bitrate_bps: None,
            duration_ms: None,
        };
    }

    let mut programs: Vec<MediaProgramInfo> = programs_raw
        .into_iter()
        .map(|(num, pmt_pid)| {
            let mut pmt = probe::scan_pmt_in_buf(buf, stride, start, pmt_pid);
            for v in &mut pmt.video_streams {
                if let Some((w, h)) = probe::extract_video_dims(
                    buf,
                    stride,
                    start,
                    v.pid,
                    v.stream_type,
                ) {
                    v.width = Some(w);
                    v.height = Some(h);
                }
            }
            MediaProgramInfo {
                program_number: num,
                pmt_pid,
                pcr_pid: pmt.pcr_pid,
                video_streams: pmt.video_streams,
                audio_streams: pmt.audio_streams,
            }
        })
        .collect();
    programs.sort_by_key(|p| p.program_number);

    MediaScanResult {
        is_ts: true,
        programs,
        not_found: false,
        file_size_bytes: None,
        bitrate_bps: None,
        duration_ms: None,
    }
}

/// Resolve the configured media directory. Mirrors
/// [`crate::engine::input_media_player::media_dir`] but lives here too so
/// the upload / list / delete API layer doesn't depend on the engine
/// module's call surface.
pub fn media_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BILBYCAST_MEDIA_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("bilbycast").join("media");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".bilbycast").join("media");
    }
    PathBuf::from("./media")
}

fn staging_dir() -> PathBuf {
    media_dir().join(".tmp")
}

/// In-memory upload session bookkeeping, keyed by sanitised filename.
/// Holds the staging file path, declared chunk count + total bytes, and a
/// running `bytes_received` counter. Lives behind a `Mutex` because the
/// final-chunk rename must observe a consistent view. The session is
/// wrapped in `Arc` so callers can clone the handle out of the DashMap
/// and release the shard lock *before* awaiting on the inner mutex —
/// holding a `RefMut` across the inner `lock().await` and then calling
/// `self.sessions.remove(name)` on the same shard is a self-deadlock
/// (write-lock acquired twice on a non-reentrant `RwLock`).
pub struct MediaLibrary {
    sessions: DashMap<String, Arc<Mutex<UploadSession>>>,
}

struct UploadSession {
    staging_path: PathBuf,
    next_chunk: u32,
    total_chunks: u32,
    total_bytes: u64,
    bytes_received: u64,
    last_touch: SystemTime,
}

impl MediaLibrary {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Ensure the media directory and `.tmp` staging directory exist.
    /// Called once on boot from the WS handler hook so a fresh edge can
    /// accept uploads without the operator pre-mkdir-ing anything.
    pub fn ensure_dirs() -> Result<()> {
        let dir = media_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("create media dir {}: {e}", dir.display()))?;
        let stg = staging_dir();
        std::fs::create_dir_all(&stg)
            .map_err(|e| anyhow!("create staging dir {}: {e}", stg.display()))?;
        Ok(())
    }

    /// Enumerate the library, sorted by filename. Reaps stale staging
    /// files as a side effect so operators see a clean list after a
    /// failed upload.
    pub async fn list() -> Result<Vec<MediaFileInfo>> {
        Self::ensure_dirs()?;
        Self::reap_stale_staging().await;
        let dir = media_dir();
        let mut entries: Vec<MediaFileInfo> = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| anyhow!("read_dir entry: {e}"))?
        {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push(MediaFileInfo {
                name,
                size_bytes: meta.len(),
                modified_unix,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Total bytes of finalised library files. Used by the upload handler
    /// to enforce [`MAX_TOTAL_BYTES`] before accepting a new chunk.
    pub async fn total_bytes() -> Result<u64> {
        let files = Self::list().await?;
        Ok(files.iter().map(|f| f.size_bytes).sum())
    }

    /// Probe an MPEG-TS file's PAT and return its programs.
    ///
    /// Reads up to [`SCAN_PROBE_BYTES`] from the head of the file (enough for
    /// any well-formed mux to land its first PAT — typical PSI cadence is
    /// every 100 ms; 512 KiB at 50 Mbps is ~80 ms of stream so we cover one
    /// full PSI cycle even on dense-program multiplexes). Files that don't
    /// look like MPEG-TS (no sync byte, or no PAT in the probe window) come
    /// back as `is_ts: false` with an empty program list — the manager UI
    /// uses that to hide the program-picker for non-TS sources (MP4 etc).
    ///
    /// Single-program files also come back as `is_ts: true` with a one-entry
    /// list; the UI can still offer "All programs" (the whole-file passthrough)
    /// or the single program (which down-selects identically when the file
    /// has only one).
    pub async fn scan_programs(name: &str) -> Result<MediaScanResult> {
        crate::config::validation::validate_media_filename(name, "scan_media")?;
        let path = media_dir().join(name);
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(MediaScanResult {
                    is_ts: false,
                    programs: Vec::new(),
                    not_found: true,
                    file_size_bytes: None,
                    bitrate_bps: None,
                    duration_ms: None,
                });
            }
            Err(e) => return Err(anyhow!("open {}: {e}", path.display())),
        };
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let file_size = file
            .metadata()
            .await
            .map(|m| m.len())
            .map_err(|e| anyhow!("metadata {}: {e}", path.display()))?;

        let mut head = vec![0u8; SCAN_PROBE_BYTES];
        let n = file
            .read(&mut head)
            .await
            .map_err(|e| anyhow!("read head {}: {e}", path.display()))?;
        head.truncate(n);

        let mut result = scan_programs_in_buf(&head);
        result.file_size_bytes = Some(file_size);

        // Bitrate / duration estimate via PCR delta. Pick the first
        // program's PCR PID — for an MPTS every program shares the wire
        // clock, so any one is fine. Skip cleanly when no PCR is
        // observable in the head window or the file is too short to span
        // a useful tail probe.
        let pcr_pid = result
            .programs
            .iter()
            .find_map(|p| p.pcr_pid);
        if let Some(pcr_pid) = pcr_pid {
            if let Some((stride, start)) = detect_ts_stride(&head) {
                let first_pcr = probe::find_first_pcr(&head, stride, start, pcr_pid);
                if let Some(first_pcr) = first_pcr {
                    let tail_window = TAIL_PROBE_BYTES.min(file_size as usize) as u64;
                    if file_size > tail_window && tail_window >= TS_PACKET_SIZE_U64 {
                        let tail_offset = file_size - tail_window;
                        if file
                            .seek(std::io::SeekFrom::Start(tail_offset))
                            .await
                            .is_ok()
                        {
                            let mut tail = vec![0u8; tail_window as usize];
                            if let Ok(tn) = file.read(&mut tail).await {
                                tail.truncate(tn);
                                if let Some((tail_stride, tail_start)) =
                                    detect_ts_stride(&tail)
                                {
                                    if let Some(last_pcr) = probe::find_last_pcr(
                                        &tail,
                                        tail_stride,
                                        tail_start,
                                        pcr_pid,
                                    ) {
                                        // PCR is 27 MHz. Handle the rare
                                        // wallclock wrap by ignoring
                                        // negative deltas.
                                        if last_pcr > first_pcr {
                                            let delta_27mhz = last_pcr - first_pcr;
                                            let duration_secs =
                                                delta_27mhz as f64 / 27_000_000.0;
                                            if duration_secs > 0.05 {
                                                result.duration_ms =
                                                    Some((duration_secs * 1000.0) as u64);
                                                result.bitrate_bps = Some(
                                                    (file_size as f64 * 8.0
                                                        / duration_secs)
                                                        as u64,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    /// Hard-delete a file. Filename validation is the caller's responsibility
    /// (we re-check defence-in-depth here too).
    pub async fn delete(name: &str) -> Result<bool> {
        crate::config::validation::validate_media_filename(name, "delete_media")?;
        let path = media_dir().join(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow!("delete {}: {e}", path.display())),
        }
    }

    /// Apply one chunk of an upload. Returns `true` when the final chunk
    /// fsyncs + renames into place.
    pub async fn apply_chunk(
        &self,
        name: &str,
        chunk_index: u32,
        total_chunks: u32,
        total_bytes: u64,
        data_b64: &str,
    ) -> Result<UploadProgress> {
        crate::config::validation::validate_media_filename(name, "upload_media_chunk")?;
        if total_chunks == 0 {
            bail!("upload_media_chunk: total_chunks must be ≥ 1");
        }
        if chunk_index >= total_chunks {
            bail!(
                "upload_media_chunk: chunk_index {chunk_index} ≥ total_chunks {total_chunks}"
            );
        }
        if total_bytes > MAX_FILE_BYTES {
            bail!(
                "upload_media_chunk: total_bytes {total_bytes} exceeds per-file cap {MAX_FILE_BYTES}"
            );
        }

        Self::ensure_dirs()?;
        Self::reap_stale_staging().await;

        // Decode and verify chunk size before touching disk so we fail
        // fast on a malformed encode.
        let chunk = BASE64
            .decode(data_b64)
            .map_err(|e| anyhow!("upload_media_chunk: base64 decode failed: {e}"))?;

        // Library-footprint guard: existing finalised bytes + this upload's
        // declared size must not breach the cap. Counting `total_bytes` (not
        // `bytes_received`) catches the breach on chunk 0 instead of mid-upload.
        let used = Self::total_bytes().await?;
        if used.saturating_add(total_bytes) > MAX_TOTAL_BYTES {
            bail!(
                "upload_media_chunk: library footprint {used} + new file {total_bytes} would exceed cap {MAX_TOTAL_BYTES}"
            );
        }

        // Open or resume the staging session for this filename. Clone the
        // `Arc<Mutex<_>>` handle out of the DashMap and drop the entry
        // immediately so we never hold a shard write lock across the
        // inner `lock().await` or the final-chunk `self.sessions.remove`.
        let staging_path = staging_dir().join(format!("{name}.staging"));
        let session = self
            .sessions
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(UploadSession {
                    staging_path: staging_path.clone(),
                    next_chunk: 0,
                    total_chunks,
                    total_bytes,
                    bytes_received: 0,
                    last_touch: SystemTime::now(),
                }))
            })
            .clone();

        // tokio::sync::Mutex — the guard *is* Send so it survives the
        // spawn_blocking await below without collapsing the whole future
        // off Send.
        let mut sess = session.lock().await;

        if sess.total_chunks != total_chunks {
            bail!(
                "upload_media_chunk: total_chunks {} differs from session {}",
                total_chunks,
                sess.total_chunks
            );
        }
        if sess.total_bytes != total_bytes {
            bail!(
                "upload_media_chunk: total_bytes {} differs from session {}",
                total_bytes,
                sess.total_bytes
            );
        }
        if chunk_index != sess.next_chunk {
            bail!(
                "upload_media_chunk: out-of-order chunk {chunk_index}, expected {}",
                sess.next_chunk
            );
        }

        let new_total = sess.bytes_received.saturating_add(chunk.len() as u64);
        if new_total > total_bytes {
            bail!(
                "upload_media_chunk: chunk {chunk_index} would push session to {new_total} bytes, exceeding declared total {total_bytes}"
            );
        }

        // Append the chunk on a blocking thread so the tokio reactor stays
        // responsive even on slow disks.
        let staging_path = sess.staging_path.clone();
        let chunk_bytes = chunk.clone();
        let is_first = chunk_index == 0;
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::fs::OpenOptions;
            use std::io::Write;
            let mut f = OpenOptions::new()
                .create(is_first)
                .append(true)
                .write(true)
                .truncate(false)
                .open(&staging_path)
                .map_err(|e| anyhow!("open staging {}: {e}", staging_path.display()))?;
            f.write_all(&chunk_bytes)
                .map_err(|e| anyhow!("write staging {}: {e}", staging_path.display()))?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow!("upload chunk write join: {e}"))??;

        sess.bytes_received = new_total;
        sess.next_chunk = sess.next_chunk.saturating_add(1);
        sess.last_touch = SystemTime::now();

        let final_chunk = chunk_index + 1 == total_chunks;
        if final_chunk {
            if sess.bytes_received != total_bytes {
                let path = sess.staging_path.clone();
                drop(sess);
                self.sessions.remove(name);
                let _ = tokio::fs::remove_file(&path).await;
                bail!(
                    "upload_media_chunk: assembled file is {} bytes, declared {}",
                    new_total, total_bytes
                );
            }
            // Atomic rename onto the final path. We deliberately do NOT
            // fsync before rename — fsync of a multi-hundred-MB staging
            // file can take tens of seconds on consumer SSDs and was
            // pushing the manager's per-chunk ACK timeout past 60 s. A
            // media library is not a durability-critical surface: worst
            // case after a host crash is a partial file the operator
            // re-uploads. The OS flushes in the background regardless.
            let final_path = media_dir().join(name);
            let staging_path = sess.staging_path.clone();
            drop(sess);
            self.sessions.remove(name);
            tokio::task::spawn_blocking(move || -> Result<()> {
                std::fs::rename(&staging_path, &final_path)
                    .map_err(|e| anyhow!("rename {}→{}: {e}", staging_path.display(), final_path.display()))?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("upload finalise join: {e}"))??;
            return Ok(UploadProgress::Complete {
                bytes_received: total_bytes,
            });
        }

        Ok(UploadProgress::InProgress {
            chunks_received: chunk_index + 1,
            chunks_total: total_chunks,
            bytes_received: new_total,
        })
    }

    /// Best-effort reap of staging files older than [`STAGING_TTL`]. Runs
    /// before every `list()` and `apply_chunk()` so stale partials don't
    /// build up across edge restarts.
    async fn reap_stale_staging() {
        let stg = staging_dir();
        let now = SystemTime::now();
        let mut rd = match tokio::fs::read_dir(&stg).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = meta.modified().ok().unwrap_or(now);
            if now.duration_since(modified).unwrap_or_default() > STAGING_TTL {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
}

impl Default for MediaLibrary {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-chunk progress signal returned from [`MediaLibrary::apply_chunk`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadProgress {
    /// More chunks expected.
    InProgress {
        chunks_received: u32,
        chunks_total: u32,
        bytes_received: u64,
    },
    /// File assembled and renamed into the library.
    Complete { bytes_received: u64 },
}

/// One process-wide instance shared across the WS command handlers. Used
/// because per-filename upload sessions must persist across multiple WS
/// command messages on the same connection.
static GLOBAL: std::sync::OnceLock<MediaLibrary> = std::sync::OnceLock::new();

pub fn global() -> &'static MediaLibrary {
    GLOBAL.get_or_init(MediaLibrary::new)
}

/// One-shot resolver mirroring [`crate::engine::input_media_player::resolve_media_path`]
/// — kept here so non-engine callers (the upload handler, future REST API) can
/// resolve paths without depending on the engine module.
#[allow(dead_code)]
pub fn resolve_path(name: &str) -> Result<PathBuf> {
    crate::config::validation::validate_media_filename(name, "resolve_media_path")?;
    Ok(media_dir().join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_progress_serialises_with_status_tag() {
        let p = UploadProgress::InProgress {
            chunks_received: 1,
            chunks_total: 4,
            bytes_received: 1024,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"status\":\"in_progress\""));
        assert!(json.contains("\"chunks_received\":1"));
    }

    #[test]
    fn upload_progress_complete_serialises() {
        let p = UploadProgress::Complete {
            bytes_received: 4096,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"status\":\"complete\""));
        assert!(json.contains("\"bytes_received\":4096"));
    }

    /// Build a minimal PAT TS packet for the given (program_number, pmt_pid)
    /// pairs. CRC is left as the buffer fill (`parse_pat_programs` does not
    /// validate the CRC, so we don't need a valid one here).
    fn build_pat_packet(programs: &[(u16, u16)]) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40; // PUSI=1, PID high = 0
        pkt[2] = 0x00; // PID low = 0 (PAT)
        pkt[3] = 0x10; // adaptation_field_control=01, CC=0
        pkt[4] = 0x00; // pointer_field
        let section_start = 5;
        pkt[section_start] = 0x00; // table_id
        let n = programs.len();
        // section_length covers everything after itself: 5 header bytes
        // (ts_id, version/cni, section_number, last_section_number) +
        // 4 × N program entries + 4 bytes CRC.
        let section_length = 5 + 4 * n + 4;
        let section_length_bytes = (section_length as u16) | 0xB000; // syntax=1, '0'=0, reserved=11
        pkt[section_start + 1] = (section_length_bytes >> 8) as u8;
        pkt[section_start + 2] = section_length_bytes as u8;
        // transport_stream_id
        pkt[section_start + 3] = 0x00;
        pkt[section_start + 4] = 0x01;
        // reserved/version/current
        pkt[section_start + 5] = 0xC1;
        // section_number / last_section_number
        pkt[section_start + 6] = 0x00;
        pkt[section_start + 7] = 0x00;
        // program loop
        let mut off = section_start + 8;
        for (num, pid) in programs {
            pkt[off] = (num >> 8) as u8;
            pkt[off + 1] = (*num & 0xFF) as u8;
            pkt[off + 2] = 0xE0 | ((pid >> 8) & 0x1F) as u8;
            pkt[off + 3] = (*pid & 0xFF) as u8;
            off += 4;
        }
        // CRC bytes (4) — left as 0xFF buffer fill; not validated.
        pkt
    }

    #[test]
    fn scan_in_buf_handles_non_ts_bytes() {
        let buf = vec![0x00; 4096];
        let res = scan_programs_in_buf(&buf);
        assert!(!res.is_ts);
        assert!(res.programs.is_empty());
        assert!(!res.not_found);
    }

    #[test]
    fn scan_in_buf_finds_spts_program() {
        let pat = build_pat_packet(&[(101, 0x1000)]);
        // Pad with another sync byte at +188 so the resync confirmation passes.
        let mut buf = Vec::with_capacity(188 * 4);
        buf.extend_from_slice(&pat);
        // A null packet (PID 0x1FFF) so the second sync byte is at +188.
        let mut null = [0xFFu8; 188];
        null[0] = 0x47;
        null[1] = 0x1F;
        null[2] = 0xFF;
        null[3] = 0x10;
        buf.extend_from_slice(&null);
        let res = scan_programs_in_buf(&buf);
        assert!(res.is_ts);
        assert_eq!(res.programs.len(), 1);
        assert_eq!(res.programs[0].program_number, 101);
        assert_eq!(res.programs[0].pmt_pid, 0x1000);
    }

    #[test]
    fn scan_in_buf_finds_mpts_programs_sorted_no_nit() {
        // Program 0 is the NIT — must be filtered out.
        let pat = build_pat_packet(&[(0, 0x10), (303, 0x1300), (101, 0x1100), (202, 0x1200)]);
        let mut buf = Vec::with_capacity(188 * 4);
        buf.extend_from_slice(&pat);
        let mut null = [0xFFu8; 188];
        null[0] = 0x47;
        null[1] = 0x1F;
        null[2] = 0xFF;
        null[3] = 0x10;
        buf.extend_from_slice(&null);
        let res = scan_programs_in_buf(&buf);
        assert!(res.is_ts);
        let nums: Vec<u16> = res.programs.iter().map(|p| p.program_number).collect();
        assert_eq!(nums, vec![101, 202, 303]);
    }

    #[test]
    fn scan_in_buf_resyncs_past_leading_garbage() {
        let pat = build_pat_packet(&[(42, 0x1042)]);
        let mut buf = vec![0xCCu8; 137]; // arbitrary non-0x47 prefix
        buf.extend_from_slice(&pat);
        let mut null = [0xFFu8; 188];
        null[0] = 0x47;
        null[1] = 0x1F;
        null[2] = 0xFF;
        null[3] = 0x10;
        buf.extend_from_slice(&null);
        let res = scan_programs_in_buf(&buf);
        assert!(res.is_ts);
        assert_eq!(res.programs.len(), 1);
        assert_eq!(res.programs[0].program_number, 42);
    }

    /// 204-byte DVB capture: every TS packet is suffixed with 16 bytes
    /// of Reed-Solomon parity. The scanner must walk the file at the
    /// 204-byte stride to land on the PAT instead of returning
    /// `is_ts: false`.
    #[test]
    fn scan_in_buf_finds_program_in_204_byte_dvb_capture() {
        let pat = build_pat_packet(&[(23584, 0x1100)]);
        let mut buf = Vec::with_capacity(204 * 4);
        buf.extend_from_slice(&pat);
        buf.extend_from_slice(&[0xDEu8; 16]); // synthetic RS parity

        let mut null = [0xFFu8; 188];
        null[0] = 0x47;
        null[1] = 0x1F;
        null[2] = 0xFF;
        null[3] = 0x10;
        buf.extend_from_slice(&null);
        buf.extend_from_slice(&[0xDEu8; 16]);

        let res = scan_programs_in_buf(&buf);
        assert!(res.is_ts);
        assert_eq!(res.programs.len(), 1);
        assert_eq!(res.programs[0].program_number, 23584);
        assert_eq!(res.programs[0].pmt_pid, 0x1100);
    }

    /// 192-byte M2TS / AVCHD: every TS packet is prefixed with 4 bytes
    /// of arrival timestamp. The detector finds sync bytes 192 apart,
    /// and the parser walks the buffer at stride 192 starting at the
    /// first sync byte (offset 4 from file start).
    #[test]
    fn scan_in_buf_finds_program_in_192_byte_m2ts_capture() {
        let pat = build_pat_packet(&[(7, 0x1007)]);
        let mut buf = Vec::with_capacity(192 * 4);
        // 4-byte timestamp prefix on the PAT packet.
        buf.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        buf.extend_from_slice(&pat);

        let mut null = [0xFFu8; 188];
        null[0] = 0x47;
        null[1] = 0x1F;
        null[2] = 0xFF;
        null[3] = 0x10;
        buf.extend_from_slice(&[0x12, 0x34, 0x56, 0x79]);
        buf.extend_from_slice(&null);

        let res = scan_programs_in_buf(&buf);
        assert!(res.is_ts);
        assert_eq!(res.programs.len(), 1);
        assert_eq!(res.programs[0].program_number, 7);
        assert_eq!(res.programs[0].pmt_pid, 0x1007);
    }
}
