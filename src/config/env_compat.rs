//! Compatibility bridge for environment variables that have moved into
//! `config.json` (and therefore onto the manager UI).
//!
//! Three things live here, and the distinction matters operationally:
//!
//! * **Deprecated** — the variable still works, as a fallback *below* the
//!   config field that replaced it. The node boots identically to before,
//!   and the operator gets one Warning telling them what to set instead.
//! * **Removed** — the variable does nothing. Silence here would be the
//!   worst outcome: a host whose systemd unit still pins
//!   `BILBYCAST_INGRESS_RESIDENCE_MS` after the field moved would run on
//!   defaults while its unit file says otherwise, and nobody would know
//!   until a stream misbehaved. So a removed variable that is still set is
//!   reported exactly as loudly as a deprecated one.
//! * **Host-level** — everything not listed here. CPU pinning, `mlockall`,
//!   the SO_TXTIME/qdisc tier, the bond routing-table bases and the various
//!   path overrides are properties of the *machine*, not of the media
//!   configuration, and they correctly stay environment variables.
//!
//! The report is surfaced twice: as `tracing::warn!` lines (for an operator
//! reading `journalctl` during a boot) and as a single Warning
//! `deprecated_env_var` event per variable (for the fleet operator who
//! never logs into the box and lives on the manager's Events page).

use crate::config::models::NodeTuningConfig;

/// What happened to a variable an operator still has set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvStatus {
    /// Still honoured this release, below the config field. Will be removed.
    Deprecated,
    /// Read by nothing. Set on this host and doing nothing at all.
    Removed,
}

impl EnvStatus {
    /// Stable wire string for the event's `details.status`. The manager's
    /// Events page filters on it, so it is part of the contract.
    pub fn label(self) -> &'static str {
        match self {
            EnvStatus::Deprecated => "deprecated",
            EnvStatus::Removed => "removed",
        }
    }
}

/// One variable that is set in this process's environment and shouldn't be.
#[derive(Debug, Clone)]
pub struct DeprecatedEnvUse {
    /// The variable name, e.g. `BILBYCAST_INGRESS_BUFFER_MS`.
    pub var: &'static str,
    /// What to set instead, in operator terms, e.g.
    /// `tuning.ingress_dejitter_ms`.
    pub replacement: &'static str,
    pub status: EnvStatus,
    /// The value found. Included so the operator can copy it into the
    /// config field verbatim; none of these carry a secret.
    pub value: String,
}

impl DeprecatedEnvUse {
    /// One-line operator-facing sentence, shared by the log line and the
    /// event message.
    pub fn message(&self) -> String {
        match self.status {
            EnvStatus::Deprecated => format!(
                "{} is deprecated and will be removed in a future release. \
                 It is still honoured for now, below the config. \
                 Set `{}` in the node's configuration (Manager → node → Configure → Tuning) \
                 and remove the environment variable.",
                self.var, self.replacement
            ),
            EnvStatus::Removed => format!(
                "{} has been removed and does nothing — this host still sets it to {:?}, \
                 so its intent is NOT being applied. Use `{}` instead.",
                self.var, self.value, self.replacement
            ),
        }
    }
}

/// Variables that were removed outright. Set here so an operator who still
/// pins one is told rather than left with a unit file that lies.
const REMOVED: &[(&str, &str)] = &[
    (
        "BILBYCAST_ENABLE_SO_TXTIME",
        "BILBYCAST_ENABLE_TXTIME (the alias was collapsed onto one name)",
    ),
    (
        "BILBYCAST_EGRESS_PACING",
        "the per-output `egress_pacing` config field",
    ),
    (
        "BILBYCAST_EGRESS_BUFFER_MS",
        "the per-output `egress_buffer_ms` config field",
    ),
    (
        "BILBYCAST_EGRESS_RESIDENCE_MS",
        "the per-output `egress_pacing` config field",
    ),
    (
        "BILBYCAST_BOND_FWMARK_BASE",
        "BILBYCAST_BOND_RT_TABLE_BASE / BILBYCAST_BOND_RT_PRIO_BASE",
    ),
];

/// Deprecated variables, paired with the `tuning` field that replaces each.
const DEPRECATED: &[(&str, &str)] = &[
    ("BILBYCAST_INGRESS_BUFFER_MS", "tuning.ingress_dejitter_ms"),
    (
        "BILBYCAST_INGRESS_RESIDENCE_MS",
        "tuning.ingress_residence_ms",
    ),
    (
        "BILBYCAST_PROBE_SESSION_LIMITS",
        "tuning.probe_session_limits",
    ),
    ("BILBYCAST_PROBE_4K", "tuning.probe_4k"),
];

/// The tuning values actually in force, after config and the deprecated
/// environment fallback have been folded together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTuning {
    pub ingress_dejitter_ms: Option<u32>,
    pub ingress_residence_ms: Option<u32>,
    pub probe_session_limits: bool,
    pub probe_4k: bool,
}

