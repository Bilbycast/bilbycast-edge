use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

/// Create a UDP socket bound to the given address with multicast support.
/// If the bind address is a multicast group, automatically joins the group.
/// `interface_addr` optionally specifies which local interface to join multicast on.
pub async fn bind_udp_input(
    bind_addr: &str,
    interface_addr: Option<&str>,
) -> Result<UdpSocket> {
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("Invalid bind address: {bind_addr}"))?;

    if addr.ip().is_multicast() {
        bind_multicast_input(addr, interface_addr).await
    } else {
        let sock = UdpSocket::bind(addr)
            .await
            .with_context(|| format!("Failed to bind UDP socket to {addr}"))?;
        tracing::info!("UDP input socket bound to {addr} (unicast)");
        Ok(sock)
    }
}

/// Bind a multicast receive socket.
/// For multicast input we bind to 0.0.0.0:<port> (or [::]:<port>) with SO_REUSEADDR,
/// then join the multicast group.
async fn bind_multicast_input(
    mcast_addr: SocketAddr,
    interface_addr: Option<&str>,
) -> Result<UdpSocket> {
    let domain = match mcast_addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };

    // Use socket2 for SO_REUSEADDR before binding
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .context("Failed to create UDP socket")?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "macos")]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    // Bind to wildcard address with the multicast port
    let bind_to: SocketAddr = match mcast_addr {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), v4.port()),
        SocketAddr::V6(v6) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), v6.port()),
    };
    socket
        .bind(&SockAddr::from(bind_to))
        .with_context(|| format!("Failed to bind multicast socket to {bind_to}"))?;

    // Convert to tokio UdpSocket
    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)
        .context("Failed to convert socket2 socket to tokio UdpSocket")?;

    // Join multicast group
    let iface = parse_interface_v4(interface_addr)?;

    match mcast_addr.ip() {
        IpAddr::V4(group) => {
            tokio_socket
                .join_multicast_v4(group, iface)
                .with_context(|| {
                    format!("Failed to join multicast group {group} on interface {iface}")
                })?;
            tracing::info!(
                "UDP input: joined multicast group {group} on interface {iface}, port {}",
                mcast_addr.port()
            );
        }
        IpAddr::V6(group) => {
            tokio_socket
                .join_multicast_v6(&group, 0)
                .with_context(|| format!("Failed to join multicast group {group}"))?;
            tracing::info!(
                "UDP input: joined multicast group {group}, port {}",
                mcast_addr.port()
            );
        }
    }

    Ok(tokio_socket)
}

/// Create a UDP socket for sending to a destination address.
/// If `bind_addr` is provided, bind to that address. Otherwise bind to ephemeral port.
pub async fn create_udp_output(
    dest_addr: &str,
    bind_addr: Option<&str>,
    interface_addr: Option<&str>,
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

    socket
        .bind(&SockAddr::from(bind_to))
        .with_context(|| format!("Failed to bind output socket to {bind_to}"))?;

    // If destination is multicast, set the outgoing interface
    if dest.ip().is_multicast() {
        if let IpAddr::V4(_) = dest.ip() {
            let iface = parse_interface_v4(interface_addr)?;
            socket
                .set_multicast_if_v4(&iface)
                .context("Failed to set multicast interface")?;
            // TTL for multicast
            socket.set_multicast_ttl_v4(16)?;
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
