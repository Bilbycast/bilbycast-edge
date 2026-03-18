//! Pure-Rust RTMP client implementation for live publishing.
//!
//! This module implements a minimal RTMP (Real-Time Messaging Protocol) client
//! suitable for publishing H.264 + AAC streams to an RTMP server (e.g., Nginx-RTMP,
//! YouTube Live, Twitch). It handles:
//!
//! - **Handshake** (`handshake`): the three-phase C0/C1/C2 exchange.
//! - **Chunk streams** (`chunk`): RTMP's chunked message framing protocol.
//! - **AMF0** (`amf0`): Action Message Format encoding/decoding for command messages.
//! - **FLV tags** (`flv`): building FLV-style audio/video tags (AVC + AAC).
//! - **Session** (`session`): the high-level publish state machine with reconnection.
//!
//! All I/O is async via tokio. TLS support requires the `tls` feature (tokio-rustls).

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;
pub mod session;

pub use session::RtmpSession;