impl Default for ResolvedTuning {
    fn default() -> Self {
        Self {
            ingress_dejitter_ms: None,
            ingress_residence_ms: None,
            probe_session_limits: true,
            probe_4k: true,
        }
    }
}

/// Parse the `0`/`false`/`no`/`off` family the probe switches have always
/// accepted. Anything else — including an unparseable value — is `true`,
/// matching the previous behaviour exactly.
fn env_flag(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Resolve the effective tuning, and report every stale environment
/// variable this host still sets.
///
/// Config always wins: a field present in `tuning` is used even when the
/// matching environment variable is also set, and the variable is still
/// reported so the operator knows to delete the now-ineffective one.
pub fn resolve_tuning(tuning: Option<&NodeTuningConfig>) -> (ResolvedTuning, Vec<DeprecatedEnvUse>) {
    let mut out = ResolvedTuning::default();
    let mut found = Vec::new();

    for (var, replacement) in REMOVED {
        if let Ok(value) = std::env::var(var) {
            found.push(DeprecatedEnvUse {
                var,
                replacement,
                status: EnvStatus::Removed,
                value,
            });
        }
    }

    let mut env_of = |var: &'static str| -> Option<String> {
        let value = std::env::var(var).ok()?;
        let replacement = DEPRECATED
            .iter()
            .find(|(name, _)| *name == var)
            .map(|(_, r)| *r)
            .unwrap_or("the node's configuration");
        found.push(DeprecatedEnvUse {
            var,
            replacement,
            status: EnvStatus::Deprecated,
            value: value.clone(),
        });
        Some(value)
    };

    let env_dejitter = env_of("BILBYCAST_INGRESS_BUFFER_MS").and_then(|v| v.trim().parse().ok());
    let env_residence =
        env_of("BILBYCAST_INGRESS_RESIDENCE_MS").and_then(|v| v.trim().parse().ok());
    let env_probe = env_of("BILBYCAST_PROBE_SESSION_LIMITS").map(|v| env_flag(&v));
    let env_probe_4k = env_of("BILBYCAST_PROBE_4K").map(|v| env_flag(&v));

    out.ingress_dejitter_ms = tuning.and_then(|t| t.ingress_dejitter_ms).or(env_dejitter);
    out.ingress_residence_ms = tuning.and_then(|t| t.ingress_residence_ms).or(env_residence);
    out.probe_session_limits = tuning
        .and_then(|t| t.probe_session_limits)
        .or(env_probe)
        .unwrap_or(true);
    out.probe_4k = tuning
        .and_then(|t| t.probe_4k)
        .or(env_probe_4k)
        .unwrap_or(true);

    (out, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_wins_over_the_deprecated_env() {
        // The env fallback must never override an explicit config field —
        // that would reintroduce exactly the silent-no-op trap the move
        // away from environment variables exists to close.
        let tuning = NodeTuningConfig {
            ingress_dejitter_ms: Some(120),
            probe_session_limits: Some(false),
            ..Default::default()
        };
        let (resolved, _) = resolve_tuning(Some(&tuning));
        assert_eq!(resolved.ingress_dejitter_ms, Some(120));
        assert!(!resolved.probe_session_limits);
    }

    #[test]
    fn absent_config_and_absent_env_is_the_documented_default() {
        let (resolved, _) = resolve_tuning(None);
        assert_eq!(resolved, ResolvedTuning::default());
        assert!(resolved.probe_session_limits);
        assert!(resolved.probe_4k);
        assert_eq!(resolved.ingress_dejitter_ms, None);
    }

    #[test]
    fn env_flag_matches_the_previous_parser() {
        for off in ["0", "false", "no", "off", "OFF", " False "] {
            assert!(!env_flag(off), "{off} should read as off");
        }
        for on in ["1", "true", "yes", "", "garbage"] {
            assert!(env_flag(on), "{on} should read as on");
        }
    }

    #[test]
    fn a_removed_variable_reports_that_it_does_nothing() {
        let use_ = DeprecatedEnvUse {
            var: "BILBYCAST_EGRESS_PACING",
            replacement: "the per-output `egress_pacing` config field",
            status: EnvStatus::Removed,
            value: "servo".into(),
        };
        let msg = use_.message();
        assert!(msg.contains("has been removed"), "{msg}");
        assert!(msg.contains("NOT being applied"), "{msg}");
    }

    #[test]
    fn every_listed_variable_names_a_distinct_replacement() {
        // A copy-paste that pointed two variables at the same field would
        // send an operator to the wrong knob.
        let mut names: Vec<&str> = DEPRECATED.iter().map(|(n, _)| *n).collect();
        names.extend(REMOVED.iter().map(|(n, _)| *n));
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate variable in the tables");
        for (var, replacement) in DEPRECATED {
            assert!(!replacement.is_empty(), "{var} has no replacement");
            assert_ne!(var, replacement);
        }
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(EnvStatus::Deprecated.label(), "deprecated");
        assert_eq!(EnvStatus::Removed.label(), "removed");
    }
}
