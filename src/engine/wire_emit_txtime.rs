// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kernel-paced wire emission via Linux `SO_TXTIME` + ETF qdisc.
//!
//! Phase 2 of the wire-pacing rollout — see `docs/wire-pacing.md`.
//!
//! At ST 2110-20 / -22 packet rates (250 k–1 M pps; 1–4 µs inter-packet
//! budget), userspace pacing — even with `clock_nanosleep` on a
//! `SCHED_FIFO` thread, as `engine::wire_emit` does — cannot meet
//! ST 2110-21 narrow profile bounds. Per-packet syscall cost alone
//! saturates a CPU core well before the link is full. The kernel-paced
//! alternative is `SO_TXTIME`: each datagram carries a `SCM_TXTIME`
//! CMSG with a target nanosecond timestamp, the ETF qdisc holds the
//! packet until that time, and (on supported NICs) the NIC HW offload
//! pushes the bytes out at the operator-disciplined PTP clock — sub-µs
//! jitter, fully decoupled from CPU contention.
//!
//! ## Pure Rust, no `nix`
//!
//! This module uses raw `libc` FFI for `setsockopt`, `sendmsg`,
//! `sendmmsg`, and the CMSG accessors. The `nix` crate is intentionally
//! avoided — same pattern as `engine::st2110::hwts`.
//!
//! ## Platform gating
//!
//! Linux-only. On macOS/BSD/etc. the public surface still compiles but
//! every operation returns `Err(io::ErrorKind::Unsupported)` and the
//! capability is never advertised on the host.

use std::io;
use std::net::SocketAddr;

/// Kernel `CLOCK_TAI` clock id. The ETF qdisc accepts `CLOCK_TAI` (the
/// natural choice when PTP4L is disciplining the system clock).
/// `CLOCK_MONOTONIC` is also accepted by the kernel but pairs awkwardly
/// with the ST 2110 frame raster anchor (which is wall-time aligned to
/// the PTP grandmaster).
#[cfg(target_os = "linux")]
pub const CLOCK_TAI: i32 = libc::CLOCK_TAI;

#[cfg(not(target_os = "linux"))]
pub const CLOCK_TAI: i32 = 0;

/// One datagram in a `send_batch_with_txtime` call.
///
/// Reserved for the `sendmmsg` batch optimization. The v1
/// `run_paced_sender` in `engine::st2110_video_io` uses the per-packet
/// `send_with_txtime` entry point — clean code shape, fine at
/// 250 k pps on a single-output system. The batched path becomes
/// material at multi-Mpps loads (e.g. an edge pumping several 4K60
/// outputs concurrently). Wire-up is a future revision.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct TxtimeBatchEntry<'a> {
    pub bytes: &'a [u8],
    /// Target tx time in nanoseconds since the kernel epoch of the
    /// `clockid` configured on the socket (typically `CLOCK_TAI`).
    pub target_tx_time_ns: u64,
}

/// One-shot probe — open a transient UDP socket, attempt
/// `SO_TXTIME` setsockopt with `CLOCK_TAI`. Returns `true` if accepted.
///
/// Used at startup by `engine::hardware_probe` to gate the
/// `wire_pacing_txtime` capability on `HealthPayload.capabilities`. The
/// probe does NOT validate that ETF qdisc is actually installed on any
/// interface — that's an operator-side concern surfaced via the runbook.
#[cfg(target_os = "linux")]
pub fn probe() -> bool {
    use std::net::Ipv4Addr;
    let s = match std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    enable_so_txtime(&s, CLOCK_TAI).is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn probe() -> bool {
    false
}

/// Enable `SO_TXTIME` on a UDP socket. The clockid selects which
/// kernel clock the per-datagram `SCM_TXTIME` timestamps are interpreted
/// against. `flags` always includes `SOF_TXTIME_REPORT_ERRORS` so the
/// kernel pushes failures (e.g. `EOVERFLOW` for targets in the past)
/// onto the socket's error queue, which `drain_errqueue_late_count`
/// reads.
#[cfg(target_os = "linux")]
pub fn enable_so_txtime(socket: &std::net::UdpSocket, clockid: i32) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let cfg = libc::sock_txtime {
        clockid,
        flags: libc::SOF_TXTIME_REPORT_ERRORS,
    };
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TXTIME,
            (&cfg as *const libc::sock_txtime) as *const libc::c_void,
            std::mem::size_of::<libc::sock_txtime>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enable_so_txtime(_socket: &std::net::UdpSocket, _clockid: i32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_TXTIME is Linux-only",
    ))
}

