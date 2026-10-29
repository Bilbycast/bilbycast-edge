//! Boot-time diagnosis of `/dev/blackmagic/` for the DeckLink probe (#168).
//!
//! `decklink_rs::enumerate_devices()` reports only a count, and when that
//! count is zero the SDK says nothing about why. At least three unrelated
//! causes end there, and the old single "is a card fitted?" warning sent the
//! operator to the hardware for all of them:
//!
//! 1. the service's **device cgroup** denies the nodes — under the packaged
//!    systemd unit, its `DeviceAllow=` list (`open(2)` fails `EPERM`);
//! 2. the nodes are present and openable, but the card's **firmware** does not
//!    match the driver, which only `dmesg` says;
//! 3. there are **no nodes**: no card fitted, or no kernel driver loaded.
//!
//! This module looks at the device nodes themselves so the boot log names
//! which one it is. It also catches a partial denial — some cards enumerate,
//! but nodes beyond the unit's list (a second Quad 2's `io8`…) do not.
//!
//! Each node is checked with `access(2)`, not `open(2)`. Both reach the same
//! `inode_permission()` verdict — the DAC / LSM checks (`EACCES`), then the
//! device-cgroup hook `devcgroup_inode_permission()` (`EPERM`) — but `access`
//! never calls into the blackmagic driver's open handler.
//!
//! Plain std + libc, compiled under `cfg(any(test, feature = "sdi-decklink"))`
//! so the default `cargo test` and `cargo clippy --all-targets` exercise it,
//! although only the `sdi-decklink` build's boot probe
//! (`super::domain::DecklinkDeviceManager::probe`) calls it.

use std::cmp::Ordering;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

/// Where Desktop Video creates the cards' device nodes.
pub const BLACKMAGIC_DEV_DIR: &str = "/dev/blackmagic";

/// Node-name prefixes the packaged unit (`packaging/bilbycast-edge.service`)
/// grants by path, each as `{prefix}0` … `{prefix}{UNIT_GRANTED_PER_PREFIX - 1}`:
/// `io` for multi-channel cards (DeckLink Quad 2), `dv` for single-channel
/// DeckLink SDI cards. The remedy text in [`diagnose`] is built from these, and
/// a test pins the unit's `DeviceAllow=` lines to the same list, so the log's
/// advice cannot drift from what the unit actually grants.
pub const UNIT_GRANTED_PREFIXES: [&str; 2] = ["io", "dv"];

/// How many nodes per prefix the packaged unit grants (see
/// [`UNIT_GRANTED_PREFIXES`]).
pub const UNIT_GRANTED_PER_PREFIX: usize = 8;

/// Bound on directory entries examined, so a pathological directory cannot
/// stall the boot probe. A fully populated host has a few dozen at most.
const MAX_SCANNED_ENTRIES: usize = 256;

/// Bound on node names listed per finding, so the log line stays one line.
const MAX_LISTED_NODES: usize = 16;

/// Whether this process may open a device node read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAccess {
    /// `access(R_OK | W_OK)` succeeded.
    Accessible,
    /// `EPERM`: the device cgroup refused it — under systemd, the unit's
    /// `DeviceAllow=` list (`DevicePolicy=closed`); in a container, the
    /// runtime's device rules.
    DeviceCgroupDenied,
    /// `EACCES`: the file mode and the process's groups, or an LSM
    /// (AppArmor / SELinux), refused it.
    PermissionDenied,
    /// Any other errno; the node's state is unknown.
    Other(i32),
}

/// Map an `access(2)` errno to [`NodeAccess`].
pub fn classify_access_errno(errno: i32) -> NodeAccess {
    match errno {
        libc::EPERM => NodeAccess::DeviceCgroupDenied,
        libc::EACCES => NodeAccess::PermissionDenied,
        other => NodeAccess::Other(other),
    }
}

/// One character-device node found in [`BLACKMAGIC_DEV_DIR`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevNode {
    /// File name within the directory, e.g. `dv0`.
    pub name: String,
    pub access: NodeAccess,
}

