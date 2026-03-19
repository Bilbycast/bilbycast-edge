//! Pure-Rust RTMP implementation for live publishing and ingest.
//!
//! This module implements a minimal RTMP (Real-Time Messaging Protocol) stack
//! suitable for both publishing H.264 + AAC streams to an RTMP server (output)
//! and accepting incoming publish connections from OBS, ffmpeg, etc. (input).
//!
//! - **Handshake** (`handshake`): the three-phase C0/C1/C2 exchange.
//! - **Chunk streams** (`chunk`): RTMP's chunked message framing protocol.
//! - **AMF0** (`amf0`): Action Message Format encoding/decoding for command messages.
//! - **FLV tags** (`flv`): building FLV-style audio/video tags (AVC + AAC).
//! - **Session** (`session`): the high-level publish state machine with reconnection.
//! - **Server** (`server`): RTMP server that accepts incoming publish connections.
//! - **TS Muxer** (`ts_mux`): Muxes H.264/AAC into MPEG-TS for the broadcast channel.
//!
//! All I/O is async via tokio. TLS support requires the `tls` feature (tokio-rustls).

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;
pub mod server;
pub mod session;
pub mod ts_mux;

pub use session::RtmpSession;
