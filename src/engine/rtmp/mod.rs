//! Pure-Rust RTMP implementation for live ingest.
//!
//! This module implements a minimal RTMP (Real-Time Messaging Protocol) stack
//! for accepting incoming publish connections from OBS, ffmpeg, etc. (input).
//!
//! - **Chunk streams** (`chunk`): RTMP's chunked message framing protocol.
//! - **AMF0** (`amf0`): Action Message Format encoding/decoding for command messages.
//! - **Server** (`server`): RTMP server that accepts incoming publish connections.
//! - **TS Muxer** (`ts_mux`): Muxes H.264/AAC into MPEG-TS for the broadcast channel.
//!
//! All I/O is async via tokio.

pub mod amf0;
pub mod chunk;
pub mod server;
pub mod ts_mux;
