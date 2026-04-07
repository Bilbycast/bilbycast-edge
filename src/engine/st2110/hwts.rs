// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

// Phase 1 step 2: foundation. Wired into the actual ST 2110 input tasks in
// step 4; the public surface is exercised by unit tests in this module.
#![allow(dead_code)]

//! Hardware timestamp helpers for SMPTE ST 2110 input sockets.
//!
//! ST 2110 reception requires accurate ingress timestamps to align audio
//! samples and ancillary data to the PTP grandmaster. On Linux, this is done
//! by enabling `SO_TIMESTAMPING` on the receive socket and reading the
//! `SCM_TIMESTAMPING` cmsghdr returned by `recvmsg(2)`. The kernel populates
//! up to three timestamps in the message: software receive, hardware receive,
//! and (on send) hardware transmit.
//!
//! Bilbycast does not link any C library for this — only `libc` FFI bindings
//! to the kernel syscalls. The whole module is gated on `target_os = "linux"`;
//! on macOS, BSD, etc., the API still compiles but `enable_so_timestamping`
//! returns `Err(HwTimestampError::Unsupported)` and the receiver falls back
//! to software timestamps from `tokio::net::UdpSocket::recv_from`.
//!
//! ## Pure Rust
//!
//! This module exists explicitly to avoid pulling `nix`. We use the `libc`
//! crate (already a dependency) for the few constants and structs we need.
//! The cmsg parser is hand-rolled and unit-tested.

use std::io;

use socket2::Socket;

/// Errors that can occur while configuring or reading hardware timestamps.
#[derive(Debug)]
pub enum HwTimestampError {
    /// Hardware/kernel timestamping is not available on this platform.
    Unsupported,
    /// The kernel returned an error from `setsockopt`/`recvmsg`.
    Io(io::Error),
}

impl std::fmt::Display for HwTimestampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HwTimestampError::Unsupported => {
                f.write_str("hardware timestamping is not supported on this platform")
            }
            HwTimestampError::Io(e) => write!(f, "hardware timestamping I/O error: {e}"),
        }
    }
}

impl std::error::Error for HwTimestampError {}

impl From<io::Error> for HwTimestampError {
    fn from(e: io::Error) -> Self {
        HwTimestampError::Io(e)
    }
}

/// A hardware timestamp recovered from a `SCM_TIMESTAMPING` cmsghdr.
///
/// The kernel reports up to three `struct timespec` entries:
///
/// 1. Software receive timestamp (always populated when SOF_TIMESTAMPING_SOFTWARE
///    is requested).
/// 2. (Legacy hardware timestamp, deprecated since Linux 3.17.)
/// 3. Hardware receive/transmit timestamp from the NIC PHC.
///
/// We surface only the most accurate one available: hardware first, software
/// fallback. Both are converted to a single signed nanosecond count for
/// uniform downstream handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwTimestamp {
    /// Source of this timestamp.
    pub source: HwTimestampSource,
    /// Time in nanoseconds since the UNIX epoch (PTP-aligned when source is
    /// `Hardware` and the NIC PHC is synced to PTP).
    pub unix_ns: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwTimestampSource {
    /// Software timestamp captured by the kernel network stack.
    Software,
    /// Hardware timestamp from the NIC PHC. Most accurate.
    Hardware,
}

/// Enable kernel timestamping on the given socket.
///
/// On Linux, sets `SO_TIMESTAMPING` with the broadcast-relevant flags:
///
/// - `SOF_TIMESTAMPING_RX_SOFTWARE | SOF_TIMESTAMPING_SOFTWARE` always (so
///   software timestamps are populated even on NICs without hardware support).
/// - `SOF_TIMESTAMPING_RX_HARDWARE | SOF_TIMESTAMPING_RAW_HARDWARE` when the
///   caller requests hardware timestamps.
///
/// Returns `Err(Unsupported)` on non-Linux targets so the caller can degrade
/// gracefully without a runtime panic.
pub fn enable_so_timestamping(socket: &Socket, want_hw: bool) -> Result<(), HwTimestampError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut flags: u32 = SOF_TIMESTAMPING_RX_SOFTWARE | SOF_TIMESTAMPING_SOFTWARE;
        if want_hw {
            flags |= SOF_TIMESTAMPING_RX_HARDWARE | SOF_TIMESTAMPING_RAW_HARDWARE;
        }
        let value = flags as libc::c_int;
        // SAFETY: standard setsockopt FFI.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_TIMESTAMPING,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(HwTimestampError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, want_hw);
        Err(HwTimestampError::Unsupported)
    }
}

/// Parse a `SCM_TIMESTAMPING` ancillary buffer (i.e., the bytes between
/// `cmsghdr` headers from a `recvmsg` call) and extract the most accurate
/// timestamp present.
///
/// The buffer must be exactly the cmsg data field — not the full cmsghdr
/// envelope. Bytes beyond the third `timespec` are ignored. Returns `None`
/// when both `tv_sec` and `tv_nsec` are zero in every slot, which indicates
/// the kernel did not populate any timestamp (e.g., the NIC has no PHC).
pub fn parse_scm_timestamping(cmsg_data: &[u8]) -> Option<HwTimestamp> {
    // SCM_TIMESTAMPING returns three struct timespec entries. Each timespec
    // is 16 bytes on 64-bit systems (i64 + i64) and 8 bytes on some 32-bit
    // builds. The Linux ABI uses the size matching `time_t`. We match against
    // both possibilities by inferring from the slice length.
    let entries = parse_three_timespecs(cmsg_data)?;
    // Prefer the hardware timestamp (slot 2) when present.
    if let Some(t) = entries[2] {
        return Some(HwTimestamp {
            source: HwTimestampSource::Hardware,
            unix_ns: t,
        });
    }
    if let Some(t) = entries[0] {
        return Some(HwTimestamp {
            source: HwTimestampSource::Software,
            unix_ns: t,
        });
    }
    None
}