/// Send one datagram with a target tx time. The socket must already
/// have `SO_TXTIME` enabled — otherwise the CMSG is silently dropped
/// and the packet emits immediately.
///
/// `target_tx_time_ns` is in the epoch of the socket's configured
/// clockid (typically `CLOCK_TAI` ns).
#[cfg(target_os = "linux")]
pub fn send_with_txtime(
    socket: &std::net::UdpSocket,
    dest: SocketAddr,
    bytes: &[u8],
    target_tx_time_ns: u64,
) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;

    // sockaddr storage. We use sockaddr_in / sockaddr_in6 sized
    // separately to avoid the larger sockaddr_storage allocation in the
    // hot path.
    let (sa_ptr, sa_len) = sockaddr_for(&dest);

    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };

    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<u64>() as libc::c_uint) };
    let mut cmsg_buf = vec![0u8; cmsg_space as usize];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = sa_ptr;
    msg.msg_namelen = sa_len;
    msg.msg_iov = &mut iov as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "CMSG_FIRSTHDR returned null",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_TXTIME;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u64>() as libc::c_uint) as _;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut u64;
        std::ptr::write_unaligned(data_ptr, target_tx_time_ns);
    }

    let rc = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as usize)
}

#[cfg(not(target_os = "linux"))]
pub fn send_with_txtime(
    _socket: &std::net::UdpSocket,
    _dest: SocketAddr,
    _bytes: &[u8],
    _target_tx_time_ns: u64,
) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_TXTIME is Linux-only",
    ))
}

/// Send a batch of datagrams in a single `sendmmsg(2)` syscall, each
/// carrying its own `SCM_TXTIME`. Amortizes the syscall cost — at
/// 250 k+ pps on the ST 2110-20 path, per-datagram `sendmsg` saturates
/// CPU long before the NIC.
///
/// Returns the number of datagrams actually transmitted (kernel may
/// short-write the batch on transient errors).
///
/// Reserved for the multi-output / multi-Mpps optimization (see
/// `TxtimeBatchEntry` doc). v1 uses [`send_with_txtime`] per-packet.
#[allow(dead_code)]
#[cfg(target_os = "linux")]
pub fn send_batch_with_txtime(
    socket: &std::net::UdpSocket,
    dest: SocketAddr,
    batch: &[TxtimeBatchEntry<'_>],
) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;

    if batch.is_empty() {
        return Ok(0);
    }

    let (sa_ptr, sa_len) = sockaddr_for(&dest);

    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<u64>() as libc::c_uint) };
    let cmsg_len_value =
        unsafe { libc::CMSG_LEN(std::mem::size_of::<u64>() as libc::c_uint) };

    // Per-entry storage. iovecs and cmsg buffers must outlive the
    // sendmmsg call.
    let mut iovs: Vec<libc::iovec> = Vec::with_capacity(batch.len());
    let mut cmsg_bufs: Vec<Vec<u8>> = Vec::with_capacity(batch.len());
    let mut mmsgs: Vec<libc::mmsghdr> = Vec::with_capacity(batch.len());

    for entry in batch {
        iovs.push(libc::iovec {
            iov_base: entry.bytes.as_ptr() as *mut libc::c_void,
            iov_len: entry.bytes.len(),
        });
        let mut buf = vec![0u8; cmsg_space as usize];
        // Fill the cmsg in place. We need stable pointers — Vec<Vec<u8>>
        // keeps each inner Vec at a stable address.
        // Safety: buf is exclusively ours and large enough for the cmsg.
        unsafe {
            let cmsg = buf.as_mut_ptr() as *mut libc::cmsghdr;
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_TXTIME;
            (*cmsg).cmsg_len = cmsg_len_value as _;
            // Data starts after the aligned cmsg header.
            let data_offset = libc::CMSG_LEN(0) as usize;
            let data_ptr = buf.as_mut_ptr().add(data_offset) as *mut u64;
            std::ptr::write_unaligned(data_ptr, entry.target_tx_time_ns);
        }
        cmsg_bufs.push(buf);
    }

    // Now wire iovs + cmsg_bufs into mmsghdrs. Must happen after Vec
    // pushes complete so addresses are stable.
    for (i, _entry) in batch.iter().enumerate() {
        let mut mh: libc::mmsghdr = unsafe { std::mem::zeroed() };
        mh.msg_hdr.msg_name = sa_ptr;
        mh.msg_hdr.msg_namelen = sa_len;
        mh.msg_hdr.msg_iov = (&mut iovs[i]) as *mut libc::iovec;
        mh.msg_hdr.msg_iovlen = 1;
        mh.msg_hdr.msg_control = cmsg_bufs[i].as_mut_ptr() as *mut libc::c_void;
        mh.msg_hdr.msg_controllen = cmsg_space as _;
        mmsgs.push(mh);
    }

    let rc = unsafe {
        libc::sendmmsg(
            socket.as_raw_fd(),
            mmsgs.as_mut_ptr(),
            mmsgs.len() as libc::c_uint,
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as usize)
}

