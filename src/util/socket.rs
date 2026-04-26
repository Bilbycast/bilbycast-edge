// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

/// Default receive socket buffer size (4 MB).
///
/// OS defaults are often 212 KB on Linux, far too small for broadcast video
/// at multi-Gbps rates. 4 MB absorbs ~25 ms of burst at 1.5 Gbps without
/// kernel drops. Operators needing higher throughput should tune via sysctl
/// (`net.core.rmem_max`).
const DEFAULT_RECV_BUF_SIZE: usize = 4 * 1024 * 1024;

/// Default send socket buffer size (4 MB).
const DEFAULT_SEND_BUF_SIZE: usize = 4 * 1024 * 1024;

/// Apply receive and send socket buffer sizes via SO_RCVBUF / SO_SNDBUF.
///
/// Best-effort: logs a warning if the kernel clamps to a lower value (e.g.,
/// `net.core.rmem_max` / `net.core.wmem_max` is too low) but does not fail.
fn set_socket_buffers(socket: &Socket, recv_size: usize, send_size: usize) {
    if let Err(e) = socket.set_recv_buffer_size(recv_size) {
        tracing::warn!(
            "Failed to set SO_RCVBUF to {} bytes: {e}. \
             Increase net.core.rmem_max for optimal performance.",
            recv_size
        );
    } else {
        let actual = socket.recv_buffer_size().unwrap_or(0);
        if actual < recv_size {
            tracing::debug!(
                "SO_RCVBUF requested {} bytes, kernel set {} bytes",
                recv_size, actual
            );
        }
    }
    if let Err(e) = socket.set_send_buffer_size(send_size) {
        tracing::warn!(
            "Failed to set SO_SNDBUF to {} bytes: {e}. \
             Increase net.core.wmem_max for optimal performance.",
            send_size
        );
    }
}

/// Create a UDP socket bound to the given address with multicast support.
/// If the bind address is a multicast group, automatically joins the group —
/// any-source (ASM) when `source_addr` is `None`, or source-specific (SSM)
/// when `Some`. `interface_addr` selects the local NIC for the join.
pub async fn bind_udp_input(
    bind_addr: &str,
    interface_addr: Option<&str>,
    source_addr: Option<&str>,
) -> Result<UdpSocket> {
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("Invalid bind address: {bind_addr}"))?;

    if addr.ip().is_multicast() {
        bind_multicast_input(addr, interface_addr, source_addr).await
    } else {
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
            .context("Failed to create UDP socket")?;
        socket.set_nonblocking(true)?;
        set_socket_buffers(&socket, DEFAULT_RECV_BUF_SIZE, DEFAULT_SEND_BUF_SIZE);
        socket
            .bind(&SockAddr::from(addr))
            .map_err(|e| crate::util::port_error::annotate_bind_error(e, addr, "UDP/RTP input"))?;
        let std_socket: std::net::UdpSocket = socket.into();
        let sock = UdpSocket::from_std(std_socket)
            .context("Failed to convert socket2 socket to tokio UdpSocket")?;
        tracing::info!("UDP input socket bound to {addr} (unicast)");
        Ok(sock)
    }
}

/// Bind a multicast receive socket.
/// For multicast input we bind to 0.0.0.0:<port> (or [::]:<port>) with SO_REUSEADDR,
/// then join the multicast group (ASM by default, SSM when `source_addr` is set).
async fn bind_multicast_input(
    mcast_addr: SocketAddr,
    interface_addr: Option<&str>,
    source_addr: Option<&str>,
) -> Result<UdpSocket> {
    let domain = match mcast_addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .context("Failed to create UDP socket")?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "macos")]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    set_socket_buffers(&socket, DEFAULT_RECV_BUF_SIZE, DEFAULT_SEND_BUF_SIZE);

    let bind_to: SocketAddr = match mcast_addr {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), v4.port()),
        SocketAddr::V6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), v6.port()),
    };
    socket
        .bind(&SockAddr::from(bind_to))
        .with_context(|| format!("Failed to bind multicast socket to {bind_to}"))?;

    match (mcast_addr.ip(), source_addr) {
        (IpAddr::V4(group), None) => {
            let iface = parse_interface_v4(interface_addr)?;
            socket
                .join_multicast_v4(&group, &iface)
                .with_context(|| {
                    format!("Failed to join multicast group {group} on interface {iface}")
                })?;
            tracing::info!(
                "UDP input: joined multicast group {group} on interface {iface}, port {}",
                mcast_addr.port()
            );
        }
        (IpAddr::V4(group), Some(src_str)) => {
            let iface = parse_interface_v4(interface_addr)?;
            let source = parse_source_v4(src_str)?;
            socket
                .join_ssm_v4(&group, &source, &iface)
                .with_context(|| {
                    format!("Failed to join SSM ({source}, {group}) on interface {iface}")
                })?;
            tracing::info!(
                "UDP input: joined SSM ({source}, {group}) on interface {iface}, port {}",
                mcast_addr.port()
            );
        }
        (IpAddr::V6(group), None) => {
            let iface_index = parse_interface_v6_index(interface_addr)?;
            socket
                .join_multicast_v6(&group, iface_index)
                .with_context(|| {
                    format!("Failed to join multicast group {group} on interface index {iface_index}")
                })?;
            tracing::info!(
                "UDP input: joined multicast group {group} on interface index {iface_index}, port {}",
                mcast_addr.port()
            );
        }
        (IpAddr::V6(group), Some(src_str)) => {
            let iface_index = parse_interface_v6_index(interface_addr)?;
            let source = parse_source_v6(src_str)?;
            join_ssm_v6(&socket, &group, &source, iface_index).with_context(|| {
                format!("Failed to join SSM ({source}, {group}) on interface index {iface_index}")
            })?;
            tracing::info!(
                "UDP input: joined SSM ({source}, {group}) on interface index {iface_index}, port {}",
                mcast_addr.port()
            );
        }
    }

    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)
        .context("Failed to convert socket2 socket to tokio UdpSocket")?;
    Ok(tokio_socket)
}

