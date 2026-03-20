//! IP Tunnel subsystem for bilbycast-edge.
//!
//! Provides QUIC-based tunneling between edge nodes, either through a relay
//! server (when both edges are behind NAT) or directly (when one edge has a
//! public IP).
//!
//! ## Tunnel Modes
//!
//! - **Relay**: Both edges connect to a bilbycast-relay server. The relay pairs
//!   them by tunnel UUID and forwards data bidirectionally.
//! - **Direct**: One edge listens on a public port, the other connects directly.
//!   Uses the same QUIC transport but without a relay intermediary.
//!
//! ## Protocols
//!
//! - **TCP tunnels**: Each incoming TCP connection becomes a QUIC stream.
//!   Reliable, ordered delivery (good for camera control, signaling).
//! - **UDP tunnels**: Each UDP packet becomes a QUIC datagram.
//!   Unreliable, unordered (good for SRT, which has its own retransmission).
//!
//! ## Architecture
//!
//! ```text
//! [Local App] ←UDP/TCP→ [TunnelForwarder] ←QUIC→ [Relay/Peer] ←QUIC→ [Remote TunnelForwarder] ←UDP/TCP→ [Remote App]
//! ```

pub mod auth;
pub mod config;
pub mod manager;
pub mod protocol;
pub mod quic;
pub mod relay_client;
pub mod tcp_forwarder;
pub mod udp_forwarder;

pub use config::*;