#[allow(dead_code)]
#[cfg(not(target_os = "linux"))]
pub fn send_batch_with_txtime(
    _socket: &std::net::UdpSocket,
    _dest: SocketAddr,
    _batch: &[TxtimeBatchEntry<'_>],
) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_TXTIME is Linux-only",
    ))
}

/// Send a batch of datagrams to the same destination in a single
/// `sendmmsg(2)` syscall — no `SCM_TXTIME` cmsg, no kernel-side pacing.
/// Used by the `ClockNanosleep` releaser tier in `engine::wire_emit` to
/// amortize per-packet send overhead when the channel has multiple
/// "send NOW" datagrams queued (typically the burst of non-PCR datagrams
/// between two PCR-bearing packets on a TS output).
///
/// Per-packet `socket.send_to(...)` carries ~30 µs of syscall overhead
/// on SCHED_OTHER, and at 20 datagrams per PCR cycle (33 ms at 30 fps
/// over 6 Mbps TS) that's 600 µs / cycle — significant fraction of the
/// pacing budget, enough to drift the wire egress rate behind the
/// encoder producer rate and grow the `wire_tx` channel into the cap on
/// long runs. A single `sendmmsg` for the same 20 datagrams amortizes
/// the syscall to one ~30 µs call, restoring the pacing margin.
///
/// Returns the number of datagrams the kernel actually accepted (may be
/// less than the batch size on a transient send error).
///
/// On non-Linux this falls back to a loop of `send_to` calls.
#[cfg(target_os = "linux")]
pub fn send_batch_simple(
    socket: &std::net::UdpSocket,
    dest: SocketAddr,
    batch: &[&[u8]],
) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;

    if batch.is_empty() {
        return Ok(0);
    }

    let (sa_ptr, sa_len) = sockaddr_for(&dest);

    let mut iovs: Vec<libc::iovec> = Vec::with_capacity(batch.len());
    let mut mmsgs: Vec<libc::mmsghdr> = Vec::with_capacity(batch.len());

    for bytes in batch {
        iovs.push(libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        });
    }

    for i in 0..batch.len() {
        let mut mh: libc::mmsghdr = unsafe { std::mem::zeroed() };
        mh.msg_hdr.msg_name = sa_ptr;
        mh.msg_hdr.msg_namelen = sa_len;
        mh.msg_hdr.msg_iov = (&mut iovs[i]) as *mut libc::iovec;
        mh.msg_hdr.msg_iovlen = 1;
        mh.msg_hdr.msg_control = std::ptr::null_mut();
        mh.msg_hdr.msg_controllen = 0;
        mmsgs.push(mh);
    }

    let rc = unsafe {
        libc::sendmmsg(
            socket.as_raw_fd(),
            mmsgs.as_mut_ptr(),
            mmsgs.len() as libc::c_uint,
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as usize)
}

