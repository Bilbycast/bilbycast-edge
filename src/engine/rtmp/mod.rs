// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Pure-Rust RTMP implementation for live ingest and publish.
//!
//! This module implements a minimal RTMP (Real-Time Messaging Protocol) stack
//! for both accepting incoming publish connections (input/server) and pushing
//! media to external RTMP servers like Twitch, YouTube, etc. (output/client).
//!
//! - **Chunk streams** (`chunk`): RTMP's chunked message framing protocol.
//! - **AMF0** (`amf0`): Action Message Format encoding/decoding for command messages.
//! - **Server** (`server`): RTMP server that accepts incoming publish connections.
//! - **Client** (`client`): RTMP client that connects and publishes to external servers.
//! - **TS Muxer** (`ts_mux`): Muxes H.264/AAC into MPEG-TS for the broadcast channel.
//!
//! All I/O is async via tokio.

pub mod amf0;
pub mod chunk;
pub mod client;
pub mod server;
pub mod ts_mux;
