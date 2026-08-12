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
    /// Still read, but the value this host holds could not be parsed, so it
    /// was discarded and the layer below answered. Distinct from
    /// [`EnvStatus::Deprecated`] on purpose: telling an operator their
    /// variable "is still honoured" while quietly dropping the value it
    /// carries is the same lie as a unit file that states an intent nothing
    /// applies.
    Unparseable,
}

impl EnvStatus {
    /// Stable wire string for the event's `details.status`. The manager's
    /// Events page filters on it, so it is part of the contract.
    pub fn label(self) -> &'static str {
        match self {
            EnvStatus::Deprecated => "deprecated",
            EnvStatus::Removed => "removed",
            EnvStatus::Unparseable => "unparseable",
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
            EnvStatus::Unparseable => format!(
                "{} is set to {:?}, which could not be read as a value for it — \
                 it has been DISCARDED, not honoured. \
                 Set `{}` in the node's configuration (Manager → node → Configure → Tuning) \
                 and remove the environment variable.",
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
    // No field carries this one across: the egress servo derives its
    // residence from the cushion it is asked to hold. Naming `egress_pacing`
    // here (as an earlier revision did) sent the operator to the mode switch,
    // which cannot express a residence at all.
    (
        "BILBYCAST_EGRESS_RESIDENCE_MS",
        "the per-output `egress_buffer_ms` config field, which the residence is derived from",
    ),
    (
        "BILBYCAST_BOND_FWMARK_BASE",
        "BILBYCAST_BOND_RT_TABLE_BASE / BILBYCAST_BOND_RT_PRIO_BASE",
    ),
    (
        "BILBYCAST_TESTBED_TRACE_EVENTS",
        "RUST_LOG=info,bilbycast_edge::testbed_events=debug (the trace has its own \
         tracing target, so the level selects it — note the leading `info`, since a \
         bare target directive sets the global default to off)",
    ),
    // Not "removed as part of the move to config" — removed because it never
    // did anything, in any release. The node-wide setpoint it carried was
    // consulted only *after* the per-input setpoint had already answered, so
    // no value it held could change behaviour: with no per-input value the
    // publisher never reached the resolver, and with one the per-input value
    // won. Reviving it as a deprecated fallback would have made a knob that
    // was inert for its whole life suddenly start adding ingress latency to
    // every UDP/RTP input on any host whose unit file still pins it. The
    // config field is the one that works.
    (
        "BILBYCAST_INGRESS_BUFFER_MS",
        "tuning.ingress_dejitter_ms (this variable never had any effect)",
    ),
    // Not migrated to config on purpose. Its "off" position selects the
    // whole-file MP4 demux, which holds an entire asset resident — a 4 GiB
    // file is a 4 GiB spike, and that OOM is precisely what the bounded
    // reader was written to fix. A knob whose disabled state is a known
    // out-of-memory must not be reachable from an operator's screen, so it
    // did not earn a `tuning` field; it survives only under
    // `cfg(debug_assertions)` for diagnostics, exactly like
    // `BILBYCAST_TESTBED_SHARED_WALLCLOCK`. A release binary ignores it.
    (
        "BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4",
        "nothing — the bounded incremental reader is unconditional in release \
         builds. The whole-file demux it selected is retained for debug builds \
         only, because its resident-memory cost is the OOM this replaced",
    ),
];

/// Deprecated variables, paired with the `tuning` field that replaces each.
const DEPRECATED: &[(&str, &str)] = &[
    (
        "BILBYCAST_INGRESS_RESIDENCE_MS",
        "tuning.ingress_residence_ms",
    ),
    (
        "BILBYCAST_PROBE_SESSION_LIMITS",
        "tuning.probe_session_limits",
    ),
    ("BILBYCAST_PROBE_4K", "tuning.probe_4k"),
    (
        "BILBYCAST_MEDIA_PLAYER_CONTROLLER",
        "tuning.media_player_controller (or per-input `operator_control`)",
    ),
    (
        "BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES",
        "tuning.media_player_pcr_deadlines (or per-input `pcr_deadlines`)",
    ),
];

/// The tuning values actually in force, after config and the deprecated
/// environment fallback have been folded together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTuning {
    pub ingress_dejitter_ms: Option<u32>,
    pub ingress_residence_ms: Option<u32>,
    pub probe_session_limits: bool,
    pub probe_4k: bool,
    pub media_player_controller: bool,
    pub media_player_pcr_deadlines: bool,
}

impl Default for ResolvedTuning {
    fn default() -> Self {
        Self {
            ingress_dejitter_ms: None,
            ingress_residence_ms: None,
            probe_session_limits: true,
            probe_4k: true,
            media_player_controller: true,
            media_player_pcr_deadlines: true,
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

    // Parse first, THEN record the status. Recording `Deprecated` up front
    // and parsing afterwards told an operator with a typo'd value that their
    // variable was "still honoured" while the value was being dropped on the
    // floor — the same class of lie this module exists to stop.
    let mut env_of = |var: &'static str, parse: &dyn Fn(&str) -> bool| -> Option<String> {
        let value = std::env::var(var).ok()?;
        let replacement = DEPRECATED
            .iter()
            .find(|(name, _)| *name == var)
            .map(|(_, r)| *r)
            .unwrap_or("the node's configuration");
        let parsed = parse(&value);
        found.push(DeprecatedEnvUse {
            var,
            replacement,
            status: if parsed {
                EnvStatus::Deprecated
            } else {
                EnvStatus::Unparseable
            },
            value: value.clone(),
        });
        parsed.then_some(value)
    };

    // `env_flag` accepts anything (unrecognised reads as "on"), matching the
    // parser these two switches have always had — so they can never be
    // unparseable, and passing `|_| true` states that rather than hiding it.
    let env_residence = env_of("BILBYCAST_INGRESS_RESIDENCE_MS", &|v: &str| {
        v.trim().parse::<u32>().is_ok()
    })
    .and_then(|v| v.trim().parse().ok());
    let env_probe = env_of("BILBYCAST_PROBE_SESSION_LIMITS", &|_| true).map(|v| env_flag(&v));
    let env_probe_4k = env_of("BILBYCAST_PROBE_4K", &|_| true).map(|v| env_flag(&v));
    // Same "unrecognised reads as on" parser as the probe switches, which is
    // the one both media-player levers have always used.
    let env_mp_controller =
        env_of("BILBYCAST_MEDIA_PLAYER_CONTROLLER", &|_| true).map(|v| env_flag(&v));
    let env_mp_pcr_deadlines =
        env_of("BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES", &|_| true).map(|v| env_flag(&v));

    // No env fallback for the setpoint: `BILBYCAST_INGRESS_BUFFER_MS` is in
    // `REMOVED`, for the reason given there.
    out.ingress_dejitter_ms = tuning.and_then(|t| t.ingress_dejitter_ms);
    out.ingress_residence_ms = tuning.and_then(|t| t.ingress_residence_ms).or(env_residence);
    out.probe_session_limits = tuning
        .and_then(|t| t.probe_session_limits)
        .or(env_probe)
        .unwrap_or(true);
    out.probe_4k = tuning
        .and_then(|t| t.probe_4k)
        .or(env_probe_4k)
        .unwrap_or(true);
    out.media_player_controller = tuning
        .and_then(|t| t.media_player_controller)
        .or(env_mp_controller)
        .unwrap_or(true);
    out.media_player_pcr_deadlines = tuning
        .and_then(|t| t.media_player_pcr_deadlines)
        .or(env_mp_pcr_deadlines)
        .unwrap_or(true);

    (out, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard for the whole module: these tests mutate process-wide
    /// environment state, so they must not interleave. `cargo test` runs a
    /// crate's tests on N threads by default, and two of these racing
    /// produced the classic 1-in-20 red build.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set the listed variables, run `f`, then remove them again — including
    /// on panic, so one failing assertion cannot leak a variable into every
    /// later test in the process.
    fn with_env<T>(vars: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        match out {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn config_wins_over_the_deprecated_env() {
        // The env fallback must never override an explicit config field —
        // that would reintroduce exactly the silent-no-op trap the move
        // away from environment variables exists to close.
        let tuning = NodeTuningConfig {
            ingress_residence_ms: Some(900),
            probe_session_limits: Some(false),
            ..Default::default()
        };
        let (resolved, found) = with_env(
            &[
                ("BILBYCAST_INGRESS_RESIDENCE_MS", "4321"),
                ("BILBYCAST_PROBE_SESSION_LIMITS", "1"),
            ],
            || resolve_tuning(Some(&tuning)),
        );
        assert_eq!(resolved.ingress_residence_ms, Some(900), "config must win");
        assert!(!resolved.probe_session_limits, "config must win");
        // ...and the now-ineffective variables are still reported, so the
        // operator learns to delete them rather than wondering why editing
        // one changed nothing.
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.status == EnvStatus::Deprecated));
    }

    #[test]
    fn the_env_answers_only_when_the_config_field_is_absent() {
        let (resolved, found) = with_env(
            &[
                ("BILBYCAST_INGRESS_RESIDENCE_MS", "777"),
                ("BILBYCAST_PROBE_4K", "off"),
            ],
            || resolve_tuning(None),
        );
        assert_eq!(resolved.ingress_residence_ms, Some(777));
        assert!(!resolved.probe_4k);
        assert!(resolved.probe_session_limits, "untouched knob keeps its default");
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn the_removed_ingress_setpoint_is_reported_and_never_applied() {
        // BILBYCAST_INGRESS_BUFFER_MS never had an effect in any release.
        // Honouring it now would start adding ingress latency to every
        // UDP/RTP input on a host whose unit file still pins it, so it is
        // reported as removed and the resolved setpoint stays empty.
        let (resolved, found) = with_env(&[("BILBYCAST_INGRESS_BUFFER_MS", "250")], || {
            resolve_tuning(None)
        });
        assert_eq!(resolved.ingress_dejitter_ms, None, "must not be applied");
        let hit = found
            .iter()
            .find(|f| f.var == "BILBYCAST_INGRESS_BUFFER_MS")
            .expect("must still be reported");
        assert_eq!(hit.status, EnvStatus::Removed);
        assert!(hit.replacement.contains("tuning.ingress_dejitter_ms"));
    }

    #[test]
    fn a_clean_environment_reports_nothing() {
        // A false positive here would put a Warning on the manager's Events
        // page for every node in the fleet on every boot.
        let (_, found) = with_env(&[], || resolve_tuning(None));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_unparseable_deprecated_value_is_reported_and_falls_through() {
        // Reporting it as honoured while silently dropping it would be the
        // worst of both worlds: the operator is told the variable still
        // works, and the value it holds is discarded.
        let (resolved, found) = with_env(&[("BILBYCAST_INGRESS_RESIDENCE_MS", "not-a-number")], || {
            resolve_tuning(None)
        });
        assert_eq!(resolved.ingress_residence_ms, None);
        let hit = found
            .iter()
            .find(|f| f.var == "BILBYCAST_INGRESS_RESIDENCE_MS")
            .expect("must be reported even though it could not be parsed");
        assert_eq!(hit.status, EnvStatus::Unparseable);
        assert!(hit.message().contains("could not be read"), "{}", hit.message());
    }

    #[test]
    fn absent_config_and_absent_env_is_the_documented_default() {
        // Must take the guard even though it sets nothing: `resolve_tuning`
        // READS the environment, so without the mutex a sibling test that is
        // mid-`with_env` makes this one fail intermittently. Every test that
        // touches this resolver has to be in the same critical section, not
        // just the ones that write.
        let (resolved, _) = with_env(&[], || resolve_tuning(None));
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
        // This used to dedup the variable NAMES and assert on that, which is
        // not what its name says and not the mistake worth catching: a
        // copy-paste that points two variables at the SAME field sends an
        // operator to a knob that cannot express what they asked for. That
        // had actually happened — BILBYCAST_EGRESS_RESIDENCE_MS pointed at
        // `egress_pacing`, the mode switch, which carries no residence.
        let mut names: Vec<&str> = DEPRECATED.iter().map(|(n, _)| *n).collect();
        names.extend(REMOVED.iter().map(|(n, _)| *n));
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate variable in the tables");

        let mut replacements: Vec<&str> = DEPRECATED.iter().map(|(_, r)| *r).collect();
        replacements.extend(REMOVED.iter().map(|(_, r)| *r));
        let before = replacements.len();
        replacements.sort_unstable();
        replacements.dedup();
        assert_eq!(
            before,
            replacements.len(),
            "two variables point at the same replacement: {replacements:?}"
        );

        for (var, replacement) in DEPRECATED.iter().chain(REMOVED) {
            assert!(!replacement.is_empty(), "{var} has no replacement");
            assert_ne!(var, replacement);
        }
    }

    #[test]
    fn the_deprecated_and_removed_tables_do_not_overlap() {
        // A variable in both would be reported twice with contradictory
        // statuses, and the resolver would honour one it had just called dead.
        for (var, _) in DEPRECATED {
            assert!(
                !REMOVED.iter().any(|(r, _)| r == var),
                "{var} is in both tables"
            );
        }
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(EnvStatus::Deprecated.label(), "deprecated");
        assert_eq!(EnvStatus::Removed.label(), "removed");
    }
}
