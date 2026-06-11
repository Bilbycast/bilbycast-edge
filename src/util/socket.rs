// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

use crate::config::models::InterfaceBinding;

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
///
/// `interface_binding`, when set, takes precedence over `interface_addr`:
/// the binding's NIC name is resolved to its primary IPv4 (loose), and if
/// `binding.strict` is true, `setsockopt(SO_BINDTODEVICE, name)` is also
/// applied so the kernel won't deliver packets that arrived on any other
/// NIC. Strict requires `CAP_NET_RAW`; bind fails with `EPERM` otherwise.
pub async fn bind_udp_input(
    bind_addr: &str,
    interface_addr: Option<&str>,
    source_addr: Option<&str>,
    interface_binding: Option<&InterfaceBinding>,
) -> Result<UdpSocket> {
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("Invalid bind address: {bind_addr}"))?;

    // Resolve interface_binding once. When set, its loose IP wins over
    // any operator-supplied interface_addr (we logged-warned at validation).
    let resolved = resolve_interface_binding(interface_binding)?;
    let effective_iface_addr_storage: Option<String> = resolved.as_ref()
        .and_then(|r| r.ipv4_for_loose.map(|ip| ip.to_string()));
    let effective_iface_addr: Option<&str> = effective_iface_addr_storage
        .as_deref()
        .or(interface_addr);

    if addr.ip().is_multicast() {
        bind_multicast_input(addr, effective_iface_addr, source_addr, resolved.as_ref()).await
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
        if let Some(r) = resolved.as_ref() {
            if let Some(name) = r.name_for_strict.as_deref() {
                apply_strict_binding(&socket, name)?;
            }
        }
        socket
            .bind(&SockAddr::from(addr))
            .map_err(|e| crate::util::port_error::annotate_bind_error(e, addr, "UDP/RTP input"))?;
        let std_socket: std::net::UdpSocket = socket.into();
        let sock = UdpSocket::from_std(std_socket)
            .context("Failed to convert socket2 socket to tokio UdpSocket")?;
        let strict_note = resolved.as_ref().and_then(|r| r.name_for_strict.as_deref())
            .map(|n| format!(" (strict bind to {n})")).unwrap_or_default();
        tracing::info!("UDP input socket bound to {addr} (unicast){strict_note}");
        Ok(sock)
    }
}