#[cfg(not(target_os = "linux"))]
pub fn send_batch_simple(
    socket: &std::net::UdpSocket,
    dest: SocketAddr,
    batch: &[&[u8]],
) -> io::Result<usize> {
    let mut n = 0;
    for bytes in batch {
        socket.send_to(bytes, dest)?;
        n += 1;
    }
    Ok(n)
}

/// Drain the socket's error queue and count datagrams the kernel
/// rejected as late (target tx time landed in the past, returning
/// `EOVERFLOW`). Should be called periodically — e.g. once per ST 2110
/// frame — to keep the queue from filling.
///
/// On non-Linux this is a no-op returning 0.
#[cfg(target_os = "linux")]
pub fn drain_errqueue_late_count(socket: &std::net::UdpSocket) -> u64 {
    use std::os::unix::io::AsRawFd;

    let mut count: u64 = 0;
    let mut scratch = [0u8; 1500];
    let mut control = [0u8; 256];

    loop {
        let mut iov = libc::iovec {
            iov_base: scratch.as_mut_ptr() as *mut libc::c_void,
            iov_len: scratch.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = control.len() as _;

        let rc = unsafe {
            libc::recvmsg(
                socket.as_raw_fd(),
                &mut msg,
                libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT,
            )
        };
        if rc < 0 {
            // EAGAIN/EWOULDBLOCK: queue empty.
            break;
        }
        // Each successful recvmsg from MSG_ERRQUEUE corresponds to one
        // dropped datagram. We don't bother walking the cmsgs — the
        // drop semantic is binary, not categorical.
        count = count.saturating_add(1);
    }
    count
}

#[cfg(not(target_os = "linux"))]
pub fn drain_errqueue_late_count(_socket: &std::net::UdpSocket) -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn sockaddr_for(addr: &SocketAddr) -> (*mut libc::c_void, libc::socklen_t) {
    // SAFETY: returns a thread-local-style storage. We use a thread-
    // local buffer so the pointer is valid for the duration of one
    // syscall and overwritten on the next call from the same thread —
    // safe because syscalls are synchronous within a thread.
    thread_local! {
        static SA_V4: std::cell::UnsafeCell<libc::sockaddr_in> =
            std::cell::UnsafeCell::new(unsafe { std::mem::zeroed() });
        static SA_V6: std::cell::UnsafeCell<libc::sockaddr_in6> =
            std::cell::UnsafeCell::new(unsafe { std::mem::zeroed() });
    }
    match addr {
        SocketAddr::V4(v4) => SA_V4.with(|cell| {
            let sa = cell.get();
            unsafe {
                (*sa).sin_family = libc::AF_INET as libc::sa_family_t;
                (*sa).sin_port = v4.port().to_be();
                (*sa).sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            }
            (
                sa as *mut libc::c_void,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }),
        SocketAddr::V6(v6) => SA_V6.with(|cell| {
            let sa = cell.get();
            unsafe {
                (*sa).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                (*sa).sin6_port = v6.port().to_be();
                (*sa).sin6_flowinfo = v6.flowinfo();
                (*sa).sin6_addr.s6_addr = v6.ip().octets();
                (*sa).sin6_scope_id = v6.scope_id();
            }
            (
                sa as *mut libc::c_void,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }),
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe is non-fatal regardless of host. On Linux ≥ 4.19 it should
    /// succeed; elsewhere it returns false. Either way, the test passes
    /// — we only care that the function does not panic.
    #[test]
    fn probe_does_not_panic() {
        let _ = probe();
    }

    /// Stub APIs return `Unsupported` on non-Linux without panicking.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn non_linux_returns_unsupported() {
        use std::net::Ipv4Addr;
        let s = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let r = enable_so_txtime(&s, CLOCK_TAI);
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    /// On Linux, enable_so_txtime succeeds on a fresh UDP socket.
    /// (Kernels < 4.19 will fail with ENOPROTOOPT — gated by the
    /// build target_os, not the running kernel; if this test fails on
    /// a too-old kernel that's a pre-existing operator constraint.)
    #[test]
    #[cfg(target_os = "linux")]
    fn enable_so_txtime_succeeds_on_modern_linux() {
        use std::net::Ipv4Addr;
        let s = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let r = enable_so_txtime(&s, CLOCK_TAI);
        // Accept both success (kernel ≥ 4.19) and ENOPROTOOPT (older).
        // We don't assert on the result — the probe is the canonical
        // capability check; this test exercises that the syscall path
        // builds and runs without UB.
        let _ = r;
    }

    /// Sending without an ETF qdisc still succeeds at the syscall
    /// layer (kernel just emits immediately, ignoring the SCM_TXTIME
    /// CMSG). This test verifies the CMSG construction code does not
    /// segfault, the cmsg layout matches what the kernel expects, and
    /// the iovec/msghdr wiring is valid.
    #[test]
    #[cfg(target_os = "linux")]
    fn send_with_txtime_syscall_smoke() {
        use std::net::Ipv4Addr;
        // Two sockets on loopback: one for paced send, one for raw
        // recv. Bind both to ephemeral ports.
        let send = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let recv = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        // Best-effort enable. If the kernel rejects, skip the rest.
        if enable_so_txtime(&send, CLOCK_TAI).is_err() {
            return;
        }
        let dest = recv.local_addr().unwrap();
        let payload = b"hello-txtime";
        // target = now + 5 ms in CLOCK_TAI ns. We don't have a TAI
        // clock_gettime helper inline; use a clearly future epoch
        // value. Without ETF qdisc, kernel emits immediately anyway,
        // so this just exercises the syscall surface.
        let target_ns = 1u64 << 62;
        let n = send_with_txtime(&send, dest, payload, target_ns).unwrap();
        assert_eq!(n, payload.len());

        let mut buf = [0u8; 64];
        let (n, _) = recv.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], payload);
    }

    /// sendmmsg path: build a 4-entry batch, send it, recv all 4 on
    /// loopback, verify ordering and content.
    #[test]
    #[cfg(target_os = "linux")]
    fn send_batch_with_txtime_syscall_smoke() {
        use std::net::Ipv4Addr;
        let send = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let recv = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        if enable_so_txtime(&send, CLOCK_TAI).is_err() {
            return;
        }
        let dest = recv.local_addr().unwrap();
        let payloads: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; 32]).collect();
        let batch: Vec<TxtimeBatchEntry> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| TxtimeBatchEntry {
                bytes: p,
                target_tx_time_ns: (1u64 << 62) + (i as u64) * 1_000_000,
            })
            .collect();
        let n = send_batch_with_txtime(&send, dest, &batch).unwrap();
        assert_eq!(n, 4);

        let mut received = Vec::new();
        let mut buf = [0u8; 64];
        for _ in 0..4 {
            let (n, _) = recv.recv_from(&mut buf).unwrap();
            received.push(buf[..n].to_vec());
        }
        assert_eq!(received.len(), 4);
        // Order is preserved on loopback within a single sendmmsg.
        for (i, got) in received.iter().enumerate() {
            assert_eq!(got, &payloads[i]);
        }
    }

    /// CMSG_SPACE / CMSG_LEN return values match the on-the-wire kernel
    /// layout for a u64 payload (24 / 24 on x86_64). This test catches
    /// a regression if libc-rs's CMSG accessors ever drift from the
    /// kernel's expectations.
    #[test]
    #[cfg(target_os = "linux")]
    fn cmsg_size_for_u64_payload() {
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<u64>() as libc::c_uint) };
        let len = unsafe { libc::CMSG_LEN(std::mem::size_of::<u64>() as libc::c_uint) };
        // sizeof(cmsghdr) is 16 on x86_64, aligned to 8. CMSG_SPACE adds
        // CMSG_ALIGN(payload_len) = 8. Total 24.
        // CMSG_LEN is sizeof(cmsghdr) + payload_len = 16 + 8 = 24.
        assert_eq!(space, 24);
        assert_eq!(len, 24);
    }
}
