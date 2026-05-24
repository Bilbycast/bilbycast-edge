// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `bilbycast-ptp-helper` — operator-facing PTP daemon supervisor.
//!
//! Sits between the bilbycast-edge daemon (or operator CLI) and the
//! distro-provided `ptp4l` + `phc2sys`. Reads a small KEY=VALUE config
//! at `/var/lib/bilbycast/ptp.conf` and dispatches the correct
//! `bilbycast-ptp-gm.sh` invocation. In daemon mode (the systemd unit
//! shape), polls the config file's mtime once per second; when the
//! file changes, restarts the underlying ptp4l + phc2sys with the new
//! mode. This sidesteps PolicyKit / dbus — the unit owns the
//! AmbientCapabilities (CAP_NET_RAW / CAP_NET_ADMIN / CAP_SYS_TIME),
//! so the operator can flip modes through the manager UI without
//! sudo at runtime.
//!
//! The helper is intentionally thin: orchestration lives in the
//! shell script which is easier to inspect during a support call.
//! All the helper does is parse the config, watch for changes, and
//! exec the script with the right flags.
//!
//! Subcommands:
//!
//! - `scan` — one-shot Announce listener via the script's `scan`
//!   subcommand. Prints `grandmaster` or `slave-only` to stdout.
//!   Exit 0 always (the recommendation is in stdout).
//! - `apply` — read the config file once + exec
//!   `bilbycast-ptp-gm.sh restart` with the right flags. Used by
//!   ad-hoc operator commands ("apply my edits now"). Exits when
//!   the script exits.
//! - `run` — daemon loop. Initial apply, then poll config mtime at
//!   1 Hz; on change, re-apply. Designed for the systemd unit.
//!
//! Config file format (`/var/lib/bilbycast/ptp.conf`):
//!
//! ```text
//! mode = auto
//! iface = eno4
//! domain = 127
//! priority1 =
//! scan_timeout = 5
//! ```
//!
//! Empty values fall back to script defaults. `mode = off` makes the
//! helper stop ptp4l + phc2sys and idle until the next config write.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

const DEFAULT_CONF_PATH: &str = "/var/lib/bilbycast/ptp.conf";
const DEFAULT_SCRIPT_PATH: &str = "/opt/bilbycast/bin/bilbycast-ptp-gm.sh";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default)]
struct PtpConfig {
    /// `auto` | `grandmaster` | `slave-only` | `off`. Defaults to
    /// `off` when missing/blank, so a freshly-installed node with
    /// no operator action stays out of the PTP namespace entirely.
    mode: String,
    /// NIC name (e.g. `eno4`). Empty → script auto-picks.
    iface: String,
    /// PTP domain number. Empty → script defaults to 127 (SMPTE).
    domain: String,
    /// BMCA priority1 override. Empty → script picks per role
    /// (128 for grandmaster, 255 for slave-only).
    priority1: String,
    /// Seconds the `auto` scan waits for Announce. Empty → 5.
    scan_timeout: String,
}

impl PtpConfig {
    fn from_file(path: &Path) -> std::io::Result<Self> {
        let body = std::fs::read_to_string(path)?;
        let mut cfg = PtpConfig::default();
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((k, v)) = trimmed.split_once('=') else {
                continue;
            };
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match key.as_str() {
                "mode" => cfg.mode = val,
                "iface" | "interface" => cfg.iface = val,
                "domain" => cfg.domain = val,
                "priority1" | "priority_1" => cfg.priority1 = val,
                "scan_timeout" | "scan-timeout" => cfg.scan_timeout = val,
                _ => {} // tolerate unknown keys
            }
        }
        if cfg.mode.is_empty() {
            cfg.mode = "off".to_string();
        }
        Ok(cfg)
    }

    /// Build the `bilbycast-ptp-gm.sh start` arg vector from this
    /// config. Caller prepends the script path + `start`.
    fn to_start_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if !self.iface.is_empty() {
            args.push(self.iface.clone().into());
        }
        args.push("--mode".into());
        args.push(self.mode.clone().into());
        if !self.domain.is_empty() {
            args.push("--domain".into());
            args.push(self.domain.clone().into());
        }
        if !self.priority1.is_empty() {
            args.push("--priority1".into());
            args.push(self.priority1.clone().into());
        }
        if !self.scan_timeout.is_empty() {
            args.push("--scan-timeout".into());
            args.push(self.scan_timeout.clone().into());
        }
        args
    }
}