/// The character-device entries of `dir` with this process's access to each,
/// sorted naturally (`io2` before `io10`). Symlinks are followed; anything
/// that is not a character device (or a dangling link) is skipped. A missing
/// directory is `Ok(empty)` — it is a finding, not an error.
pub fn scan(dir: &Path) -> io::Result<Vec<DevNode>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries.take(MAX_SCANNED_ENTRIES).flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_char_device() {
            continue;
        }
        out.push(DevNode {
            name: entry.file_name().to_string_lossy().into_owned(),
            access: check_rw(&path),
        });
    }
    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    Ok(out)
}

/// `access(path, R_OK | W_OK)`, classified. See the module doc for why this is
/// `access` and not `open`.
fn check_rw(path: &Path) -> NodeAccess {
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return NodeAccess::Other(libc::EINVAL);
    };
    // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the
    // call; access(2) only reads it.
    if unsafe { libc::access(c_path.as_ptr(), libc::R_OK | libc::W_OK) } == 0 {
        return NodeAccess::Accessible;
    }
    classify_access_errno(io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// Compare by non-digit prefix, then by the numeric value of the trailing
/// digits, then by the whole name as a tie-break.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a_prefix, a_digits) = split_trailing_digits(a);
    let (b_prefix, b_digits) = split_trailing_digits(b);
    a_prefix
        .cmp(b_prefix)
        .then_with(|| {
            let a_num = a_digits.trim_start_matches('0');
            let b_num = b_digits.trim_start_matches('0');
            a_num.len().cmp(&b_num.len()).then_with(|| a_num.cmp(b_num))
        })
        .then_with(|| a.cmp(b))
}

fn split_trailing_digits(s: &str) -> (&str, &str) {
    let prefix = s.trim_end_matches(|c: char| c.is_ascii_digit());
    (prefix, &s[prefix.len()..])
}

