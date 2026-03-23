// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! RTMP chunk stream framing.
//!
//! RTMP multiplexes messages over a single TCP connection using *chunk streams*.
//! Each RTMP message is split into one or more chunks whose maximum payload size
//! is negotiated via the **Set Chunk Size** control message (type 1).
//!
//! ## Chunk format
//!
//! Every chunk starts with a **Basic Header** (1-3 bytes) encoding:
//! - `fmt` (2 bits): header type (0 = full, 1 = same stream, 2 = same length+type, 3 = continuation)
//! - `cs_id` (chunk stream ID): 2..65599
//!
//! Followed by an optional **Message Header** whose size depends on `fmt`:
//! - fmt 0 (11 bytes): timestamp(3) + message_length(3) + message_type(1) + stream_id(4 LE)
//! - fmt 1 (7 bytes):  timestamp_delta(3) + message_length(3) + message_type(1)
//! - fmt 2 (3 bytes):  timestamp_delta(3)
//! - fmt 3 (0 bytes):  continuation of the previous chunk
//!
//! If the timestamp/delta >= 0xFFFFFF, an **Extended Timestamp** (4 bytes, big-endian)
//! is appended after the message header.
//!
//! ## Chunk stream IDs used in this implementation
//!
//! - CSID 2: protocol control messages (set chunk size, acknowledgement, etc.)
//! - CSID 3: AMF command messages (connect, createStream, publish)
//! - CSID 4: audio data
//! - CSID 5: video data (some servers also accept 6)
use anyhow::{Context, Result, bail};
use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default chunk size per RTMP spec.
pub const DEFAULT_CHUNK_SIZE: u32 = 128;

/// The chunk size we request from the server (and use for sending).
/// 4096 is a common choice that reduces header overhead.
pub const DESIRED_CHUNK_SIZE: u32 = 4096;

/// RTMP message type IDs.
pub mod msg_type {
    /// Set Chunk Size (protocol control, chunk stream 2).
    pub const SET_CHUNK_SIZE: u8 = 1;
    /// Window Acknowledgement Size.
    pub const WINDOW_ACK_SIZE: u8 = 5;
    /// Set Peer Bandwidth.
    pub const SET_PEER_BANDWIDTH: u8 = 6;
    /// Audio data.
    pub const AUDIO: u8 = 8;
    /// Video data.
    pub const VIDEO: u8 = 9;
    /// AMF0 command message.
    pub const COMMAND_AMF0: u8 = 20;
    /// AMF0 data message (onMetaData, etc.).
    pub const DATA_AMF0: u8 = 18;
}

// ---------------------------------------------------------------------------
// ChunkWriter — serialises RTMP messages into chunks
// ---------------------------------------------------------------------------

/// Writes RTMP messages as properly chunked data to an async writer.
///
/// Tracks per-chunk-stream state so it can emit compact `fmt 1/2/3` headers
/// when consecutive messages share the same stream/type.
pub struct ChunkWriter {
    /// The negotiated outbound chunk size.
    chunk_size: u32,
    /// Scratch buffer used to assemble an entire chunked message before flushing.
    buf: BytesMut,
}

