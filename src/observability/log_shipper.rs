// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Structured-JSON log shipper.
//!
//! Renders every operational event the edge emits (the same stream that
//! flows over the manager WS `event` channel) as a single-line JSON record
//! and forwards it to a configured sink — file (with size-based rotation),
//! UDP syslog (RFC 5424-style framing), or stdout. Intended for Splunk /
//! Skyline DataMiner / Loki / generic SIEM pickup; the manager WS path is
//! unchanged.
//!
//! ## Key invariants
//!
//! * **Fire-and-forget on the call site.** `JsonLogShipper::ship_event`
//!   never blocks the caller and never propagates errors. Sink I/O happens
//!   on a dedicated writer task; the call site only does a non-blocking
//!   `try_send` into a bounded mpsc, dropping on full. A black-holed
//!   syslog endpoint or a wedged disk cannot stall the data path or the
//!   event mpsc the manager WS client drains.
//! * **Drop-on-full, sampled warn.** When the writer falls behind, drops
//!   are counted and a single `tracing::warn!` is emitted at most once per
//!   second carrying the running drop total — same shape as the manager
//!   event channel's drop counter.
//! * **Format swap is purely cosmetic.** The same fields are rendered for
//!   every format; `Splunk` wraps the envelope in a top-level `{"event":
//!   ...}` and `Dataminer` renames `error_code` → `parameter_id`.
//!
//! See [`docs/events-and-alarms.md`](../../../docs/events-and-alarms.md) for
//! the catalogue of categories and error codes that flow through here.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::config::models::{JsonLogTarget, LogFormat, LoggingConfig};
use crate::manager::events::{Event, EventSeverity};

/// Capacity of the writer-task mpsc. Generous — the writer task only
/// blocks on its sink I/O, and a black-holed sink should drop quickly
/// rather than backing up indefinitely.
const SHIPPER_CHANNEL_CAPACITY: usize = 2048;

/// JSON line shipper handle. Cheaply clonable; clones share a single
/// writer task and a single bounded mpsc.
#[derive(Clone, Debug)]
pub struct JsonLogShipper {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    tx: mpsc::Sender<Vec<u8>>,
    drop_total: AtomicU64,
    drop_pending: AtomicU64,
    last_warn_us: AtomicU64,
    static_fields: StaticFields,
    format: LogFormat,
}

#[derive(Clone, Debug)]
struct StaticFields {
    edge_id: String,
    software_version: &'static str,
    host: String,
}

impl JsonLogShipper {
    /// Build the shipper for the given config and spawn its writer task.
    /// Returns `Ok(None)` if `cfg.json_target` is `None` (shipper disabled).
    pub fn from_config(
        cfg: &LoggingConfig,
        edge_id: String,
        software_version: &'static str,
    ) -> Result<Option<Self>> {
        let Some(target) = cfg.json_target.clone() else {
            return Ok(None);
        };
        let host = hostname();
        let static_fields = StaticFields {
            edge_id,
            software_version,
            host,
        };
        let format = match &target {
            JsonLogTarget::Stdout { format }
            | JsonLogTarget::File { format, .. }
            | JsonLogTarget::Syslog { format, .. } => *format,
        };
        let (tx, rx) = mpsc::channel::<Vec<u8>>(SHIPPER_CHANNEL_CAPACITY);
        spawn_writer_task(target, rx)?;
        let inner = Arc::new(Inner {
            tx,
            drop_total: AtomicU64::new(0),
            drop_pending: AtomicU64::new(0),
            last_warn_us: AtomicU64::new(0),
            static_fields,
            format,
        });
        Ok(Some(Self { inner }))
    }

    /// Render `event` as a JSON line and queue it for the writer task.
    /// Never blocks; on a full queue, drops the line and bumps the sampled
    /// drop counter.
    pub fn ship_event(&self, event: &Event) {
        let line = self.render_line(event);
        if let Err(mpsc::error::TrySendError::Full(_)) = self.inner.tx.try_send(line) {
            self.inner.note_drop();
        }
    }

    fn render_line(&self, event: &Event) -> Vec<u8> {
        let envelope = build_envelope(&self.inner.static_fields, event, self.inner.format);
        let mut line = serde_json::to_vec(&envelope).unwrap_or_else(|_| {
            // Should never happen — the envelope is built from owned, clean
            // serde_json::Value tree. Belt-and-braces fall back.
            br#"{"level":"error","message":"log_shipper render failed"}"#.to_vec()
        });
        line.push(b'\n');
        line
    }
}