/// Up to [`MAX_LISTED_NODES`] labels joined with `, `, then `and N more`.
fn list_labels(labels: &[String]) -> String {
    let mut out = labels
        .iter()
        .take(MAX_LISTED_NODES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if labels.len() > MAX_LISTED_NODES {
        let _ = write!(out, " and {} more", labels.len() - MAX_LISTED_NODES);
    }
    out
}

/// `io0-io7 and dv0-dv7`: what the packaged unit grants, from the same
/// constants the unit-pin test checks.
fn unit_granted_ranges() -> String {
    let last = UNIT_GRANTED_PER_PREFIX - 1;
    UNIT_GRANTED_PREFIXES
        .iter()
        .map(|p| format!("{p}0-{p}{last}"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// What to log about the device nodes, given how many devices the DeckLink API
/// enumerated and the result of [`scan`]. Pure. `Some(message)` to warn,
/// `None` when nothing is wrong.
///
/// **Invariant:** `enumerated == 0` always yields `Some` — the zero-device
/// case never goes silent, whatever the scan found. With devices enumerated
/// it warns only when some nodes are denied (`EPERM` / `EACCES`): those are
/// ports the API could not reach. A node that merely could not be checked
/// (any other errno) is not, on its own, worth a warning on a host whose
/// cards enumerate.
pub fn diagnose(enumerated: usize, scan: &io::Result<Vec<DevNode>>) -> Option<String> {
    let dir = BLACKMAGIC_DEV_DIR;
    let nodes = match scan {
        Ok(nodes) => nodes,
        Err(e) if enumerated == 0 => {
            // The device cgroup never gates a directory read (it only acts on
            // char/block inodes), so this is DAC, an LSM, or a /dev that is
            // not the host's.
            return Some(format!(
                "The DeckLink API enumerated 0 devices, and {dir}/ could not be read ({e}), \
                 so its device nodes could not be checked. Check the permissions on {dir}/ \
                 (and, in a container, that /dev is the host's), then `dmesg | grep -i blackmagic`."
            ));
        }
        Err(_) => return None,
    };

    let total = nodes.len();
    let names_with = |want: NodeAccess| -> Vec<String> {
        nodes
            .iter()
            .filter(|n| n.access == want)
            .map(|n| n.name.clone())
            .collect()
    };
    let cgroup_denied = names_with(NodeAccess::DeviceCgroupDenied);
    let perm_denied = names_with(NodeAccess::PermissionDenied);
    let unchecked: Vec<String> = nodes
        .iter()
        .filter_map(|n| match n.access {
            NodeAccess::Other(errno) => Some(format!("{} (os error {errno})", n.name)),
            _ => None,
        })
        .collect();
    let accessible = names_with(NodeAccess::Accessible);
    let denied = cgroup_denied.len() + perm_denied.len();

    if enumerated > 0 && denied == 0 {
        return None;
    }
    if enumerated == 0 && total == 0 {
        return Some(format!(
            "The DeckLink API is installed but enumerated 0 devices, and {dir}/ holds no \
             device nodes. No DeckLink card is fitted, the Desktop Video kernel driver is not \
             loaded (`lsmod | grep blackmagic`; after a kernel upgrade DKMS rebuilds it on the \
             next boot), or the nodes are not visible to this process (e.g. a container \
             started without them passed through)."
        ));
    }
    if enumerated == 0 && accessible.len() == total {
        return Some(format!(
            "The DeckLink API enumerated 0 devices although {total} device node(s) in {dir}/ \
             are present and accessible to this process ({}). Check `dmesg | grep -i \
             blackmagic` for a firmware version mismatch (`DesktopVideoUpdateTool --list`; \
             update and reboot) or a Desktop Video driver/library version mismatch.",
            list_labels(&accessible)
        ));
    }

    let mut msg = String::new();
    if !cgroup_denied.is_empty() {
        let _ = write!(
            msg,
            "{} of {total} device node(s) in {dir}/ are denied to this process by its device \
             cgroup (EPERM): {}. ",
            cgroup_denied.len(),
            list_labels(&cgroup_denied)
        );
    }
    if !perm_denied.is_empty() {
        let _ = write!(
            msg,
            "{} of {total} device node(s) in {dir}/ are denied by file permissions or an \
             AppArmor/SELinux policy (EACCES): {}. ",
            perm_denied.len(),
            list_labels(&perm_denied)
        );
    }
    if !unchecked.is_empty() {
        let _ = write!(
            msg,
            "{} of {total} device node(s) in {dir}/ could not be checked: {}. ",
            unchecked.len(),
            list_labels(&unchecked)
        );
    }

    if enumerated > 0 {
        let _ = write!(
            msg,
            "{enumerated} device(s) enumerated; ports behind the denied node(s) are missing. "
        );
    } else if accessible.is_empty() && unchecked.is_empty() {
        // Every node is denied, so the denial alone explains the zero.
        msg.push_str("This is why the DeckLink API enumerated 0 devices. ");
    } else {
        msg.push_str("The DeckLink API enumerated 0 devices. ");
        if !accessible.is_empty() {
            // A denied node does not explain why the accessible ones did not
            // enumerate: the Quad 2 firmware-mismatch case in #168 has every
            // node accessible and still enumerates nothing.
            let _ = write!(
                msg,
                "{} other node(s) are present and accessible ({}), so this may not be the \
                 whole cause: check `dmesg | grep -i blackmagic` for a firmware version \
                 mismatch (`DesktopVideoUpdateTool --list`; update and reboot). ",
                accessible.len(),
                list_labels(&accessible)
            );
        } else if denied == 0 {
            msg.push_str("Check `dmesg | grep -i blackmagic`. ");
        }
    }

    if !cgroup_denied.is_empty() {
        let _ = write!(
            msg,
            "Under the packaged systemd unit this is its DeviceAllow= list: grant each node in \
             a drop-in (`sudo systemctl edit bilbycast-edge`, one \
             `DeviceAllow={dir}/<node> rw` line per node) and restart the service, or refresh \
             the unit with `install-edge.sh --upgrade-installer` (it grants {}, and also moves \
             this node to the latest release), then restart. If the unit already grants these \
             nodes, they appeared after the service started: restart it. In a container, check \
             the runtime's device rules. See docs/sdi.md, Checklist C. ",
            unit_granted_ranges()
        );
    }
    if !perm_denied.is_empty() {
        let _ = write!(
            msg,
            "Check `ls -l {dir}/`, the service user's groups and any AppArmor/SELinux policy. "
        );
    }
    msg.truncate(msg.trim_end().len());
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(prefix: &str, range: std::ops::Range<usize>, access: NodeAccess) -> Vec<DevNode> {
        range
            .map(|i| DevNode {
                name: format!("{prefix}{i}"),
                access,
            })
            .collect()
    }

    fn ok(v: Vec<DevNode>) -> io::Result<Vec<DevNode>> {
        Ok(v)
    }

    /// The comma-separated list that follows `marker` up to the next `. `.
    fn listed_after<'a>(msg: &'a str, marker: &str) -> Vec<&'a str> {
        let start = msg
            .find(marker)
            .unwrap_or_else(|| panic!("no {marker:?} in {msg}"));
        let rest = &msg[start + marker.len()..];
        let end = rest.find(". ").unwrap_or(rest.len());
        rest[..end].split(", ").collect()
    }

    #[test]
    fn zero_devices_all_nodes_denied_names_device_filter_and_each_node() {
        let msg = diagnose(0, &ok(nodes("dv", 0..4, NodeAccess::DeviceCgroupDenied))).unwrap();
        for needle in [
            "EPERM",
            "DeviceAllow",
            "dv0",
            "dv3",
            "--upgrade-installer",
            "enumerated 0",
        ] {
            assert!(msg.contains(needle), "{needle:?} missing from: {msg}");
        }
        assert!(
            msg.contains("This is why the DeckLink API enumerated 0 devices"),
            "{msg}"
        );
        assert_eq!(
            listed_after(&msg, "(EPERM): "),
            ["dv0", "dv1", "dv2", "dv3"]
        );
        assert!(!msg.contains("is a card fitted"), "{msg}");
    }

    #[test]
    fn eperm_remedy_puts_drop_in_first_and_says_restart_when_already_granted() {
        let msg = diagnose(0, &ok(nodes("dv", 0..4, NodeAccess::DeviceCgroupDenied))).unwrap();
        let drop_in = msg
            .find("systemctl edit bilbycast-edge")
            .expect("drop-in remedy");
        let refresh = msg.find("--upgrade-installer").expect("refresh remedy");
        assert!(drop_in < refresh, "drop-in must come first: {msg}");
        assert!(
            msg.contains("also moves this node to the latest release"),
            "{msg}"
        );
        assert!(
            msg.contains("appeared after the service started: restart it"),
            "{msg}"
        );
    }

    #[test]
    fn eperm_remedy_names_exactly_what_the_unit_grants() {
        let msg = diagnose(0, &ok(nodes("dv", 0..1, NodeAccess::DeviceCgroupDenied))).unwrap();
        let last = UNIT_GRANTED_PER_PREFIX - 1;
        for p in UNIT_GRANTED_PREFIXES {
            assert!(
                msg.contains(&format!("{p}0-{p}{last}")),
                "{p} range missing: {msg}"
            );
        }
        assert!(msg.contains("grants io0-io7 and dv0-dv7"), "{msg}");
    }

    #[test]
    fn zero_devices_nodes_accessible_points_at_firmware_not_permissions() {
        let msg = diagnose(0, &ok(nodes("io", 0..8, NodeAccess::Accessible))).unwrap();
        assert!(msg.contains("dmesg"), "{msg}");
        assert!(msg.contains("firmware"), "{msg}");
        assert!(msg.contains("present and accessible"), "{msg}");
        assert!(!msg.contains("DeviceAllow=/dev/blackmagic"), "{msg}");
        assert!(!msg.contains("not a permissions problem"), "{msg}");
    }

    #[test]
    fn zero_devices_mixed_accessible_and_denied_claims_no_causality() {
        let mut v = nodes("io", 0..8, NodeAccess::Accessible);
        v.extend(nodes("dv", 0..1, NodeAccess::DeviceCgroupDenied));
        let msg = diagnose(0, &ok(v)).unwrap();
        assert_eq!(listed_after(&msg, "(EPERM): "), ["dv0"]);
        assert!(!msg.contains("This is why"), "{msg}");
        assert!(msg.contains("dmesg"), "{msg}");
        assert!(msg.contains("firmware"), "{msg}");
        assert!(msg.contains("DeviceAllow"), "{msg}");
    }

    #[test]
    fn zero_devices_no_nodes_points_at_card_and_driver() {
        let msg = diagnose(0, &ok(vec![])).unwrap();
        assert!(msg.contains("no device nodes"), "{msg}");
        assert!(msg.contains("lsmod"), "{msg}");
        assert!(msg.contains("container"), "{msg}");
        assert!(!msg.contains("DeviceAllow"), "{msg}");
    }

    #[test]
    fn zero_devices_unreadable_dir_still_reports_without_blaming_the_device_cgroup() {
        let scan = Err(io::Error::from_raw_os_error(libc::EACCES));
        let msg = diagnose(0, &scan).unwrap();
        assert!(msg.contains("/dev/blackmagic/"), "{msg}");
        assert!(
            msg.contains(&io::Error::from_raw_os_error(libc::EACCES).to_string()),
            "{msg}"
        );
        assert!(msg.contains("permissions on /dev/blackmagic/"), "{msg}");
        assert!(!msg.contains("sandbox"), "{msg}");
        assert!(!msg.contains("cgroup"), "{msg}");
        assert!(!msg.contains("DeviceAllow"), "{msg}");
    }

    #[test]
    fn zero_devices_always_yields_a_diagnosis() {
        let mut mixed = nodes("io", 0..2, NodeAccess::Accessible);
        mixed.extend(nodes("dv", 0..2, NodeAccess::DeviceCgroupDenied));
        mixed.extend(nodes("xx", 0..1, NodeAccess::PermissionDenied));
        mixed.extend(nodes("yy", 0..1, NodeAccess::Other(libc::EIO)));
        let shapes: Vec<io::Result<Vec<DevNode>>> = vec![
            Err(io::Error::from_raw_os_error(libc::EACCES)),
            ok(vec![]),
            ok(nodes("io", 0..8, NodeAccess::Accessible)),
            ok(nodes("dv", 0..4, NodeAccess::DeviceCgroupDenied)),
            ok(nodes("dv", 0..4, NodeAccess::PermissionDenied)),
            ok(nodes("dv", 0..4, NodeAccess::Other(libc::EIO))),
            ok(mixed),
        ];
        for scan in &shapes {
            let msg = diagnose(0, scan);
            assert!(msg.is_some(), "zero devices went silent for {scan:?}");
            assert!(!msg.unwrap().is_empty());
        }
    }

    #[test]
    fn partial_denial_with_devices_enumerated_names_only_denied() {
        let mut v = nodes("io", 0..8, NodeAccess::Accessible);
        v.extend(nodes("io", 8..16, NodeAccess::DeviceCgroupDenied));
        let msg = diagnose(8, &ok(v)).unwrap();
        assert_eq!(
            listed_after(&msg, "(EPERM): "),
            ["io8", "io9", "io10", "io11", "io12", "io13", "io14", "io15"]
        );
        assert!(msg.contains("8 of 16 device node(s)"), "{msg}");
        assert!(msg.contains("8 device(s) enumerated"), "{msg}");
        assert!(!msg.contains("This is why"), "{msg}");
    }

    #[test]
    fn healthy_host_is_silent() {
        assert_eq!(
            diagnose(4, &ok(nodes("dv", 0..4, NodeAccess::Accessible))),
            None
        );
        assert_eq!(
            diagnose(4, &Err(io::Error::from_raw_os_error(libc::EACCES))),
            None
        );
    }

    #[test]
    fn unchecked_nodes_alone_do_not_warn_when_devices_enumerate() {
        let mut v = nodes("dv", 0..4, NodeAccess::Accessible);
        v.extend(nodes("dv", 4..5, NodeAccess::Other(libc::EIO)));
        assert_eq!(diagnose(4, &ok(v)), None);
    }

    #[test]
    fn unchecked_nodes_are_rendered_with_their_errno() {
        let msg = diagnose(0, &ok(nodes("dv", 0..1, NodeAccess::Other(libc::EIO)))).unwrap();
        assert!(
            msg.contains(&format!(
                "could not be checked: dv0 (os error {})",
                libc::EIO
            )),
            "{msg}"
        );
        assert!(msg.contains("enumerated 0"), "{msg}");
        assert!(msg.contains("dmesg"), "{msg}");
        assert!(!msg.contains("This is why"), "{msg}");
    }

    #[test]
    fn eacces_reported_as_file_permissions_not_device_filter() {
        let msg = diagnose(0, &ok(nodes("dv", 0..2, NodeAccess::PermissionDenied))).unwrap();
        assert!(msg.contains("EACCES"), "{msg}");
        assert!(msg.contains("AppArmor/SELinux"), "{msg}");
        assert!(msg.contains("ls -l /dev/blackmagic/"), "{msg}");
        assert!(!msg.contains("DeviceAllow="), "{msg}");
        assert!(!msg.contains("EPERM"), "{msg}");
    }

    #[test]
    fn eperm_and_eacces_together_carry_both_remedies() {
        let mut v = nodes("dv", 0..2, NodeAccess::DeviceCgroupDenied);
        v.extend(nodes("dv", 2..4, NodeAccess::PermissionDenied));
        let msg = diagnose(0, &ok(v)).unwrap();
        assert_eq!(listed_after(&msg, "(EPERM): "), ["dv0", "dv1"]);
        assert_eq!(listed_after(&msg, "(EACCES): "), ["dv2", "dv3"]);
        assert!(msg.contains("DeviceAllow="), "{msg}");
        assert!(msg.contains("ls -l /dev/blackmagic/"), "{msg}");
        assert!(msg.contains("This is why"), "{msg}");
    }

    #[test]
    fn listed_names_are_capped() {
        let msg = diagnose(0, &ok(nodes("io", 0..40, NodeAccess::DeviceCgroupDenied))).unwrap();
        assert!(msg.contains("and 24 more"), "{msg}");
        assert!(msg.contains("io15 and 24 more"), "{msg}");
        assert!(!msg.contains("io16"), "{msg}");
        assert!(msg.len() < 1024, "{} bytes: {msg}", msg.len());
    }

    #[test]
    fn classify_access_errno_maps_eperm_eacces_other() {
        assert_eq!(
            classify_access_errno(libc::EPERM),
            NodeAccess::DeviceCgroupDenied
        );
        assert_eq!(
            classify_access_errno(libc::EACCES),
            NodeAccess::PermissionDenied
        );
        assert_eq!(
            classify_access_errno(libc::ENOENT),
            NodeAccess::Other(libc::ENOENT)
        );
    }

    #[test]
    fn natural_order_sorts_by_prefix_then_number() {
        let mut v = vec!["io10", "io2", "dv0", "io1", "dv10", "dv9"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["dv0", "dv9", "dv10", "io1", "io2", "io10"]);
    }

    #[test]
    fn scan_missing_dir_is_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan(&tmp.path().join("absent")).unwrap().is_empty());
    }

    #[test]
    fn scan_keeps_only_char_devices_naturally_sorted() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        symlink("/dev/null", dir.join("io10")).unwrap();
        symlink("/dev/null", dir.join("io2")).unwrap();
        symlink("/dev/null", dir.join("dv0")).unwrap();
        fs::write(dir.join("README"), b"not a device").unwrap();
        symlink("/nonexistent/bilbycast-edge-168", dir.join("dv9")).unwrap();
        let found = scan(dir).unwrap();
        let names: Vec<&str> = found.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["dv0", "io2", "io10"]);
        assert!(
            found.iter().all(|n| n.access == NodeAccess::Accessible),
            "{found:?}"
        );
    }

    /// The packaged unit must grant every node name the remedy text promises,
    /// each as its own rule with no trailing comment (systemd folds a `#` into
    /// the value and silently drops the rule), inside `[Service]`, and with no
    /// empty `DeviceAllow=` that would reset every grant above it.
    #[test]
    fn shipped_unit_grants_every_known_decklink_node() {
        let unit = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/bilbycast-edge.service"
        ));
        // (section header, trimmed line) for every non-header line.
        let mut section = "";
        let mut lines: Vec<(&str, &str)> = Vec::new();
        for line in unit.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                section = line;
            } else {
                lines.push((section, line));
            }
        }
        let mut expected: Vec<String> = UNIT_GRANTED_PREFIXES
            .iter()
            .flat_map(|p| (0..UNIT_GRANTED_PER_PREFIX).map(move |n| format!("{p}{n}")))
            .map(|node| format!("DeviceAllow={BLACKMAGIC_DEV_DIR}/{node} rw"))
            .collect();
        expected.push("DeviceAllow=char-blackmagic rwm".to_owned());
        for want in &expected {
            let found = lines.iter().find(|(_, l)| l == want);
            let Some((sect, _)) = found else {
                panic!("unit lacks the exact line {want:?}");
            };
            assert_eq!(*sect, "[Service]", "{want:?} is outside [Service]");
        }
        for (_, line) in &lines {
            if let Some(value) = line.strip_prefix("DeviceAllow=") {
                assert!(
                    !value.contains('#'),
                    "trailing comment folds into the rule: {line:?}"
                );
                assert!(
                    !value.trim().is_empty(),
                    "an empty DeviceAllow= resets every grant above it"
                );
            }
        }
    }
}