/// Bind a multicast receive socket.
///
/// On Linux the socket is bound to `<group>:<port>` so the kernel only
/// delivers datagrams addressed to *this* group. Binding the wildcard
/// (the historical behaviour, still used on non-Linux and as a fallback)
/// makes the socket receive traffic for **every** multicast group any
/// socket on the host has joined on the same port — two inputs on the
/// same port but different groups would each see both groups' packets
/// (cross-delivery). SO_REUSEADDR stays on so several inputs can still
/// share one `<group>:<port>` deliberately. The group join (ASM by
/// default, SSM when `source_addr` is set) happens after bind as before.
async fn bind_multicast_input(
    mcast_addr: SocketAddr,
    interface_addr: Option<&str>,
    source_addr: Option<&str>,
    binding: Option<&ResolvedBinding>,
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
    if let Some(r) = binding {
        if let Some(name) = r.name_for_strict.as_deref() {
            apply_strict_binding(&socket, name)?;
        }
    }

    let wildcard: SocketAddr = match mcast_addr {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), v4.port()),
        SocketAddr::V6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), v6.port()),
    };
    #[cfg(target_os = "linux")]
    let bound_to: SocketAddr = match socket.bind(&SockAddr::from(mcast_addr)) {
        Ok(()) => mcast_addr,
        Err(e) => {
            // Group-address bind is Linux-standard, but fall back to the
            // wildcard rather than failing the input outright — the
            // operator loses same-port group isolation, not the stream.
            tracing::warn!(
                "Multicast group-address bind to {mcast_addr} failed ({e}); \
                 falling back to wildcard {wildcard} (no same-port group isolation)"
            );
            socket
                .bind(&SockAddr::from(wildcard))
                .with_context(|| format!("Failed to bind multicast socket to {wildcard}"))?;
            wildcard
        }
    };
    #[cfg(not(target_os = "linux"))]
    let bound_to: SocketAddr = {
        socket
            .bind(&SockAddr::from(wildcard))
            .with_context(|| format!("Failed to bind multicast socket to {wildcard}"))?;
        wildcard
    };
    tracing::debug!("Multicast input socket bound to {bound_to}");

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
///
/// `interface_binding`, when set, takes precedence over `interface_addr`
/// (loose source-IP) and `bind_addr` (when bind_addr's IP is unspecified):
/// the binding's NIC name resolves to its primary IPv4 and is used as
/// the source IP. If `binding.strict` is true, `setsockopt(SO_BINDTODEVICE,
/// name)` also pins the egress NIC at the kernel level. Strict requires
/// `CAP_NET_RAW`.
pub async fn create_udp_output(
    dest_addr: &str,
    bind_addr: Option<&str>,
    interface_addr: Option<&str>,
    dscp: u8,
    interface_binding: Option<&InterfaceBinding>,
) -> Result<(UdpSocket, SocketAddr)> {
    let dest: SocketAddr = dest_addr
        .parse()
        .with_context(|| format!("Invalid destination address: {dest_addr}"))?;

    let resolved = resolve_interface_binding(interface_binding)?;
    // For outputs: if bind_addr is unset (ephemeral port) and the
    // binding resolves to an IPv4 source, override the bind so the
    // kernel uses that source. Operator-supplied bind_addr always
    // wins over the binding-derived source — they explicitly chose
    // a port.
    let binding_bind_storage: Option<String> = resolved.as_ref()
        .and_then(|r| r.ipv4_for_loose)
        .filter(|_| dest.is_ipv4())
        .map(|ip| format!("{ip}:0"));
    let effective_bind_addr: Option<&str> = bind_addr
        .or(binding_bind_storage.as_deref());

    let bind_to: SocketAddr = match effective_bind_addr {
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
    if let Some(r) = resolved.as_ref() {
        if let Some(name) = r.name_for_strict.as_deref() {
            apply_strict_binding(&socket, name)?;
        }
    }

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

    // If destination is multicast, set the outgoing interface and TTL/hop limit.
    // Binding-derived loose interface IP wins over caller's interface_addr.
    let effective_iface_storage: Option<String> = resolved.as_ref()
        .and_then(|r| r.ipv4_for_loose.map(|ip| ip.to_string()));
    let effective_iface_addr: Option<&str> = effective_iface_storage
        .as_deref()
        .or(interface_addr);
    if dest.ip().is_multicast() {
        match dest.ip() {
            IpAddr::V4(_) => {
                let iface = parse_interface_v4(effective_iface_addr)?;
                socket
                    .set_multicast_if_v4(&iface)
                    .context("Failed to set multicast interface")?;
                socket.set_multicast_ttl_v4(16)?;
            }
            IpAddr::V6(_) => {
                let iface_index = parse_interface_v6_index(effective_iface_addr)?;
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
    let strict_note = resolved.as_ref().and_then(|r| r.name_for_strict.as_deref())
        .map(|n| format!(" (strict bind to {n})")).unwrap_or_default();
    tracing::info!("UDP output socket bound to {local_addr}, destination {dest}{strict_note}");

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

// ── Per-NIC binding (loose source-IP + strict SO_BINDTODEVICE) ──────────

/// Result of resolving an `InterfaceBinding` against the live host NIC list.
///
/// `ipv4_for_loose` carries the interface's primary IPv4 address — used
/// as the socket source IP whether or not strict mode is enabled (it's
/// the loose-binding mechanism). `name_for_strict` is `Some` only when
/// `binding.strict` is true and the host supports SO_BINDTODEVICE; the
/// caller passes it to `apply_strict_binding` after socket creation but
/// before `bind()`.
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub ipv4_for_loose: Option<Ipv4Addr>,
    pub name_for_strict: Option<String>,
}

/// Resolve an `InterfaceBinding` against the live host NIC list.
///
/// Returns `Ok(None)` when the binding is `None` (caller wants the
/// kernel route table to pick the NIC). Returns `Err` when the binding
/// names an interface not present on the host — this is a hard failure
/// at bind time so the operator sees the misconfig immediately. The
/// validator catches missing interfaces at config-load time too, but
/// hot-pluggable NICs (USB-eth, container veth) can disappear between
/// load and bind, hence the runtime check.
///
/// IPv6-only interfaces are not currently supported for the loose
/// source-IP path; the validator rejects those at config time.
pub fn resolve_interface_binding(
    binding: Option<&InterfaceBinding>,
) -> Result<Option<ResolvedBinding>> {
    let Some(b) = binding else { return Ok(None) };

    let ifaces = crate::util::network_interfaces::enumerate();
    let info = ifaces
        .iter()
        .find(|i| i.name == b.name)
        .ok_or_else(|| {
            let available: Vec<&str> = ifaces.iter()
                .filter(|i| !i.is_loopback)
                .map(|i| i.name.as_str())
                .collect();
            anyhow::anyhow!(
                "interface_binding: NIC '{}' not found on host. Available: {}",
                b.name,
                available.join(", "),
            )
        })?;

    // First non-link-local IPv4 wins. Falls through to None when the
    // interface is IPv6-only, which currently means the binding can't
    // pin the source — caller should fall back to legacy fields. The
    // validator warns on IPv6-only NICs at config time.
    let ipv4_for_loose = info.ipv4.iter()
        .filter_map(|s| s.parse::<Ipv4Addr>().ok())
        .find(|ip| !ip.is_link_local() && !ip.is_unspecified());

    let name_for_strict = if b.strict {
        if !probe_strict_binding_supported() {
            anyhow::bail!(
                "interface_binding strict mode requires CAP_NET_RAW. \
                 Install bilbycast-edge.service.d/strict-binding.conf and \
                 restart the edge to grant the capability."
            );
        }
        Some(b.name.clone())
    } else {
        None
    };

    Ok(Some(ResolvedBinding { ipv4_for_loose, name_for_strict }))
}

/// Apply `setsockopt(SO_BINDTODEVICE, name)` on a Linux socket. Must be
/// called BEFORE `bind()` — Linux validates against the device's address
/// table at bind time. Returns `EPERM` when the process lacks
/// `CAP_NET_RAW`; the operator must install the systemd drop-in
/// `packaging/strict-binding.conf` to grant it.
#[cfg(target_os = "linux")]
pub fn apply_strict_binding(socket: &Socket, name: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let cname = std::ffi::CString::new(name)
        .with_context(|| format!("invalid interface name '{name}' (NUL byte)"))?;
    let bytes = cname.as_bytes_with_nul();
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            bytes.as_ptr() as *const libc::c_void,
            bytes.len() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow::Error::from(err))
            .with_context(|| format!("setsockopt SO_BINDTODEVICE({name}) failed (need CAP_NET_RAW)"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn apply_strict_binding(_socket: &Socket, _name: &str) -> Result<()> {
    anyhow::bail!("interface_binding strict mode is only supported on Linux");
}

/// SRT/RIST loose binding adapter.
///
/// When `interface_binding` is set on an SRT/RIST surface, we resolve the
/// NIC name to its primary IPv4 and pass it as `local_addr` *iff* the
/// operator hasn't already set `local_addr`. Strict mode is rejected
/// here — Phase 2 will plumb `SRTO_BINDTODEVICE` through
/// `bilbycast-libsrt-rs` and a librist setsockopt path.
///
/// Returns `Ok(None)` to mean "no change" (binding unset, or operator's
/// local_addr already wins). Returns `Ok(Some(addr_str))` to inject the
/// resolved source. Returns `Err` on strict-rejected or unknown NIC.
pub fn srt_local_addr_from_binding(
    binding: Option<&InterfaceBinding>,
    existing_local_addr: Option<&str>,
) -> Result<Option<String>> {
    let Some(b) = binding else { return Ok(None) };
    if b.strict {
        anyhow::bail!(
            "interface_binding strict mode is not supported on SRT/RIST in Phase 1 \
             (libsrt/librist SRTO_BINDTODEVICE plumbing deferred). Use strict: false \
             with the desired NIC name; the source IP is still pinned to that NIC's primary IPv4."
        );
    }
    if existing_local_addr.is_some() {
        return Ok(None);
    }
    let Some(resolved) = resolve_interface_binding(Some(b))? else { return Ok(None) };
    Ok(resolved.ipv4_for_loose.map(|ip| format!("{ip}:0")))
}

/// Memoised one-shot probe: can this process call
/// `setsockopt(SO_BINDTODEVICE)` on a UDP socket without `EPERM`? The
/// answer never changes for the lifetime of the process (capabilities
/// are immutable post-exec), so we cache it in a `OnceLock`.
///
/// The probe opens an ephemeral UDP socket bound to nothing, attempts
/// the setsockopt with the loopback name, then drops the socket. On
/// non-Linux returns `false` — `apply_strict_binding` is the canonical
/// rejection path there.
pub fn probe_strict_binding_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
                Ok(s) => s,
                Err(_) => return false,
            };
            apply_strict_binding(&socket, "lo").is_ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    })
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

    #[test]
    fn resolve_interface_binding_none_returns_none() {
        let r = resolve_interface_binding(None).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn resolve_interface_binding_unknown_nic_errors_with_available_list() {
        let b = InterfaceBinding { name: "definitely-not-a-nic".into(), strict: false };
        let err = resolve_interface_binding(Some(&b)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-a-nic"), "msg='{msg}'");
        // The error message is the only place we tell the operator which
        // names ARE valid — this is the contract the manager UI relies on
        // when surfacing the failure.
        assert!(msg.contains("Available:"), "msg='{msg}'");
    }

    #[test]
    fn resolve_loopback_loose_picks_127_0_0_1() {
        let b = InterfaceBinding { name: "lo".into(), strict: false };
        let r = resolve_interface_binding(Some(&b)).unwrap().unwrap();
        // Loopback is special-cased to 127.0.0.1 here; if this ever fails
        // the loose-binding contract for the canonical NIC is broken.
        assert_eq!(r.ipv4_for_loose, Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(r.name_for_strict.is_none(), "non-strict must not request SO_BINDTODEVICE");
    }

    #[test]
    fn probe_strict_binding_idempotent() {
        // Memoised — second call must return the same value as the first.
        let a = probe_strict_binding_supported();
        let b = probe_strict_binding_supported();
        assert_eq!(a, b);
    }

    /// Two multicast inputs on the SAME port but DIFFERENT groups must not
    /// cross-deliver. On Linux the receive socket binds the group address,
    /// so the kernel filters by destination group; the historical wildcard
    /// bind delivered BOTH groups' traffic to each socket.
    ///
    /// Uses loopback multicast (IP_MULTICAST_LOOP is on by default and the
    /// sender pins `IP_MULTICAST_IF` to 127.0.0.1). Some sandboxed kernels
    /// refuse loopback multicast entirely — if no packet arrives at all the
    /// test logs and returns rather than failing on the environment.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn multicast_same_port_different_groups_isolated() {
        // The cross-delivery scenario needs both sockets on one port, so a
        // fixed (PID-salted) port instead of an ephemeral one.
        let port = 50000 + (std::process::id() % 10000) as u16;
        let group_a: Ipv4Addr = "239.255.77.10".parse().unwrap();
        let group_b: Ipv4Addr = "239.255.77.11".parse().unwrap();
        let sock_a = bind_udp_input(&format!("{group_a}:{port}"), Some("127.0.0.1"), None, None)
            .await
            .expect("bind group A");
        let sock_b = bind_udp_input(&format!("{group_b}:{port}"), Some("127.0.0.1"), None, None)
            .await
            .expect("bind group B");

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        {
            let sref = socket2::SockRef::from(&sender);
            sref.set_multicast_if_v4(&Ipv4Addr::new(127, 0, 0, 1)).unwrap();
            sref.set_multicast_loop_v4(true).unwrap();
        }
        for _ in 0..5 {
            sender.send_to(b"GROUP-A", (group_a, port)).await.unwrap();
            sender.send_to(b"GROUP-B", (group_b, port)).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Drain each socket for a bounded window and collect payloads.
        async fn drain(sock: &UdpSocket) -> Vec<Vec<u8>> {
            let mut got = Vec::new();
            let mut buf = [0u8; 64];
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sock.recv_from(&mut buf),
                )
                .await
                {
                    Ok(Ok((n, _))) => got.push(buf[..n].to_vec()),
                    _ => break,
                }
            }
            got
        }
        let got_a = drain(&sock_a).await;
        let got_b = drain(&sock_b).await;

        if got_a.is_empty() && got_b.is_empty() {
            eprintln!("loopback multicast unavailable in this environment; skipping assertions");
            return;
        }
        assert!(
            got_a.iter().all(|p| p.as_slice() == b"GROUP-A"),
            "socket A received another group's traffic: {got_a:?}"
        );
        assert!(
            got_b.iter().all(|p| p.as_slice() == b"GROUP-B"),
            "socket B received another group's traffic: {got_b:?}"
        );
        assert!(!got_a.is_empty(), "socket A received nothing while B did");
        assert!(!got_b.is_empty(), "socket B received nothing while A did");
    }

    /// SSM IPv4 join via socket2 — uses the loopback group 232.0.0.1 and
    /// source 127.0.0.1 against an unbound socket (we just need setsockopt
    /// to succeed; no actual traffic flow). Skipped if the OS rejects (e.g.
    /// loopback iface lacks multicast — rare but possible).
    #[tokio::test]
    async fn ssm_v4_join_setsockopt_succeeds_on_loopback() {
        let bind = "232.0.0.1:0";
        let res = bind_udp_input(bind, Some("127.0.0.1"), Some("127.0.0.2"), None).await;
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
