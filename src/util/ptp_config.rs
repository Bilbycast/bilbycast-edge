// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Operator PTP configuration — the file the manager UI writes and
//! `bilbycast-ptp-helper` watches.
//!
//! The file format is a tiny KEY=VALUE flat file at
//! `/var/lib/bilbycast/ptp.conf` (path overridable via
//! `BILBYCAST_PTP_CONF_PATH` env var, primarily for tests). The
//! helper polls the mtime at 1 Hz and re-applies on every change —
//! so changing the mode is a single file write away, no systemctl
//! call, no sudo, no manager → systemd plumbing.
//!
//! The schema is intentionally narrow:
//!
//! ```text
//! mode         = auto | grandmaster | slave-only | off
//! iface        = eno4              # blank → script auto-picks
//! domain       = 127               # blank → 127 (SMPTE)
//! priority1    =                   # blank → role default (128 / 255)
//! scan_timeout = 5                 # auto-mode listener window
//! ```
//!
//! Unknown keys are tolerated (forward-compat with future fields).
//! Comments (`#`) and blank lines are skipped.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default location the manager UI writes + the
/// `bilbycast-ptp-helper` daemon polls.
pub const DEFAULT_CONF_PATH: &str = "/var/lib/bilbycast/ptp.conf";

/// Operator-facing PTP mode. Mirrors the `--mode` flag of
/// `bilbycast-ptp-gm.sh` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PtpMode {
    /// Listen for PTP Announce on the chosen interface; slave if a
    /// peer is heard, GM if not. The plug-and-play default for
    /// customer sites where the operator doesn't know in advance
    /// whether a GM is present.
    Auto,
    /// This node IS the master (priority1=128). Other nodes slave to us.
    Grandmaster,
    /// Must slave to an external GM (priority1=255, slaveOnly=1).
    /// Refuses to ever become master under BMCA — for compliance
    /// shops where the customer requires us never to be the time
    /// source.
    SlaveOnly,
    /// No PTP at all. ST 2110 / MXL flows refuse to start. TS flows
    /// run on system wallclock as today. **Default** for a
    /// freshly-installed node so PTP doesn't get on the wire until
    /// the operator opts in.
    #[default]
    Off,
}

impl PtpMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PtpMode::Auto => "auto",
            PtpMode::Grandmaster => "grandmaster",
            PtpMode::SlaveOnly => "slave-only",
            PtpMode::Off => "off",
        }
    }
}

/// Settings persisted to disk for the PTP helper. Every field except
/// `mode` may be empty / unset → script applies its built-in default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PtpSettings {
    pub mode: PtpMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub iface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority1: Option<u8>,
    /// Seconds the `auto` listener waits for Announce. Bounded
    /// 1..=60 at runtime; the manager UI's slider should clamp too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_timeout: Option<u8>,
}

impl PtpSettings {
    /// Render to the on-disk KEY=VALUE format. Stable enough that
    /// hand-editors and the manager UI can both produce/consume it.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# bilbycast PTP helper config — managed by the bilbycast-edge\n\
             # manager UI Time page. Hand-edits are picked up on the next\n\
             # 1 Hz mtime poll. See bilbycast-edge/docs/ptp.md.\n\n",
        );
        out.push_str(&format!("mode         = {}\n", self.mode.as_str()));
        out.push_str(&format!("iface        = {}\n", self.iface));
        out.push_str(&format!(
            "domain       = {}\n",
            self.domain.map(|d| d.to_string()).unwrap_or_default()
        ));
        out.push_str(&format!(
            "priority1    = {}\n",
            self.priority1.map(|p| p.to_string()).unwrap_or_default()
        ));
        out.push_str(&format!(
            "scan_timeout = {}\n",
            self.scan_timeout.map(|s| s.to_string()).unwrap_or_default()
        ));
        out
    }

    /// Parse the on-disk format. Unknown keys are tolerated.
    pub fn parse(body: &str) -> Self {
        let mut out = PtpSettings::default();
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
                "mode" => {
                    out.mode = match val.to_ascii_lowercase().as_str() {
                        "auto" => PtpMode::Auto,
                        "grandmaster" | "gm" | "master" => PtpMode::Grandmaster,
                        "slave-only" | "slave" => PtpMode::SlaveOnly,
                        _ => PtpMode::Off,
                    };
                }
                "iface" | "interface" => out.iface = val,
                "domain" => out.domain = val.parse().ok(),
                "priority1" | "priority_1" => out.priority1 = val.parse().ok(),
                "scan_timeout" | "scan-timeout" => out.scan_timeout = val.parse().ok(),
                _ => {} // tolerate unknown keys
            }
        }
        out
    }

    /// Bound `scan_timeout` to a sane range so a mis-set value (e.g.
    /// "0") doesn't render the helper unable to scan. Clamps the
    /// stored value silently — manager UI should pre-clamp too so
    /// the operator sees the clamped value.
    pub fn normalised(mut self) -> Self {
        if let Some(t) = self.scan_timeout {
            if !(1..=60).contains(&t) {
                self.scan_timeout = Some(t.clamp(1, 60));
            }
        }
        self
    }
}