/// Create a UDP socket for sending to a destination address.
/// If `bind_addr` is provided, bind to that address. Otherwise bind to ephemeral port.
pub async fn create_udp_output(
    dest_addr: &str,
    bind_addr: Option<&str>,
    interface_addr: Option<&str>,
    dscp: u8,
) -> Result<(UdpSocket, SocketAddr)> {
    let dest: SocketAddr = dest_addr
        .parse()
        .with_context(|| format!("Invalid destination address: {dest_addr}"))?;

    let bind_to: SocketAddr = match bind_addr {
        Some(addr) => addr
            .parse()
            .with_context(|| format!("Invalid bind address: {addr}"))?,
        None => {
            if dest.is_ipv4() {
                "0.0.0.0:0".parse().unwrap()
            } else {
                "[::]:0".parse().unwrap()
            }
        }
    };

    let domain = if dest.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .context("Failed to create output UDP socket")?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    set_socket_buffers(&socket, DEFAULT_RECV_BUF_SIZE, DEFAULT_SEND_BUF_SIZE);

    socket
        .bind(&SockAddr::from(bind_to))
        .with_context(|| format!("Failed to bind output socket to {bind_to}"))?;

    // Set DSCP/QoS marking (RP 2129 C10). The TOS/Traffic Class byte is DSCP << 2.
    // This is a per-socket kernel option — all packets inherit it automatically.
    if dscp > 0 {
        let tos = (dscp as u32) << 2;
        match domain {
            d if d == Domain::IPV4 => {
                socket
                    .set_tos_v4(tos)
                    .with_context(|| format!("Failed to set DSCP {dscp} (TOS {tos})"))?;
            }
            _ => {
                socket
                    .set_tclass_v6(tos)
                    .with_context(|| format!("Failed to set DSCP {dscp} (Traffic Class {tos})"))?;
            }
        }
        tracing::info!("UDP output: DSCP set to {dscp} (TOS/TCLASS byte {tos:#04x})");
    }

    // If destination is multicast, set the outgoing interface and TTL/hop limit
    if dest.ip().is_multicast() {
        match dest.ip() {
            IpAddr::V4(_) => {
                let iface = parse_interface_v4(interface_addr)?;
                socket
                    .set_multicast_if_v4(&iface)
                    .context("Failed to set multicast interface")?;
                socket.set_multicast_ttl_v4(16)?;
            }
            IpAddr::V6(_) => {
                let iface_index = parse_interface_v6_index(interface_addr)?;
                socket
                    .set_multicast_if_v6(iface_index)
                    .context("Failed to set IPv6 multicast interface")?;
                socket.set_multicast_hops_v6(16)?;
            }
        }
    }

    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)
        .context("Failed to convert output socket to tokio UdpSocket")?;

    let local_addr = tokio_socket.local_addr()?;
    tracing::info!("UDP output socket bound to {local_addr}, destination {dest}");

    Ok((tokio_socket, dest))
}

fn parse_interface_v4(interface_addr: Option<&str>) -> Result<Ipv4Addr> {
    match interface_addr {
        Some(addr) => addr
            .parse::<Ipv4Addr>()
            .with_context(|| format!("Invalid interface address: {addr}")),
        None => Ok(Ipv4Addr::UNSPECIFIED),
    }
}