impl Inner {
    fn note_drop(&self) {
        self.drop_total.fetch_add(1, Ordering::Relaxed);
        let pending = self.drop_pending.fetch_add(1, Ordering::Relaxed) + 1;
        let now_us = monotonic_us();
        let last = self.last_warn_us.load(Ordering::Relaxed);
        if now_us.saturating_sub(last) >= 1_000_000
            && self
                .last_warn_us
                .compare_exchange(last, now_us, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let pending_now = self.drop_pending.swap(0, Ordering::Relaxed);
            tracing::warn!(
                pending = pending_now,
                total = self.drop_total.load(Ordering::Relaxed),
                capacity = SHIPPER_CHANNEL_CAPACITY,
                "json log shipper queue full — dropping events (pending count includes the trigger event {})",
                pending,
            );
        }
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    ts: String,
    level: &'a str,
    host: &'a str,
    software_version: &'a str,
    edge_id: &'a str,
    flow_id: Option<&'a str>,
    input_id: Option<&'a str>,
    output_id: Option<&'a str>,
    category: &'a str,
    error_code: Option<String>,
    message: &'a str,
    details: Option<Value>,
}

fn build_envelope(static_fields: &StaticFields, event: &Event, format: LogFormat) -> Value {
    let level = match event.severity {
        EventSeverity::Info => "info",
        EventSeverity::Warning => "warning",
        EventSeverity::Critical => "critical",
    };
    let error_code = event
        .details
        .as_ref()
        .and_then(|d| d.get("error_code").and_then(|v| v.as_str()))
        .map(String::from);
    let envelope = Envelope {
        ts: format_rfc3339_now(),
        level,
        host: &static_fields.host,
        software_version: static_fields.software_version,
        edge_id: &static_fields.edge_id,
        flow_id: event.flow_id.as_deref(),
        input_id: event.input_id.as_deref(),
        output_id: event.output_id.as_deref(),
        category: &event.category,
        error_code: error_code.clone(),
        message: &event.message,
        details: event.details.clone(),
    };
    let raw = serde_json::to_value(&envelope).unwrap_or_else(|_| Value::Null);
    apply_format(raw, format, error_code)
}

fn apply_format(raw: Value, format: LogFormat, error_code: Option<String>) -> Value {
    match format {
        LogFormat::Raw => raw,
        LogFormat::Splunk => {
            // Splunk HEC expects the payload wrapped under "event". The HEC
            // forwarder will attach `time` / `host` / `source` itself; we
            // still keep our own `ts` / `host` inside the inner envelope so
            // the line is self-describing if read directly off disk.
            json!({ "event": raw })
        }
        LogFormat::Dataminer => {
            // DataMiner's parameter pipelines key off `parameter_id` — rename
            // `error_code` so it lands on the same parameter slot as the
            // existing edge alarm catalogue.
            let mut obj: Map<String, Value> = match raw {
                Value::Object(m) => m,
                other => return other,
            };
            obj.remove("error_code");
            if let Some(code) = error_code {
                obj.insert("parameter_id".to_string(), Value::String(code));
            }
            Value::Object(obj)
        }
    }
}

fn format_rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, micros * 1000)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn monotonic_us() -> u64 {
    use std::time::Instant;
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(start).as_micros() as u64
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn spawn_writer_task(target: JsonLogTarget, mut rx: mpsc::Receiver<Vec<u8>>) -> Result<()> {
    match target {
        JsonLogTarget::Stdout { .. } => {
            tokio::spawn(async move {
                let mut stdout = tokio::io::stdout();
                while let Some(line) = rx.recv().await {
                    use tokio::io::AsyncWriteExt;
                    if stdout.write_all(&line).await.is_err() {
                        break;
                    }
                }
            });
        }
        JsonLogTarget::File {
            path,
            max_size_mb,
            max_backups,
            ..
        } => {
            let path_buf = PathBuf::from(&path);
            let max_size_bytes = (max_size_mb as u64).saturating_mul(1024 * 1024);
            // Best-effort: create parent dir if missing — this lets a fresh
            // testbed cfg with `path: "/tmp/bilbycast-events.jsonl"` work
            // without an extra `mkdir -p`.
            if let Some(parent) = path_buf.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_buf)
                .with_context(|| format!("opening log_shipper file at {}", path_buf.display()))?;
            let initial_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            tokio::task::spawn_blocking(move || {
                writer_task_file_blocking(rx, path_buf, file, initial_len, max_size_bytes, max_backups)
            });
        }
        JsonLogTarget::Syslog { addr, .. } => {
            let std_socket = std::net::UdpSocket::bind("0.0.0.0:0")
                .with_context(|| "binding ephemeral UDP for syslog log_shipper")?;
            std_socket
                .set_nonblocking(true)
                .with_context(|| "setting UDP socket nonblocking")?;
            let socket = tokio::net::UdpSocket::from_std(std_socket)
                .with_context(|| "wrapping UDP socket for syslog log_shipper")?;
            let target_addr: std::net::SocketAddr = addr
                .parse()
                .with_context(|| format!("parsing syslog addr '{addr}'"))?;
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    // RFC 5424-ish: prepend the local-7 informational PRI
                    // header so off-the-shelf syslog collectors don't reject
                    // the line. The body is still the JSON envelope.
                    let mut framed = Vec::with_capacity(line.len() + 16);
                    framed.extend_from_slice(b"<134>1 ");
                    framed.extend_from_slice(&line);
                    let _ = socket.send_to(&framed, target_addr).await;
                }
            });
        }
    }
    Ok(())
}