fn script_path() -> PathBuf {
    std::env::var_os("BILBYCAST_PTP_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SCRIPT_PATH))
}

fn run_script(subcmd: &str, args: &[OsString]) -> std::io::Result<ExitStatus> {
    let script = script_path();
    eprintln!(
        "[bilbycast-ptp-helper] exec: {} {} {}",
        script.display(),
        subcmd,
        args.iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    Command::new(&script)
        .arg(subcmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

fn cmd_scan(args: &HashMap<String, String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut script_args: Vec<OsString> = Vec::new();
    if let Some(iface) = args.get("--iface") {
        script_args.push(iface.clone().into());
    }
    if let Some(d) = args.get("--domain") {
        script_args.push("--domain".into());
        script_args.push(d.clone().into());
    }
    if let Some(t) = args.get("--timeout") {
        script_args.push("--scan-timeout".into());
        script_args.push(t.clone().into());
    }
    let status = run_script("scan", &script_args)?;
    if !status.success() {
        eprintln!("[bilbycast-ptp-helper] scan returned non-zero; stdout recommendation still valid");
    }
    Ok(())
}

fn cmd_apply(conf_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = PtpConfig::from_file(conf_path).map_err(|e| {
        format!(
            "[bilbycast-ptp-helper] cannot read {}: {e} — \
             a freshly-installed node ships with mode=off here; \
             create the file via the manager UI or hand-edit",
            conf_path.display()
        )
    })?;
    eprintln!(
        "[bilbycast-ptp-helper] apply: mode={} iface={} domain={} priority1={} scan_timeout={}",
        cfg.mode, cfg.iface, cfg.domain, cfg.priority1, cfg.scan_timeout
    );
    if cfg.mode == "off" {
        // mode=off — stop the daemons and exit clean.
        let status = run_script("stop", &[])?;
        if !status.success() {
            eprintln!("[bilbycast-ptp-helper] script `stop` returned {status} (non-fatal if nothing was running)");
        }
        return Ok(());
    }
    let status = run_script("restart", &cfg.to_start_args())?;
    if !status.success() {
        return Err(format!("script restart failed: {status}").into());
    }
    Ok(())
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn cmd_run(conf_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "[bilbycast-ptp-helper] daemon: watching {} (poll {:?})",
        conf_path.display(),
        POLL_INTERVAL
    );

    // Initial apply. Don't bail if it fails — operator may fix the
    // config later; the loop will re-apply on next change.
    if let Err(e) = cmd_apply(conf_path) {
        eprintln!("[bilbycast-ptp-helper] initial apply: {e}");
    }
    let mut last_mtime = file_mtime(conf_path);

    loop {
        thread::sleep(POLL_INTERVAL);
        let cur_mtime = file_mtime(conf_path);
        if cur_mtime != last_mtime {
            eprintln!(
                "[bilbycast-ptp-helper] {} mtime changed; re-applying",
                conf_path.display()
            );
            if let Err(e) = cmd_apply(conf_path) {
                eprintln!("[bilbycast-ptp-helper] apply: {e}");
            }
            last_mtime = cur_mtime;
        }
    }
}

fn print_help() {
    println!(
        "bilbycast-ptp-helper — operator PTP supervisor for bilbycast.

Usage:
  bilbycast-ptp-helper scan [--iface IFACE] [--domain N] [--timeout SECONDS]
      One-shot Announce listener via the underlying ptp-gm.sh script.
      Prints 'grandmaster' or 'slave-only' to stdout (recommendation).

  bilbycast-ptp-helper apply [--config PATH]
      Read the config file and exec ptp-gm.sh restart with the right
      flags. Exits when the script exits.

  bilbycast-ptp-helper run [--config PATH]
      Daemon loop. Initial apply, then poll config mtime at 1 Hz; on
      change, re-apply. Designed for the bilbycast-ptp.service systemd
      unit. The unit owns the AmbientCapabilities so the operator can
      change modes via the manager UI without sudo at runtime.

  bilbycast-ptp-helper help | -h | --help
      Print this message.

Defaults:
  --config {default_conf}
  --timeout 5  (seconds)
  Script path: env BILBYCAST_PTP_SCRIPT, else {default_script}

Config file format (KEY=VALUE, # for comments):
  mode         = auto | grandmaster | slave-only | off   (required)
  iface        = eno4                                    (optional; auto-pick when blank)
  domain       = 127                                     (optional; default 127 SMPTE)
  priority1    = 128                                     (optional; per-role default)
  scan_timeout = 5                                       (optional; auto-mode listener)
",
        default_conf = DEFAULT_CONF_PATH,
        default_script = DEFAULT_SCRIPT_PATH,
    );
}

fn parse_flags(args: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(eq) = a.find('=').filter(|_| a.starts_with("--")) {
            out.insert(a[..eq].to_string(), a[eq + 1..].to_string());
            i += 1;
        } else if a.starts_with("--") {
            let key = a.clone();
            let val = args.get(i + 1).cloned().unwrap_or_default();
            out.insert(key, val);
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

fn config_path(flags: &HashMap<String, String>) -> PathBuf {
    flags
        .get("--config")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONF_PATH))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcmd = args.first().cloned().unwrap_or_default();
    let rest: Vec<String> = args.into_iter().skip(1).collect();
    let flags = parse_flags(&rest);
    let conf = config_path(&flags);

    let result: Result<(), Box<dyn std::error::Error>> = match subcmd.as_str() {
        "scan" => cmd_scan(&flags),
        "apply" => cmd_apply(&conf),
        "run" => cmd_run(&conf),
        "help" | "-h" | "--help" | "" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("[bilbycast-ptp-helper] unknown subcommand: {other}");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("[bilbycast-ptp-helper] {e}");
        std::process::exit(1);
    }
}