impl ChunkWriter {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            buf: BytesMut::with_capacity(8192),
        }
    }

    /// Serialise an RTMP message into chunks and write them to `stream`.
    ///
    /// Always uses `fmt 0` (full header) for simplicity and correctness.
    /// A production implementation could delta-compress headers, but fmt-0
    /// is accepted by all servers and is simpler to reason about.
    pub async fn write_message<S>(
        &mut self,
        stream: &mut S,
        cs_id: u32,
        msg_type: u8,
        msg_stream_id: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<()>
    where
        S: AsyncWriteExt + Unpin + Send,
    {
        self.buf.clear();

        let msg_len = payload.len() as u32;
        let use_extended_ts = timestamp >= 0x00FF_FFFF;
        let ts_field = if use_extended_ts { 0x00FF_FFFF } else { timestamp };

        // We write the first chunk with fmt=0, and continuation chunks with fmt=3.
        let chunk_size = self.chunk_size as usize;
        let mut offset = 0usize;

        while offset < payload.len() || offset == 0 {
            let is_first = offset == 0;
            let remaining = payload.len().saturating_sub(offset);
            let chunk_payload_len = remaining.min(chunk_size);

            // -- Basic header --
            let fmt: u8 = if is_first { 0 } else { 3 };
            write_basic_header(&mut self.buf, fmt, cs_id);

            // -- Message header (only for fmt 0) --
            if is_first {
                // timestamp (3 bytes, big-endian)
                self.buf.put_u8((ts_field >> 16) as u8);
                self.buf.put_u8((ts_field >> 8) as u8);
                self.buf.put_u8(ts_field as u8);
                // message length (3 bytes, big-endian) — total, not per-chunk
                self.buf.put_u8((msg_len >> 16) as u8);
                self.buf.put_u8((msg_len >> 8) as u8);
                self.buf.put_u8(msg_len as u8);
                // message type
                self.buf.put_u8(msg_type);
                // message stream id (4 bytes, little-endian per spec)
                self.buf.put_u32_le(msg_stream_id);
            }

            // -- Extended timestamp (for both fmt 0 and fmt 3 when ts >= 0xFFFFFF) --
            if use_extended_ts {
                self.buf.put_u32(timestamp);
            }

            // -- Chunk payload --
            if chunk_payload_len > 0 {
                self.buf.put_slice(&payload[offset..offset + chunk_payload_len]);
            }

            offset += chunk_payload_len;

            // If payload is empty (e.g. some control messages) break after writing the header.
            if payload.is_empty() {
                break;
            }
        }

        stream
            .write_all(&self.buf)
            .await
            .context("chunk write failed")?;

        Ok(())
    }

    /// Send a Set Chunk Size (type 1) control message and update internal state.
    pub async fn write_set_chunk_size<S>(&mut self, stream: &mut S, size: u32) -> Result<()>
    where
        S: AsyncWriteExt + Unpin + Send,
    {
        // Payload: 4 bytes, big-endian, MSB must be 0.
        let payload = (size & 0x7FFF_FFFF).to_be_bytes();
        self.write_message(stream, 2, msg_type::SET_CHUNK_SIZE, 0, 0, &payload)
            .await?;
        self.chunk_size = size;
        Ok(())
    }
}

/// Write the RTMP basic header (1-3 bytes) for a given `fmt` and `cs_id`.
///
/// - cs_id 2..63: 1-byte form  `[fmt:2 | cs_id:6]`
/// - cs_id 64..319: 2-byte form `[fmt:2 | 0:6] [cs_id - 64]`
/// - cs_id 320..65599: 3-byte form `[fmt:2 | 1:6] [low byte] [high byte]`
fn write_basic_header(buf: &mut BytesMut, fmt: u8, cs_id: u32) {
    let fmt_bits = (fmt & 0x03) << 6;
    if cs_id >= 2 && cs_id <= 63 {
        buf.put_u8(fmt_bits | cs_id as u8);
    } else if cs_id >= 64 && cs_id <= 319 {
        buf.put_u8(fmt_bits); // lower 6 bits = 0
        buf.put_u8((cs_id - 64) as u8);
    } else {
        buf.put_u8(fmt_bits | 1); // lower 6 bits = 1
        let adjusted = cs_id - 64;
        buf.put_u8(adjusted as u8);
        buf.put_u8((adjusted >> 8) as u8);
    }
}

// ---------------------------------------------------------------------------
// ChunkReader — reads incoming RTMP messages (reassembles chunks)
// ---------------------------------------------------------------------------

/// Tracks per-chunk-stream state needed for reassembly.
#[derive(Clone, Debug)]
struct ChunkStreamState {
    msg_type: u8,
    msg_len: u32,
    timestamp: u32,
    stream_id: u32,
    /// Accumulated payload bytes for the message currently being reassembled.
    payload: Vec<u8>,
}

impl Default for ChunkStreamState {
    fn default() -> Self {
        Self {
            msg_type: 0,
            msg_len: 0,
            timestamp: 0,
            stream_id: 0,
            payload: Vec::new(),
        }
    }
}