fn writer_task_file_blocking(
    mut rx: mpsc::Receiver<Vec<u8>>,
    path: PathBuf,
    mut file: std::fs::File,
    mut size: u64,
    max_size_bytes: u64,
    max_backups: u32,
) {
    while let Some(line) = rx.blocking_recv() {
        if size.saturating_add(line.len() as u64) > max_size_bytes {
            // Best-effort rotation. On any error, log once and keep
            // appending — better to over-grow than to lose events.
            if let Err(e) = rotate_file(&path, max_backups) {
                tracing::warn!(error = %e, path = %path.display(), "log_shipper file rotation failed");
            } else {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    Ok(f) => {
                        file = f;
                        size = 0;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, path = %path.display(), "log_shipper reopen after rotation failed");
                    }
                }
            }
        }
        if let Err(e) = file.write_all(&line) {
            tracing::warn!(error = %e, path = %path.display(), "log_shipper file write failed");
            // Don't break — the next iteration may succeed (transient ENOSPC etc).
            continue;
        }
        size = size.saturating_add(line.len() as u64);
    }
}

fn rotate_file(path: &PathBuf, max_backups: u32) -> std::io::Result<()> {
    if max_backups == 0 {
        // Truncate in place — no backups requested.
        return std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .map(|_| ());
    }
    // Drop the oldest backup if it exists.
    let oldest = path.with_extension(format!(
        "{}.{}",
        path.extension().and_then(|s| s.to_str()).unwrap_or("log"),
        max_backups
    ));
    let _ = std::fs::remove_file(&oldest);
    // Shift backups: path.N → path.(N+1) for N from max-1 down to 1.
    for n in (1..max_backups).rev() {
        let from = backup_path(path, n);
        let to = backup_path(path, n + 1);
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }
    // Rename current → path.1.
    if path.exists() {
        std::fs::rename(path, backup_path(path, 1))?;
    }
    Ok(())
}

fn backup_path(path: &PathBuf, n: u32) -> PathBuf {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("log");
    path.with_extension(format!("{ext}.{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::events::{Event, EventSeverity};
    use serde_json::json;

    fn make_event(category: &str, error_code: Option<&str>) -> Event {
        Event {
            severity: EventSeverity::Critical,
            category: category.to_string(),
            message: "boom".to_string(),
            details: error_code.map(|c| json!({ "error_code": c, "addr": "0.0.0.0:9527" })),
            flow_id: Some("flow-1".to_string()),
            input_id: Some("in-1".to_string()),
            output_id: None,
        }
    }

    fn static_fields() -> StaticFields {
        StaticFields {
            edge_id: "edge-test".to_string(),
            software_version: "0.0.0-test",
            host: "test-host".to_string(),
        }
    }

    #[test]
    fn raw_envelope_carries_all_required_fields() {
        let ev = make_event("port_conflict", Some("port_conflict"));
        let env = build_envelope(&static_fields(), &ev, LogFormat::Raw);
        assert_eq!(env["level"], "critical");
        assert_eq!(env["edge_id"], "edge-test");
        assert_eq!(env["host"], "test-host");
        assert_eq!(env["category"], "port_conflict");
        assert_eq!(env["error_code"], "port_conflict");
        assert_eq!(env["flow_id"], "flow-1");
        assert_eq!(env["input_id"], "in-1");
        assert!(env["ts"].is_string());
        assert_eq!(env["details"]["addr"], "0.0.0.0:9527");
    }

    #[test]
    fn splunk_format_wraps_in_event_envelope() {
        let ev = make_event("port_conflict", Some("port_conflict"));
        let env = build_envelope(&static_fields(), &ev, LogFormat::Splunk);
        assert!(env.get("event").is_some(), "splunk wrapper missing");
        assert_eq!(env["event"]["level"], "critical");
        assert_eq!(env["event"]["error_code"], "port_conflict");
    }

    #[test]
    fn dataminer_format_renames_error_code_to_parameter_id() {
        let ev = make_event("port_conflict", Some("port_conflict"));
        let env = build_envelope(&static_fields(), &ev, LogFormat::Dataminer);
        assert!(env.get("error_code").is_none(), "error_code should be renamed");
        assert_eq!(env["parameter_id"], "port_conflict");
        assert_eq!(env["category"], "port_conflict");
    }

    #[test]
    fn missing_error_code_yields_null_field() {
        let ev = make_event("flow", None);
        let env = build_envelope(&static_fields(), &ev, LogFormat::Raw);
        assert!(env["error_code"].is_null());
    }
}
