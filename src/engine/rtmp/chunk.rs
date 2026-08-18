// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

/// Hard cap on the length of any single RTMP message we will reassemble. The
/// RTMP `message_length` field is 24-bit, so a valid message can never exceed
/// this — the cap therefore NEVER rejects a conformant stream (media,
/// aggregate, command, or data). It is a per-connection memory backstop only.
pub const MAX_RTMP_MSG_LEN: usize = 16 * 1024 * 1024;

/// Cap the RTMP server puts on any single **command or data** message
/// (`COMMAND_AMF0` / `DATA_AMF0`) — the only two types it AMF0-decodes.
/// Installed once, immediately after the credential-free handshake, and never
/// lifted for the life of the connection.
///
/// [`MAX_RTMP_MSG_LEN`] is the protocol maximum and by construction rejects
/// nothing, which makes it useless as a bound here: a 24-bit `message_length`
/// of `0xFFFFFF` is what feeds ~16 MiB of attacker bytes into the AMF0 decoder
/// per connection. Nothing legitimate is anywhere near that size — a real
/// `connect` is 200–600 bytes from OBS / ffmpeg / Wirecast, and stays inside a
/// couple of KB even with a long `tcUrl`, `pageUrl` and `swfUrl`;
/// `releaseStream` / `FCPublish` / `createStream` / `publish` are tens of
/// bytes; a live `onMetaData` is a few hundred. The one AMF0 payload that
/// exists in the wild anywhere near this size is an FLV **VOD** keyframe index
/// (`filepositions` + `times`) — a file construct a live publisher cannot
/// produce. 256 KiB is ~400× the largest of those, and for comparison
/// nginx-rtmp's `max_message` default (1 MiB) applies to *every* message
/// including video.
///
/// The cap is scoped **by message type, not by connection phase**. `VIDEO` /
/// `AUDIO` (and every control type) keep [`MAX_RTMP_MSG_LEN`], so a keyframe at
/// contribution bitrates is never constrained. Phase scoping looked equivalent
/// and is not: an RTMP input may legally carry no stream key at all
/// (`RtmpInputConfig.stream_key` is an `Option`), and on such an input the
/// post-`publish` phase is reachable by any anonymous peer after one round
/// trip — a phase-scoped cap buys that deployment nothing.
///
/// Note this is a *memory* bound, not the amplification fix. The decoded-value
/// budget in [`super::amf0`] is what stops a small message from becoming a
/// large heap, and `MAX_CHUNK_STREAMS` is what stops a flood of fresh `cs_id`s
/// from retaining a map entry each. All three, deliberately — each alone
/// leaves a gap.
pub const MAX_PREPUBLISH_MSG_LEN: usize = 256 * 1024;

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
    if (2..=63).contains(&cs_id) {
        buf.put_u8(fmt_bits | cs_id as u8);
    } else if (64..=319).contains(&cs_id) {
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

/// Chunk streams one connection may open. RTMP multiplexes a handful
/// (2 = control, 3 = command, 4/6 = media); nginx-rtmp's `max_streams` default
/// is 32. Unbounded, a 3-byte `fmt 3` basic header on a fresh `cs_id` both
/// allocates a permanent map entry AND returns a complete zero-length message
/// (a default state's `msg_len` is 0, so the reassembly completes without a
/// body ever arriving) — measured at ~17× retained amplification on the
/// unauthenticated pre-publish path, with every one of those messages also
/// resetting the pre-publish read timeout.
///
/// 64 is >10× what any real encoder opens, so it cannot reject a conforming
/// peer.
const MAX_CHUNK_STREAMS: usize = 64;

/// Tracks per-chunk-stream state needed for reassembly.
#[derive(Clone, Debug, Default)]
struct ChunkStreamState {
    msg_type: u8,
    msg_len: u32,
    timestamp: u32,
    stream_id: u32,
    /// Accumulated payload bytes for the message currently being reassembled.
    payload: Vec<u8>,
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
    /// Largest `COMMAND_AMF0` / `DATA_AMF0` message this reader will
    /// reassemble. Defaults to the protocol maximum; the RTMP server tightens
    /// it to [`MAX_PREPUBLISH_MSG_LEN`] via
    /// [`ChunkReader::set_max_command_len`]. Media and control types are not
    /// affected — they are always bounded by [`MAX_RTMP_MSG_LEN`].
    max_command_len: usize,
    /// Per-chunk-stream reassembly state (indexed by cs_id), bounded by
    /// `MAX_CHUNK_STREAMS`.
    streams: std::collections::HashMap<u32, ChunkStreamState>,
}

impl ChunkReader {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_command_len: MAX_RTMP_MSG_LEN,
            streams: std::collections::HashMap::new(),
        }
    }

    /// Set the largest AMF0-bearing message (`COMMAND_AMF0` / `DATA_AMF0`)
    /// this reader will reassemble.
    ///
    /// Used by the RTMP server to hold every peer — authenticated or not — to
    /// [`MAX_PREPUBLISH_MSG_LEN`] on the two types it hands to the AMF0
    /// decoder, without ever constraining media. Outbound clients never call
    /// this, so their behaviour is the protocol maximum, unchanged.
    pub fn set_max_command_len(&mut self, max: usize) {
        self.max_command_len = max;
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
                    ((hi as u32) << 8) | (lo as u32 + 64)
                }
                n => n as u32,
            };

            // Bound the reassembly map. A fresh cs_id costs the peer three
            // wire bytes and costs us a map entry retained for the life of the
            // connection, so the entry must not be created before the count is
            // checked. Keyed on `streams.len()`, never on `cs_id` itself.
            if !self.streams.contains_key(&cs_id) && self.streams.len() >= MAX_CHUNK_STREAMS {
                bail!("RTMP peer opened more than {MAX_CHUNK_STREAMS} chunk streams");
            }

            let state = self
                .streams
                .entry(cs_id)
                .or_default();

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

            // -- Per-message memory bound, scoped by message TYPE. The two
            //    AMF0-bearing types take whatever the owner installed (the
            //    server: MAX_PREPUBLISH_MSG_LEN, for the whole connection);
            //    media and control types take the protocol maximum, which by
            //    construction rejects nothing conformant. Checked before any
            //    allocation, so an over-long declared length costs nothing but
            //    the header bytes.
            //
            //    Scope note: the *negotiated chunk size* is not a second
            //    allocation primitive — `to_read` below is
            //    `min(remaining, chunk_size)` and `remaining` never exceeds
            //    `msg_len`, so an attacker-chosen 16 MiB Set Chunk Size can
            //    only make the reassembly buffer reach this cap in one step
            //    instead of many. That clears `chunk_size` and nothing else:
            //    `cs_id` genuinely is a second primitive (a fresh one retains
            //    a map entry for the life of the connection) and is bounded
            //    separately by MAX_CHUNK_STREAMS above.
            let cap = match state.msg_type {
                msg_type::COMMAND_AMF0 | msg_type::DATA_AMF0 => self.max_command_len,
                _ => MAX_RTMP_MSG_LEN,
            };
            if state.msg_len as usize > cap {
                bail!(
                    "RTMP message type {} length {} exceeds the {} byte cap in force for this message type",
                    state.msg_type,
                    state.msg_len,
                    cap
                );
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

/// Serialise one RTMP message into `fmt 0` + `fmt 3` chunks. Test-only mirror
/// of [`ChunkWriter::write_message`] that produces bytes instead of driving an
/// async writer, shared with the RTMP server's tests.
#[cfg(test)]
pub(super) fn test_chunk_message(
    cs_id: u8,
    msg_type: u8,
    payload: &[u8],
    chunk_size: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut first = true;
    loop {
        let take = (payload.len() - offset).min(chunk_size);
        out.push(if first { cs_id } else { (3u8 << 6) | cs_id });
        if first {
            out.extend_from_slice(&[0, 0, 0]); // timestamp
            let len = payload.len() as u32;
            out.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
            out.push(msg_type);
            out.extend_from_slice(&0u32.to_le_bytes()); // message stream id
            first = false;
        }
        out.extend_from_slice(&payload[offset..offset + take]);
        offset += take;
        if offset >= payload.len() {
            return out;
        }
    }
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `fmt 0` header declaring the 24-bit maximum message length and
    /// nothing else — the first half of the unauthenticated amplification
    /// primitive (the second half is the AMF0 body it would have fed).
    fn oversized_fmt0_header(msg_type: u8) -> Vec<u8> {
        let mut hdr = vec![0x03u8]; // fmt 0, cs_id 3
        hdr.extend_from_slice(&[0, 0, 0]); // timestamp
        hdr.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // message_length = 16 777 215
        hdr.push(msg_type);
        hdr.extend_from_slice(&0u32.to_le_bytes());
        hdr
    }

    /// A bare `fmt 3` basic header — three wire bytes at most, no message
    /// header, no body. On a fresh cs_id this both creates a map entry and
    /// completes a zero-length message, which is the retained-amplification
    /// primitive `MAX_CHUNK_STREAMS` exists to bound.
    fn bare_fmt3_header(cs_id: u32) -> Vec<u8> {
        let mut buf = BytesMut::new();
        write_basic_header(&mut buf, 3, cs_id);
        buf.to_vec()
    }

    #[tokio::test]
    async fn prepublish_cap_refuses_a_24_bit_maximum_message_length() {
        let mut reader = ChunkReader::new();
        reader.set_max_command_len(MAX_PREPUBLISH_MSG_LEN);
        let bytes = oversized_fmt0_header(msg_type::COMMAND_AMF0);
        let mut cursor: &[u8] = &bytes;
        let err = reader
            .read_message(&mut cursor)
            .await
            .expect_err("a 16 MiB pre-publish message must be refused");
        assert!(
            format!("{err:#}").contains("exceeds the"),
            "expected the phase cap to fire before any allocation, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn prepublish_cap_admits_a_realistically_sized_command() {
        // Far larger than any real `connect` (200–600 bytes), still accepted.
        let payload = vec![0xAAu8; 8 * 1024];
        let bytes = test_chunk_message(3, msg_type::COMMAND_AMF0, &payload, 128);
        let mut reader = ChunkReader::new();
        reader.set_max_command_len(MAX_PREPUBLISH_MSG_LEN);
        let mut cursor: &[u8] = &bytes;
        let msg = reader.read_message(&mut cursor).await.expect("8 KiB command must be accepted");
        assert_eq!(msg.msg_type, msg_type::COMMAND_AMF0);
        assert_eq!(msg.payload, payload);
    }

    #[tokio::test]
    async fn default_reader_still_reassembles_a_large_media_message() {
        // The post-publish path is unchanged: a Set Chunk Size followed by a
        // 300 KiB video message — well past MAX_PREPUBLISH_MSG_LEN — still
        // reassembles byte-for-byte on a default reader.
        let payload: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
        let mut bytes = test_chunk_message(2, msg_type::SET_CHUNK_SIZE, &65_536u32.to_be_bytes(), 128);
        bytes.extend_from_slice(&test_chunk_message(4, msg_type::VIDEO, &payload, 65_536));

        let mut reader = ChunkReader::new();
        let mut cursor: &[u8] = &bytes;
        let set = reader.read_message(&mut cursor).await.unwrap();
        assert_eq!(set.msg_type, msg_type::SET_CHUNK_SIZE);
        let video = reader.read_message(&mut cursor).await.expect("large media message");
        assert_eq!(video.msg_type, msg_type::VIDEO);
        assert_eq!(video.payload, payload);
    }

    #[tokio::test]
    async fn command_cap_does_not_constrain_media_on_the_same_reader() {
        // The cap is scoped by message type, not by connection phase: one
        // reader holding the command cap must still reassemble a 300 KiB
        // keyframe (well past MAX_PREPUBLISH_MSG_LEN) and must still refuse an
        // over-long COMMAND_AMF0 — no phase transition involved, so an input
        // configured with no stream key is protected too.
        let mut reader = ChunkReader::new();
        reader.set_max_command_len(MAX_PREPUBLISH_MSG_LEN);

        let payload: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
        let bytes = test_chunk_message(4, msg_type::VIDEO, &payload, 128);
        let mut cursor: &[u8] = &bytes;
        let video = reader
            .read_message(&mut cursor)
            .await
            .expect("media must never be held to the command cap");
        assert_eq!(video.msg_type, msg_type::VIDEO);
        assert_eq!(video.payload, payload);

        let hdr = oversized_fmt0_header(msg_type::COMMAND_AMF0);
        let mut cursor: &[u8] = &hdr;
        let err = reader
            .read_message(&mut cursor)
            .await
            .expect_err("an over-long command must still be refused");
        assert!(
            format!("{err:#}").contains("exceeds the"),
            "expected the command cap to fire, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn chunk_stream_map_admits_sixty_four_and_refuses_the_sixty_fifth() {
        // Each bare `fmt 3` header on a fresh cs_id returns a complete
        // zero-length message and retains a map entry. Unbounded, 3 wire bytes
        // buy a permanent entry — the ~17x retained amplification. Bounded, the
        // 65th distinct cs_id ends the connection.
        let ids: Vec<u32> = (2..=63).chain(64..=65).collect();
        assert_eq!(ids.len(), MAX_CHUNK_STREAMS);

        let mut reader = ChunkReader::new();
        for id in &ids {
            let bytes = bare_fmt3_header(*id);
            let mut cursor: &[u8] = &bytes;
            let msg = reader
                .read_message(&mut cursor)
                .await
                .unwrap_or_else(|e| panic!("cs_id {id} must be accepted, got: {e:#}"));
            assert_eq!(msg.payload.len(), 0);
        }

        let bytes = bare_fmt3_header(66);
        let mut cursor: &[u8] = &bytes;
        let err = reader
            .read_message(&mut cursor)
            .await
            .expect_err("the 65th distinct chunk stream must be refused");
        assert!(
            format!("{err:#}").contains("chunk streams"),
            "expected the chunk-stream bound to fire, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn a_normal_four_chunk_stream_session_still_works() {
        // The shape every real encoder uses: control on 2, commands on 3,
        // audio on 4, video on 6 — interleaved, and revisited. Must pass with
        // or without the bound.
        let mut bytes = test_chunk_message(2, msg_type::SET_CHUNK_SIZE, &4096u32.to_be_bytes(), 128);
        bytes.extend_from_slice(&test_chunk_message(3, msg_type::COMMAND_AMF0, b"cmd", 4096));
        bytes.extend_from_slice(&test_chunk_message(4, msg_type::AUDIO, &[0xAF; 700], 4096));
        bytes.extend_from_slice(&test_chunk_message(6, msg_type::VIDEO, &[0x17; 9000], 4096));
        bytes.extend_from_slice(&test_chunk_message(4, msg_type::AUDIO, &[0xAF; 700], 4096));
        bytes.extend_from_slice(&test_chunk_message(6, msg_type::VIDEO, &[0x27; 9000], 4096));

        let mut reader = ChunkReader::new();
        let mut cursor: &[u8] = &bytes;
        let types: Vec<u8> = {
            let mut got = Vec::new();
            for _ in 0..6 {
                got.push(reader.read_message(&mut cursor).await.unwrap().msg_type);
            }
            got
        };
        assert_eq!(
            types,
            vec![
                msg_type::SET_CHUNK_SIZE,
                msg_type::COMMAND_AMF0,
                msg_type::AUDIO,
                msg_type::VIDEO,
                msg_type::AUDIO,
                msg_type::VIDEO,
            ]
        );
    }

    #[tokio::test]
    async fn default_reader_does_not_apply_the_prepublish_cap() {
        // MAX_RTMP_MSG_LEN behaviour is untouched: 0xFFFFFF is legal, so the
        // default reader accepts the header and goes on to read the body.
        let mut reader = ChunkReader::new();
        let bytes = oversized_fmt0_header(msg_type::VIDEO);
        let mut cursor: &[u8] = &bytes;
        let err = reader.read_message(&mut cursor).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to read chunk payload"),
            "default reader must not apply the pre-publish cap, got: {err:#}"
        );
    }
}