/// Resolve the config file path. Honours `BILBYCAST_PTP_CONF_PATH`
/// for tests.
pub fn config_path() -> PathBuf {
    std::env::var("BILBYCAST_PTP_CONF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONF_PATH))
}

/// Read the current settings from disk. Returns `Off` defaults on
/// any read / parse error — matches the helper's tolerance and
/// keeps the surface predictable when the file doesn't exist
/// (fresh install).
pub fn load() -> PtpSettings {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> PtpSettings {
    std::fs::read_to_string(path)
        .map(|s| PtpSettings::parse(&s).normalised())
        .unwrap_or_default()
}

/// Persist the settings to disk atomically (write + rename) so the
/// helper's mtime-watcher never sees a torn / half-written file.
pub fn save(settings: &PtpSettings) -> std::io::Result<()> {
    save_to(&config_path(), settings)
}

pub fn save_to(path: &Path, settings: &PtpSettings) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ptp config path has no parent dir",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(".ptp.conf.tmp");
    let rendered = settings.render();
    std::fs::write(&tmp, &rendered)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_off() {
        let original = PtpSettings::default();
        let rendered = original.render();
        let parsed = PtpSettings::parse(&rendered);
        assert_eq!(parsed.mode, PtpMode::Off);
        assert!(parsed.iface.is_empty());
    }

    #[test]
    fn roundtrip_auto_with_iface_and_domain() {
        let original = PtpSettings {
            mode: PtpMode::Auto,
            iface: "eno4".to_string(),
            domain: Some(127),
            priority1: None,
            scan_timeout: Some(10),
        };
        let rendered = original.render();
        let parsed = PtpSettings::parse(&rendered);
        assert_eq!(parsed.mode, PtpMode::Auto);
        assert_eq!(parsed.iface, "eno4");
        assert_eq!(parsed.domain, Some(127));
        assert_eq!(parsed.scan_timeout, Some(10));
    }

    #[test]
    fn parse_tolerates_unknown_keys_and_comments() {
        let body = "# operator comment\n\
                    mode = grandmaster\n\
                    \n\
                    iface = eno1np0\n\
                    new_future_field = whatever\n\
                    domain = 0\n";
        let parsed = PtpSettings::parse(body);
        assert_eq!(parsed.mode, PtpMode::Grandmaster);
        assert_eq!(parsed.iface, "eno1np0");
        assert_eq!(parsed.domain, Some(0));
    }

    #[test]
    fn slave_only_aliases() {
        for s in ["slave-only", "slave", "Slave-Only"] {
            let body = format!("mode = {s}\n");
            assert_eq!(PtpSettings::parse(&body).mode, PtpMode::SlaveOnly);
        }
    }

    #[test]
    fn unknown_mode_falls_back_to_off() {
        let parsed = PtpSettings::parse("mode = sometimes\n");
        assert_eq!(parsed.mode, PtpMode::Off);
    }

    #[test]
    fn scan_timeout_clamps_to_sane_range() {
        let parsed = PtpSettings::parse("mode = auto\nscan_timeout = 0\n").normalised();
        assert_eq!(parsed.scan_timeout, Some(1));
        let parsed = PtpSettings::parse("mode = auto\nscan_timeout = 120\n").normalised();
        assert_eq!(parsed.scan_timeout, Some(60));
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ptp.conf");
        let original = PtpSettings {
            mode: PtpMode::Grandmaster,
            iface: "eno4".to_string(),
            domain: Some(127),
            priority1: Some(64),
            scan_timeout: None,
        };
        save_to(&path, &original).unwrap();
        let reloaded = load_from(&path);
        assert_eq!(reloaded.mode, PtpMode::Grandmaster);
        assert_eq!(reloaded.iface, "eno4");
        assert_eq!(reloaded.domain, Some(127));
        assert_eq!(reloaded.priority1, Some(64));
        assert_eq!(reloaded.scan_timeout, None);
    }
}