/// A fully reassembled RTMP message.
#[derive(Debug)]
pub struct RtmpMessage {
    pub msg_type: u8,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// Reads RTMP chunks from an async reader and reassembles complete messages.
pub struct ChunkReader {
    /// Inbound chunk size (may be updated by Set Chunk Size messages).
    chunk_size: u32,
    /// Per-chunk-stream reassembly state (indexed by cs_id).
    streams: std::collections::HashMap<u32, ChunkStreamState>,
}

impl ChunkReader {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: std::collections::HashMap::new(),
        }
    }

    /// Read and reassemble the next complete RTMP message.
    ///
    /// This may consume multiple chunks if the message is larger than the
    /// current chunk size. Returns the reassembled [`RtmpMessage`].
    ///
    /// **Side-effect**: if a *Set Chunk Size* control message is received,
    /// the reader automatically updates its internal chunk size.
    pub async fn read_message<S>(&mut self, stream: &mut S) -> Result<RtmpMessage>
    where
        S: AsyncReadExt + Unpin + Send,
    {
        loop {
            // -- Basic header --
            let first_byte = read_u8(stream).await?;
            let fmt = (first_byte >> 6) & 0x03;
            let cs_id = match first_byte & 0x3F {
                0 => {
                    // 2-byte form
                    let b = read_u8(stream).await?;
                    b as u32 + 64
                }
                1 => {
                    // 3-byte form
                    let lo = read_u8(stream).await?;
                    let hi = read_u8(stream).await?;
                    (hi as u32) << 8 | lo as u32 + 64
                }
                n => n as u32,
            };

            let state = self
                .streams
                .entry(cs_id)
                .or_insert_with(ChunkStreamState::default);

            // -- Message header (depends on fmt) --
            match fmt {
                0 => {
                    // Full header: timestamp(3) + msg_len(3) + msg_type(1) + stream_id(4 LE)
                    let ts = read_u24_be(stream).await?;
                    let msg_len = read_u24_be(stream).await?;
                    let msg_type = read_u8(stream).await?;
                    let stream_id = read_u32_le(stream).await?;

                    state.timestamp = ts;
                    state.msg_len = msg_len;
                    state.msg_type = msg_type;
                    state.stream_id = stream_id;
                    state.payload.clear();
                }
                1 => {
                    // timestamp_delta(3) + msg_len(3) + msg_type(1)
                    let ts_delta = read_u24_be(stream).await?;
                    let msg_len = read_u24_be(stream).await?;
                    let msg_type = read_u8(stream).await?;

                    state.timestamp = state.timestamp.wrapping_add(ts_delta);
                    state.msg_len = msg_len;
                    state.msg_type = msg_type;
                    state.payload.clear();
                }
                2 => {
                    // timestamp_delta(3) only
                    let ts_delta = read_u24_be(stream).await?;
                    state.timestamp = state.timestamp.wrapping_add(ts_delta);
                    state.payload.clear();
                }
                3 => {
                    // Continuation — no header fields.
                    // payload accumulation continues below.
                }
                _ => unreachable!(),
            }

            // -- Extended timestamp (if base timestamp was 0xFFFFFF) --
            let needs_ext = match fmt {
                0 => state.timestamp == 0x00FF_FFFF,
                1 | 2 => state.timestamp >= 0x00FF_FFFF,
                _ => false,
            };
            if needs_ext {
                state.timestamp = read_u32_be(stream).await?;
            }

            // -- Read chunk payload --
            let remaining = state.msg_len as usize - state.payload.len();
            let to_read = remaining.min(self.chunk_size as usize);
            if to_read > 0 {
                let start = state.payload.len();
                state.payload.resize(start + to_read, 0);
                stream
                    .read_exact(&mut state.payload[start..])
                    .await
                    .context("failed to read chunk payload")?;
            }

            // -- Check if message is complete --
            if state.payload.len() >= state.msg_len as usize {
                let msg = RtmpMessage {
                    msg_type: state.msg_type,
                    timestamp: state.timestamp,
                    payload: state.payload.clone(),
                };
                state.payload.clear();

                // Auto-handle Set Chunk Size.
                if msg.msg_type == msg_type::SET_CHUNK_SIZE && msg.payload.len() >= 4 {
                    let new_size = u32::from_be_bytes([
                        msg.payload[0] & 0x7F,
                        msg.payload[1],
                        msg.payload[2],
                        msg.payload[3],
                    ]);
                    if new_size == 0 || new_size > 16_777_215 {
                        bail!("invalid chunk size from server: {new_size}");
                    }
                    tracing::debug!("server set chunk size to {new_size}");
                    self.chunk_size = new_size;
                }

                return Ok(msg);
            }
            // Otherwise, continue reading the next chunk of this message.
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers — small async read utilities
// ---------------------------------------------------------------------------

async fn read_u8<S: AsyncReadExt + Unpin>(s: &mut S) -> Result<u8> {
    let mut buf = [0u8; 1];
    s.read_exact(&mut buf).await.context("read_u8")?;
    Ok(buf[0])
}

async fn read_u24_be<S: AsyncReadExt + Unpin>(s: &mut S) -> Result<u32> {
    let mut buf = [0u8; 3];
    s.read_exact(&mut buf).await.context("read_u24_be")?;
    Ok((buf[0] as u32) << 16 | (buf[1] as u32) << 8 | buf[2] as u32)
}

async fn read_u32_be<S: AsyncReadExt + Unpin>(s: &mut S) -> Result<u32> {
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.context("read_u32_be")?;
    Ok(u32::from_be_bytes(buf))
}

async fn read_u32_le<S: AsyncReadExt + Unpin>(s: &mut S) -> Result<u32> {
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.context("read_u32_le")?;
    Ok(u32::from_le_bytes(buf))
}