/// Parse an interface specification for IPv6 multicast.
/// Accepts either a numeric interface index (e.g. "2") or an interface name (e.g. "eth0").
/// Returns 0 (any interface) if no interface is specified.
///
/// Note: IPv6 multicast APIs use interface indexes rather than IP addresses.
/// Use `ip -6 addr` or `ifconfig` to find the appropriate interface index or name.
fn parse_interface_v6_index(interface_addr: Option<&str>) -> Result<u32> {
    match interface_addr {
        None => Ok(0),
        Some(addr) => {
            // First try parsing as a numeric interface index
            if let Ok(index) = addr.parse::<u32>() {
                return Ok(index);
            }
            // Try resolving as an interface name (e.g. "eth0", "en0")
            let c_name = std::ffi::CString::new(addr)
                .with_context(|| format!("Invalid interface name: {addr}"))?;
            // SAFETY: if_nametoindex is a standard POSIX function that takes a null-terminated
            // C string and returns 0 on failure or the interface index on success.
            let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
            if index == 0 {
                anyhow::bail!(
                    "Invalid interface for IPv6 multicast: '{addr}'. \
                     Expected a numeric interface index (e.g. \"2\") or interface name (e.g. \"eth0\")."
                );
            }
            Ok(index)
        }
    }
}

fn parse_source_v4(addr: &str) -> Result<Ipv4Addr> {
    addr.parse::<Ipv4Addr>()
        .with_context(|| format!("Invalid IPv4 source address: {addr}"))
}

fn parse_source_v6(addr: &str) -> Result<Ipv6Addr> {
    addr.parse::<Ipv6Addr>()
        .with_context(|| format!("Invalid IPv6 source address: {addr}"))
}

/// IPv6 source-specific multicast (SSM) join via raw `setsockopt`
/// (`MCAST_JOIN_SOURCE_GROUP`, RFC 3678). socket2 0.6 doesn't expose this for
/// IPv6, so we wire `group_source_req` ourselves. Linux + macOS only — other
/// targets surface a build-time error.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn join_ssm_v6(
    socket: &Socket,
    group: &Ipv6Addr,
    source: &Ipv6Addr,
    iface_index: u32,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const MCAST_JOIN_SOURCE_GROUP: libc::c_int = 46;
    #[cfg(target_os = "macos")]
    const MCAST_JOIN_SOURCE_GROUP: libc::c_int = 82;

    #[repr(C)]
    struct GroupSourceReq {
        gsr_interface: u32,
        gsr_group: libc::sockaddr_storage,
        gsr_source: libc::sockaddr_storage,
    }

    let mut req: GroupSourceReq = unsafe { std::mem::zeroed() };
    req.gsr_interface = iface_index;

    // Populate the group sockaddr_in6 in place inside gsr_group.
    {
        let sa = &mut req.gsr_group as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6;
        unsafe {
            (*sa).sin6_family = libc::AF_INET6 as libc::sa_family_t;
            (*sa).sin6_addr.s6_addr = group.octets();
        }
    }
    // Populate the source sockaddr_in6 in place inside gsr_source.
    {
        let sa = &mut req.gsr_source as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6;
        unsafe {
            (*sa).sin6_family = libc::AF_INET6 as libc::sa_family_t;
            (*sa).sin6_addr.s6_addr = source.octets();
        }
    }

    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            MCAST_JOIN_SOURCE_GROUP,
            &req as *const _ as *const libc::c_void,
            std::mem::size_of::<GroupSourceReq>() as libc::socklen_t,
        )
    };

    if ret < 0 {
        return Err(anyhow::Error::from(std::io::Error::last_os_error()))
            .context("setsockopt MCAST_JOIN_SOURCE_GROUP failed");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn join_ssm_v6(
    _socket: &Socket,
    _group: &Ipv6Addr,
    _source: &Ipv6Addr,
    _iface_index: u32,
) -> Result<()> {
    anyhow::bail!("IPv6 source-specific multicast (SSM) is not supported on this OS");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_v4_accepts_unicast() {
        assert_eq!(parse_source_v4("10.0.0.5").unwrap(), Ipv4Addr::new(10, 0, 0, 5));
    }

    #[test]
    fn parse_source_v4_rejects_garbage() {
        assert!(parse_source_v4("not-an-ip").is_err());
    }

    #[test]
    fn parse_source_v6_accepts_unicast() {
        let v: Ipv6Addr = "2001:db8::5".parse().unwrap();
        assert_eq!(parse_source_v6("2001:db8::5").unwrap(), v);
    }

    /// SSM IPv4 join via socket2 — uses the loopback group 232.0.0.1 and
    /// source 127.0.0.1 against an unbound socket (we just need setsockopt
    /// to succeed; no actual traffic flow). Skipped if the OS rejects (e.g.
    /// loopback iface lacks multicast — rare but possible).
    #[tokio::test]
    async fn ssm_v4_join_setsockopt_succeeds_on_loopback() {
        let bind = "232.0.0.1:0";
        let res = bind_udp_input(bind, Some("127.0.0.1"), Some("127.0.0.2")).await;
        match res {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Failed to join SSM") {
                    eprintln!("ssm_v4 join failed (expected on some kernels): {msg}");
                } else {
                    panic!("unexpected error from bind_udp_input: {msg}");
                }
            }
        }
    }
}