/// Parse up to three `struct timespec` entries from a cmsg buffer.
///
/// Returns `[ts0, ts1, ts2]` where each entry is `Some(unix_ns)` when the
/// timespec is non-zero, otherwise `None`. The caller decides which slot to
/// use.
fn parse_three_timespecs(buf: &[u8]) -> Option<[Option<i128>; 3]> {
    // Try 64-bit timespec first (Linux x86_64, aarch64, riscv64): 16 bytes.
    let ts_size_64 = 16usize;
    let ts_size_32 = 8usize;

    let parse_at = |bytes: &[u8], ts_size: usize| -> [Option<i128>; 3] {
        let mut out = [None, None, None];
        for (i, slot) in out.iter_mut().enumerate() {
            let off = i * ts_size;
            if off + ts_size > bytes.len() {
                break;
            }
            let chunk = &bytes[off..off + ts_size];
            let (sec, nsec): (i128, i128) = if ts_size == 16 {
                let sec = i64::from_ne_bytes(chunk[0..8].try_into().unwrap()) as i128;
                let nsec = i64::from_ne_bytes(chunk[8..16].try_into().unwrap()) as i128;
                (sec, nsec)
            } else {
                let sec = i32::from_ne_bytes(chunk[0..4].try_into().unwrap()) as i128;
                let nsec = i32::from_ne_bytes(chunk[4..8].try_into().unwrap()) as i128;
                (sec, nsec)
            };
            if sec == 0 && nsec == 0 {
                *slot = None;
            } else {
                *slot = Some(sec * 1_000_000_000 + nsec);
            }
        }
        out
    };

    if buf.len() >= ts_size_64 * 3 {
        Some(parse_at(buf, ts_size_64))
    } else if buf.len() >= ts_size_32 * 3 {
        Some(parse_at(buf, ts_size_32))
    } else {
        None
    }
}

// ─────────────── Linux constants (no `nix` dep) ───────────────
//
// We hard-code the SO_TIMESTAMPING constants ourselves to avoid pulling
// `nix`. These values are defined in `<linux/net_tstamp.h>` and are stable
// across kernel versions; they form a public ABI that ptp4l, chrony, and
// many production tools depend on.

#[cfg(target_os = "linux")]
const SO_TIMESTAMPING: libc::c_int = 37;

#[cfg(target_os = "linux")]
const SOF_TIMESTAMPING_RX_HARDWARE: u32 = 1 << 2;
#[cfg(target_os = "linux")]
const SOF_TIMESTAMPING_RX_SOFTWARE: u32 = 1 << 3;
#[cfg(target_os = "linux")]
const SOF_TIMESTAMPING_SOFTWARE: u32 = 1 << 4;
#[cfg(target_os = "linux")]
const SOF_TIMESTAMPING_RAW_HARDWARE: u32 = 1 << 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_scm_empty_returns_none() {
        assert!(parse_scm_timestamping(&[]).is_none());
    }

    #[test]
    fn test_parse_scm_all_zero_returns_none() {
        let buf = vec![0u8; 16 * 3];
        assert!(parse_scm_timestamping(&buf).is_none());
    }

    #[test]
    fn test_parse_scm_software_only() {
        // First slot: tv_sec=10, tv_nsec=500. Second/third: zero.
        let mut buf = vec![0u8; 16 * 3];
        buf[0..8].copy_from_slice(&10i64.to_ne_bytes());
        buf[8..16].copy_from_slice(&500i64.to_ne_bytes());
        let ts = parse_scm_timestamping(&buf).expect("software timestamp");
        assert_eq!(ts.source, HwTimestampSource::Software);
        assert_eq!(ts.unix_ns, 10_000_000_500);
    }

    #[test]
    fn test_parse_scm_prefers_hardware_when_present() {
        // First slot: software (sec=10), third slot: hardware (sec=20).
        let mut buf = vec![0u8; 16 * 3];
        buf[0..8].copy_from_slice(&10i64.to_ne_bytes());
        buf[32..40].copy_from_slice(&20i64.to_ne_bytes());
        buf[40..48].copy_from_slice(&123i64.to_ne_bytes());
        let ts = parse_scm_timestamping(&buf).expect("hardware timestamp");
        assert_eq!(ts.source, HwTimestampSource::Hardware);
        assert_eq!(ts.unix_ns, 20_000_000_123);
    }

    #[test]
    fn test_parse_scm_negative_seconds() {
        // sec=-1, nsec=999_999_999 → unix_ns = -1
        let mut buf = vec![0u8; 16 * 3];
        buf[0..8].copy_from_slice(&(-1i64).to_ne_bytes());
        buf[8..16].copy_from_slice(&999_999_999i64.to_ne_bytes());
        let ts = parse_scm_timestamping(&buf).expect("software timestamp");
        assert_eq!(ts.unix_ns, -1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_enable_so_timestamping_runs_on_linux() {
        use socket2::{Domain, Protocol, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        // Should succeed even on a NIC without hardware support — the kernel
        // happily accepts the flags and silently downgrades to software.
        let res = enable_so_timestamping(&s, false);
        assert!(res.is_ok(), "linux setsockopt should succeed: {res:?}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_enable_so_timestamping_returns_unsupported_off_linux() {
        use socket2::{Domain, Protocol, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        let res = enable_so_timestamping(&s, true);
        assert!(matches!(res, Err(HwTimestampError::Unsupported)));
    }
}
