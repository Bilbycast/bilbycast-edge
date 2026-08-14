// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RTMP server — accepts incoming publish connections.
//!
//! Listens on a TCP port and handles the RTMP handshake, `connect`, `createStream`,
//! and `publish` commands from the client. Once publishing starts, audio/video
//! messages are forwarded to the caller via a channel.
//!
//! ## Supported encoders
//!
//! Any RTMP encoder that follows the standard publish flow:
//! - OBS Studio
//! - ffmpeg (`-f flv rtmp://host:port/app/key`)
//! - Wirecast, vMix, etc.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut, BufMut};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use super::amf0::{self, Amf0Value};
use super::chunk::{
    ChunkReader, ChunkWriter, msg_type, DESIRED_CHUNK_SIZE, MAX_PREPUBLISH_MSG_LEN,
};

/// Maximum number of concurrent RTMP client connections. Bounds FD / memory /
/// task usage against an unauthenticated flood (slowloris). RTMP publish is a
/// single-source contribution protocol, so a small cap is ample; excess
/// connections are dropped immediately.
const MAX_RTMP_CONNECTIONS: usize = 8;

/// Time budget for the server-side handshake. A peer that connects and then
/// stalls mid-handshake is dropped rather than parking a task forever. The
/// handshake is a quick fixed exchange, so 15 s is far above any legitimate
/// timing.
const RTMP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-message time budget for the pre-publish command phase (connect →
/// createStream → publish). Defeats a slowloris that dribbles bytes before
/// authenticating, but is deliberately generous (60 s) so an armed-and-waiting
/// contribution encoder that connects before it starts publishing is not
/// dropped. Slowloris resource use is bounded by MAX_RTMP_CONNECTIONS
/// regardless, so we can afford the margin.
const RTMP_PREPUBLISH_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Idle-read timeout once publishing: if the publisher stops sending media
/// (and never closes the socket) we reclaim the connection instead of parking
/// on a bare read forever. Sized for broadcast source-loss ride-through — many
/// hardware contribution encoders (Haivision/Elemental) stop emitting RTMP
/// media on SDI/upstream loss but hold the session open and resume seamlessly
/// on the SAME session when the source returns; a shorter window would force an
/// avoidable full reconnect + IDR/GOP re-alignment on every source switch. The
/// DoS pressure is bounded by MAX_RTMP_CONNECTIONS (single publisher per input,
/// 8x headroom), and a clean FIN/RST reconnect frees its permit immediately, so
/// a long idle window is safe.
const RTMP_MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Media data received from the RTMP publisher.
#[derive(Debug, Clone)]
pub enum RtmpMediaMessage {
    /// H.264 video tag body (FLV format: frame_type + codec_id + AVC packet type + data).
    Video {
        data: Bytes,
        timestamp_ms: u32,
    },
    /// AAC audio tag body (FLV format: sound_format + AAC packet type + data).
    Audio {
        data: Bytes,
        timestamp_ms: u32,
    },
    /// Metadata message (onMetaData).
    Metadata,
    /// Publisher disconnected.
    Disconnected,
}

/// Configuration for the RTMP server.
pub struct RtmpServerConfig {
    pub listen_addr: String,
    pub expected_app: String,
    pub expected_stream_key: Option<String>,
}

/// Run the RTMP server, accepting one publisher at a time.
///
/// Media messages from the publisher are sent to `media_tx`.
/// The server runs until `cancel` is triggered.
pub async fn run_rtmp_server(
    config: RtmpServerConfig,
    media_tx: mpsc::Sender<RtmpMediaMessage>,
    is_publishing: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(&config.listen_addr).await.map_err(|e| {
        crate::util::port_error::annotate_bind_error(e, &config.listen_addr, "RTMP server")
    })?;

    tracing::info!("RTMP server listening on {}", config.listen_addr);

    let conn_sem = Arc::new(Semaphore::new(MAX_RTMP_CONNECTIONS));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTMP server shutting down");
                return Ok(());
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        // Bound concurrent connections. If we're at capacity,
                        // drop the newcomer immediately rather than spawning an
                        // unbounded task (slowloris / FD-exhaustion guard).
                        let permit = match conn_sem.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::warn!(
                                    "RTMP connection limit ({MAX_RTMP_CONNECTIONS}) reached; rejecting {addr}"
                                );
                                drop(stream);
                                continue;
                            }
                        };
                        tracing::info!("RTMP client connected from {addr}");
                        stream.set_nodelay(true).ok();

                        let media_tx = media_tx.clone();
                        let cancel = cancel.child_token();
                        let expected_app = config.expected_app.clone();
                        let expected_key = config.expected_stream_key.clone();
                        let is_pub = is_publishing.clone();

                        tokio::spawn(async move {
                            // Hold the permit for the connection's lifetime.
                            let _permit = permit;
                            if let Err(e) = handle_rtmp_client(
                                stream, addr, &expected_app, expected_key.as_deref(),
                                media_tx, is_pub.clone(), cancel,
                            ).await {
                                tracing::warn!("RTMP client {addr} error: {e:#}");
                            }
                            is_pub.store(false, Ordering::Relaxed);
                            tracing::info!("RTMP client {addr} disconnected");
                        });
                    }
                    Err(e) => {
                        tracing::warn!("RTMP accept error: {e}");
                    }
                }
            }
        }
    }
}

/// Handle a single RTMP client connection.
async fn handle_rtmp_client<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut stream: S,
    addr: std::net::SocketAddr,
    expected_app: &str,
    expected_stream_key: Option<&str>,
    media_tx: mpsc::Sender<RtmpMediaMessage>,
    is_publishing: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<()> {
    // 1. RTMP handshake (server side), time-bounded so a stalled peer can't
    //    park this task indefinitely.
    tokio::time::timeout(RTMP_HANDSHAKE_TIMEOUT, server_handshake(&mut stream))
        .await
        .map_err(|_| anyhow::anyhow!("RTMP handshake timed out after {RTMP_HANDSHAKE_TIMEOUT:?}"))??;
    tracing::debug!("RTMP handshake complete with {addr}");

    let mut writer = ChunkWriter::new();
    let mut reader = ChunkReader::new();
    // Hold the two AMF0-bearing message types to the command cap for the whole
    // connection. Deliberately not lifted after `publish`: `expected_stream_key`
    // is an `Option`, so on an input configured without one there is no
    // credential to present and "post-publish" is one round trip from anonymous
    // — a phase-scoped cap would protect the keyed deployment only. Media is
    // unaffected at either end of that: VIDEO / AUDIO keep the protocol
    // maximum, which is what a contribution-bitrate keyframe needs.
    reader.set_max_command_len(MAX_PREPUBLISH_MSG_LEN);

    // 2. Send server Window Acknowledgement Size and Set Peer Bandwidth
    let ack_size: u32 = 2_500_000;
    let mut buf = BytesMut::with_capacity(4);
    buf.put_u32(ack_size);
    writer.write_message(&mut stream, 2, msg_type::WINDOW_ACK_SIZE, 0, 0, &buf).await?;

    buf.clear();
    buf.put_u32(ack_size);
    buf.put_u8(2); // Dynamic bandwidth limit type
    writer.write_message(&mut stream, 2, msg_type::SET_PEER_BANDWIDTH, 0, 0, &buf).await?;

    // Send Set Chunk Size
    writer.write_set_chunk_size(&mut stream, DESIRED_CHUNK_SIZE).await?;
    stream.flush().await?;

    // 3. Wait for connect command
    let mut publish_stream_id: u32 = 0;
    let mut app_name = String::new();

    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            r = tokio::time::timeout(
                RTMP_PREPUBLISH_READ_TIMEOUT,
                reader.read_message(&mut stream),
            ) => {
                r.map_err(|_| anyhow::anyhow!(
                    "RTMP {addr}: timed out waiting for command (pre-publish slowloris guard)"
                ))??
            }
        };

        match msg.msg_type {
            msg_type::SET_CHUNK_SIZE => {
                if msg.payload.len() >= 4 {
                    let new_size = u32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
                    // ChunkReader handles this automatically via read_message()
                    let _ = new_size;
                    tracing::debug!("Client set chunk size to {new_size}");
                }
            }
            msg_type::COMMAND_AMF0 => {
                let values = amf0::decode_all(&msg.payload)
                    .context("failed to decode AMF0 command")?;
                let cmd = values.first().and_then(|v| v.as_str()).unwrap_or("");
                let tx_id = values.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);

                match cmd {
                    "connect" => {
                        // Extract app name from connect properties
                        if let Some(props) = values.get(2) {
                            app_name = props.get_property("app")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                        }

                        tracing::debug!("RTMP connect: app='{app_name}'");

                        // Validate app name
                        if !expected_app.is_empty() && app_name != expected_app {
                            tracing::warn!("RTMP: rejected connect for app '{app_name}' (expected '{expected_app}')");
                            // Send _error
                            let resp = amf0::encode_values(&[
                                Amf0Value::String("_error".into()),
                                Amf0Value::Number(tx_id),
                                Amf0Value::Null,
                                Amf0Value::Object(vec![
                                    ("level".into(), Amf0Value::String("error".into())),
                                    ("code".into(), Amf0Value::String("NetConnection.Connect.Rejected".into())),
                                    ("description".into(), Amf0Value::String("Application not found".into())),
                                ]),
                            ]);
                            writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, 0, 0, &resp).await?;
                            stream.flush().await?;
                            bail!("App name mismatch");
                        }

                        // Send _result for connect
                        let resp = amf0::encode_values(&[
                            Amf0Value::String("_result".into()),
                            Amf0Value::Number(tx_id),
                            Amf0Value::Object(vec![
                                ("fmsVer".into(), Amf0Value::String("FMS/3,5,7,7009".into())),
                                ("capabilities".into(), Amf0Value::Number(31.0)),
                                ("mode".into(), Amf0Value::Number(1.0)),
                            ]),
                            Amf0Value::Object(vec![
                                ("level".into(), Amf0Value::String("status".into())),
                                ("code".into(), Amf0Value::String("NetConnection.Connect.Success".into())),
                                ("description".into(), Amf0Value::String("Connection succeeded.".into())),
                                ("objectEncoding".into(), Amf0Value::Number(0.0)),
                            ]),
                        ]);
                        writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, 0, 0, &resp).await?;
                        stream.flush().await?;
                    }
                    "releaseStream" | "FCPublish" => {
                        // Acknowledge these (some encoders require _result)
                        let resp = amf0::encode_values(&[
                            Amf0Value::String("_result".into()),
                            Amf0Value::Number(tx_id),
                            Amf0Value::Null,
                        ]);
                        writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, 0, 0, &resp).await?;
                        stream.flush().await?;
                    }
                    "createStream" => {
                        publish_stream_id = 1; // We always use stream ID 1
                        let resp = amf0::encode_values(&[
                            Amf0Value::String("_result".into()),
                            Amf0Value::Number(tx_id),
                            Amf0Value::Null,
                            Amf0Value::Number(publish_stream_id as f64),
                        ]);
                        writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, 0, 0, &resp).await?;
                        stream.flush().await?;
                    }
                    "publish" => {
                        // Extract stream key. This is the input's access-control
                        // credential — it is never logged, at any level: a log
                        // line is readable by anyone with journal access and is
                        // shipped off-box, and logging it *before* validating it
                        // would additionally hand every failed guess to the same
                        // reader. Only the decision is recorded.
                        let stream_key = values.get(3).and_then(|v| v.as_str()).unwrap_or("");
                        tracing::info!("RTMP publish requested: app='{app_name}'");

                        // Validate stream key. Constant-time so the comparison
                        // itself doesn't rank a guess by how far it matched
                        // (same treatment as the setup token and tunnel bind
                        // token).
                        if let Some(expected) = expected_stream_key
                            && !bool::from(
                                stream_key.as_bytes().ct_eq(expected.as_bytes()),
                            ) {
                                tracing::warn!("RTMP: rejected publish with wrong stream key");
                                let resp = amf0::encode_values(&[
                                    Amf0Value::String("onStatus".into()),
                                    Amf0Value::Number(0.0),
                                    Amf0Value::Null,
                                    Amf0Value::Object(vec![
                                        ("level".into(), Amf0Value::String("error".into())),
                                        ("code".into(), Amf0Value::String("NetStream.Publish.BadName".into())),
                                        ("description".into(), Amf0Value::String("Bad stream key".into())),
                                    ]),
                                ]);
                                writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, publish_stream_id, 0, &resp).await?;
                                stream.flush().await?;
                                bail!("Stream key mismatch");
                            }

                        // Send onStatus success
                        let resp = amf0::encode_values(&[
                            Amf0Value::String("onStatus".into()),
                            Amf0Value::Number(0.0),
                            Amf0Value::Null,
                            Amf0Value::Object(vec![
                                ("level".into(), Amf0Value::String("status".into())),
                                ("code".into(), Amf0Value::String("NetStream.Publish.Start".into())),
                                ("description".into(), Amf0Value::String("Publishing started".into())),
                            ]),
                        ]);
                        writer.write_message(&mut stream, 3, msg_type::COMMAND_AMF0, publish_stream_id, 0, &resp).await?;
                        stream.flush().await?;

                        is_publishing.store(true, Ordering::Relaxed);
                        tracing::info!("RTMP publish accepted: app='{app_name}'");

                        // Nothing to lift: the reader's command cap stays in
                        // force, and media was never subject to it.

                        // Enter media receive loop
                        receive_media_loop(&mut stream, &mut reader, &media_tx, &cancel).await?;
                        let _ = media_tx.send(RtmpMediaMessage::Disconnected).await;
                        return Ok(());
                    }
                    "FCUnpublish" | "deleteStream" => {
                        tracing::info!("RTMP client sent {cmd}");
                        let _ = media_tx.send(RtmpMediaMessage::Disconnected).await;
                        return Ok(());
                    }
                    other => {
                        tracing::debug!("RTMP ignoring command: {other}");
                    }
                }
            }
            msg_type::WINDOW_ACK_SIZE => {}
            _ => {
                tracing::trace!("RTMP pre-publish: ignoring msg type {}", msg.msg_type);
            }
        }
    }
}

/// Receive media data (audio/video) from the RTMP publisher.
async fn receive_media_loop<S: AsyncRead + AsyncWrite + Unpin + Send>(
    stream: &mut S,
    reader: &mut ChunkReader,
    media_tx: &mpsc::Sender<RtmpMediaMessage>,
    cancel: &CancellationToken,
) -> Result<()> {
    tracing::info!("RTMP: receiving media data");

    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            r = tokio::time::timeout(RTMP_MEDIA_IDLE_TIMEOUT, reader.read_message(stream)) => {
                match r {
                    Err(_) => {
                        tracing::info!(
                            "RTMP publisher idle for {RTMP_MEDIA_IDLE_TIMEOUT:?} — closing connection"
                        );
                        return Ok(());
                    }
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        tracing::debug!("RTMP read error: {e}");
                        return Ok(()); // Publisher disconnected
                    }
                }
            }
        };

        match msg.msg_type {
            msg_type::VIDEO => {
                let _ = media_tx.send(RtmpMediaMessage::Video {
                    data: Bytes::from(msg.payload),
                    timestamp_ms: msg.timestamp,
                }).await;
            }
            msg_type::AUDIO => {
                let _ = media_tx.send(RtmpMediaMessage::Audio {
                    data: Bytes::from(msg.payload),
                    timestamp_ms: msg.timestamp,
                }).await;
            }
            msg_type::DATA_AMF0 => {
                let _ = media_tx.send(RtmpMediaMessage::Metadata).await;
            }
            msg_type::SET_CHUNK_SIZE => {
                if msg.payload.len() >= 4 {
                    let new_size = u32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
                    // ChunkReader handles this automatically via read_message()
                    let _ = new_size;
                }
            }
            msg_type::COMMAND_AMF0 => {
                if let Ok(values) = amf0::decode_all(&msg.payload) {
                    let cmd = values.first().and_then(|v| v.as_str()).unwrap_or("");
                    match cmd {
                        "FCUnpublish" | "deleteStream" | "closeStream" => {
                            tracing::info!("RTMP publisher sent {cmd}");
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Perform the RTMP handshake as a server (S0/S1/S2).
async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin + Send>(stream: &mut S) -> Result<()> {
    use tokio::io::AsyncReadExt;

    // Read C0 (1 byte: version)
    let mut c0 = [0u8; 1];
    stream.read_exact(&mut c0).await.context("failed to read C0")?;
    if c0[0] != 3 {
        bail!("Unsupported RTMP version: {}", c0[0]);
    }

    // Read C1 (1536 bytes)
    let mut c1 = vec![0u8; 1536];
    stream.read_exact(&mut c1).await.context("failed to read C1")?;

    // Send S0 (version 3)
    stream.write_all(&[3]).await?;

    // Send S1 (1536 bytes: timestamp + zero + random)
    let mut s1 = vec![0u8; 1536];
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    s1[0..4].copy_from_slice(&ts.to_be_bytes());
    // Fill random data
    for (i, byte) in s1[8..].iter_mut().enumerate() {
        *byte = (i * 37 + 73) as u8;
    }
    stream.write_all(&s1).await?;

    // Send S2 (echo of C1)
    stream.write_all(&c1).await?;
    stream.flush().await?;

    // Read C2 (1536 bytes: echo of S1, we just consume it)
    let mut c2 = vec![0u8; 1536];
    stream.read_exact(&mut c2).await.context("failed to read C2")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::chunk::test_chunk_message;
    use tokio::io::{AsyncReadExt, DuplexStream};

    /// Drive the client half of the fixed RTMP handshake — the whole of the
    /// "authentication" an attacker has to clear before the pre-publish
    /// command phase: no credential is involved.
    async fn client_handshake(c: &mut DuplexStream) {
        c.write_all(&[3u8]).await.unwrap(); // C0
        c.write_all(&[0u8; 1536]).await.unwrap(); // C1
        let mut s0_s1_s2 = vec![0u8; 1 + 1536 + 1536];
        c.read_exact(&mut s0_s1_s2).await.unwrap();
        c.write_all(&[0u8; 1536]).await.unwrap(); // C2
    }

    fn spawn_server(
        server: DuplexStream,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        mpsc::Receiver<RtmpMediaMessage>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let addr: std::net::SocketAddr = "127.0.0.1:1935".parse().unwrap();
        let handle = tokio::spawn(handle_rtmp_client(
            server,
            addr,
            "live",
            Some("s3cret-stream-key"),
            tx,
            Arc::new(AtomicBool::new(false)),
            CancellationToken::new(),
        ));
        (handle, rx)
    }

    #[tokio::test]
    async fn unauthenticated_peer_cannot_declare_a_16_mib_command() {
        // The reported attack, end to end: clear the credential-free
        // handshake, then declare a COMMAND_AMF0 message of 0xFFFFFF bytes.
        // The connection must die on the header, before the body (and the
        // AMF0 values it would have expanded into) is ever buffered.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (handle, _rx) = spawn_server(server);

        client_handshake(&mut client).await;
        let mut hdr = vec![0x03u8]; // fmt 0, cs_id 3
        hdr.extend_from_slice(&[0, 0, 0]); // timestamp
        hdr.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // message_length
        hdr.push(msg_type::COMMAND_AMF0);
        hdr.extend_from_slice(&0u32.to_le_bytes());
        client.write_all(&hdr).await.unwrap();
        // Close only the client→server direction: a server that accepts the
        // length then fails on EOF instead of parking on a body that never
        // arrives, while its own control-message writes still succeed.
        client.shutdown().await.unwrap();

        let err = handle
            .await
            .unwrap()
            .expect_err("an oversized pre-publish message must drop the connection");
        drop(client);
        assert!(
            format!("{err:#}").contains("exceeds the"),
            "expected the pre-publish cap to be installed on the reader, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn unauthenticated_peer_cannot_flood_amf0_values() {
        // The other half of the primitive: stay well inside the message-length
        // cap, but fill it with `00 00 05` — one AMF0 object property per 3
        // bytes. Depth stays at 1, so only the value budget can catch it.
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let (handle, _rx) = spawn_server(server);

        client_handshake(&mut client).await;
        let mut body = vec![0x02, 0x00, 0x07]; // AMF0 string, len 7
        body.extend_from_slice(b"connect");
        body.push(0x03); // object marker
        for _ in 0..60_000 {
            body.extend_from_slice(&[0x00, 0x00, 0x05]); // empty key, Null value
        }
        assert!(body.len() < MAX_PREPUBLISH_MSG_LEN, "must be inside the length cap");
        let bytes = test_chunk_message(3, msg_type::COMMAND_AMF0, &body, 128);
        client.write_all(&bytes).await.unwrap();
        client.shutdown().await.unwrap();

        let err = handle
            .await
            .unwrap()
            .expect_err("an AMF0 value flood must drop the connection");
        drop(client);
        let text = format!("{err:#}");
        assert!(
            text.contains("decoded value count"),
            "expected the AMF0 value budget to fire, got: {text}"
        );
    }

    #[tokio::test]
    async fn a_realistic_connect_is_still_answered() {
        // Guard against over-tightening: a `connect` an order of magnitude
        // larger than anything OBS / ffmpeg / Wirecast sends must still be
        // decoded and answered with _result / Connect.Success.
        let (mut client, server) = tokio::io::duplex(256 * 1024);
        let (handle, _rx) = spawn_server(server);

        client_handshake(&mut client).await;

        let mut props = vec![
            ("app".to_string(), Amf0Value::String("live".into())),
            ("flashVer".to_string(), Amf0Value::String("FMLE/3.0 (compatible; FMSc/1.0)".into())),
            ("tcUrl".to_string(), Amf0Value::String("rtmp://edge.example.com:1935/live".into())),
            ("fpad".to_string(), Amf0Value::Boolean(false)),
            ("capabilities".to_string(), Amf0Value::Number(239.0)),
            ("audioCodecs".to_string(), Amf0Value::Number(3575.0)),
            ("videoCodecs".to_string(), Amf0Value::Number(252.0)),
            ("videoFunction".to_string(), Amf0Value::Number(1.0)),
        ];
        // Pad well past any real connect payload (~200-600 bytes).
        for i in 0..200 {
            props.push((format!("pad{i}"), Amf0Value::String("x".repeat(24))));
        }
        let payload = amf0::encode_values(&[
            Amf0Value::String("connect".into()),
            Amf0Value::Number(1.0),
            Amf0Value::Object(props),
        ]);
        assert!(payload.len() > 6_000, "payload should be genuinely large: {}", payload.len());
        client
            .write_all(&test_chunk_message(3, msg_type::COMMAND_AMF0, &payload, 128))
            .await
            .unwrap();

        // Read past the server's control messages to the connect response.
        let mut reader = ChunkReader::new();
        let resp = loop {
            let msg = reader.read_message(&mut client).await.unwrap();
            if msg.msg_type == msg_type::COMMAND_AMF0 {
                break msg;
            }
        };
        let values = amf0::decode_all(&resp.payload).unwrap();
        assert_eq!(values.first().and_then(|v| v.as_str()), Some("_result"));
        assert_eq!(
            values
                .get(3)
                .and_then(|v| v.get_property("code"))
                .and_then(|v| v.as_str()),
            Some("NetConnection.Connect.Success")
        );

        drop(client);
        let _ = handle.await;
    }

    /// Send a `publish` command carrying `key` and return the first
    /// COMMAND_AMF0 the server answers with.
    async fn publish_with_key(client: &mut DuplexStream, key: &str) -> Vec<Amf0Value> {
        let payload = amf0::encode_values(&[
            Amf0Value::String("publish".into()),
            Amf0Value::Number(0.0),
            Amf0Value::Null,
            Amf0Value::String(key.into()),
            Amf0Value::String("live".into()),
        ]);
        client
            .write_all(&test_chunk_message(3, msg_type::COMMAND_AMF0, &payload, 128))
            .await
            .unwrap();
        let mut reader = ChunkReader::new();
        loop {
            let msg = reader.read_message(client).await.unwrap();
            if msg.msg_type == msg_type::COMMAND_AMF0 {
                return amf0::decode_all(&msg.payload).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn publish_with_the_configured_stream_key_is_accepted() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (handle, _rx) = spawn_server(server);
        client_handshake(&mut client).await;

        let values = publish_with_key(&mut client, "s3cret-stream-key").await;
        assert_eq!(values.first().and_then(|v| v.as_str()), Some("onStatus"));
        assert_eq!(
            values.get(3).and_then(|v| v.get_property("code")).and_then(|v| v.as_str()),
            Some("NetStream.Publish.Start"),
        );
        drop(client);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn publish_with_a_wrong_stream_key_is_rejected() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (handle, _rx) = spawn_server(server);
        client_handshake(&mut client).await;

        // Same length as the real key, differing in the last byte: a
        // length-only or prefix-only comparison would accept this.
        let values = publish_with_key(&mut client, "s3cret-stream-keZ").await;
        assert_eq!(
            values.get(3).and_then(|v| v.get_property("code")).and_then(|v| v.as_str()),
            Some("NetStream.Publish.BadName"),
        );
        let err = handle.await.unwrap().expect_err("a wrong key must drop the connection");
        assert!(format!("{err:#}").contains("Stream key mismatch"), "got: {err:#}");
        drop(client);
    }

    #[tokio::test]
    async fn publish_with_an_empty_key_is_rejected_when_one_is_configured() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (handle, _rx) = spawn_server(server);
        client_handshake(&mut client).await;

        let values = publish_with_key(&mut client, "").await;
        assert_eq!(
            values.get(3).and_then(|v| v.get_property("code")).and_then(|v| v.as_str()),
            Some("NetStream.Publish.BadName"),
        );
        let _ = handle.await;
        drop(client);
    }

    #[tokio::test]
    async fn a_large_media_message_is_accepted_after_publish() {
        // Regression guard on the other side of the command cap: an accepted
        // publisher's keyframe is far past MAX_PREPUBLISH_MSG_LEN and must
        // reach the media channel intact. Passes with or without the
        // type-scoping — it is here so a future tightening cannot silently
        // start clipping contribution media.
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let (handle, mut rx) = spawn_server(server);
        client_handshake(&mut client).await;

        let values = publish_with_key(&mut client, "s3cret-stream-key").await;
        assert_eq!(
            values.get(3).and_then(|v| v.get_property("code")).and_then(|v| v.as_str()),
            Some("NetStream.Publish.Start"),
        );

        let payload = vec![0x17u8; 300 * 1024];
        client
            .write_all(&test_chunk_message(4, msg_type::VIDEO, &payload, 128))
            .await
            .unwrap();

        match rx.recv().await.expect("a 300 KiB keyframe must be delivered") {
            RtmpMediaMessage::Video { data, .. } => assert_eq!(data.len(), payload.len()),
            other => panic!("expected Video, got {other:?}"),
        }
        drop(client);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn an_oversized_command_is_still_refused_after_publish() {
        // The half a phase-scoped cap got wrong. Post-`publish`, declare a
        // 1 MiB COMMAND_AMF0 — legal per the 24-bit length field, far past the
        // command cap. The reader must refuse it on the header, which ends the
        // media loop and lands `Disconnected` on the channel. With the cap
        // lifted at publish the reader instead parks on a 1 MiB body that never
        // arrives and nothing is ever delivered, so the timeout is the
        // assertion.
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let (handle, mut rx) = spawn_server(server);
        client_handshake(&mut client).await;

        let values = publish_with_key(&mut client, "s3cret-stream-key").await;
        assert_eq!(
            values.get(3).and_then(|v| v.get_property("code")).and_then(|v| v.as_str()),
            Some("NetStream.Publish.Start"),
        );

        let len: u32 = 1024 * 1024;
        let mut hdr = vec![0x03u8]; // fmt 0, cs_id 3
        hdr.extend_from_slice(&[0, 0, 0]); // timestamp
        hdr.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        hdr.push(msg_type::COMMAND_AMF0);
        hdr.extend_from_slice(&0u32.to_le_bytes());
        client.write_all(&hdr).await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the command cap must still be in force after publish");
        assert!(
            matches!(got, Some(RtmpMediaMessage::Disconnected)),
            "expected the connection to end on the oversized command, got {got:?}"
        );
        drop(client);
        let _ = handle.await;
    }

    /// Collects formatted `tracing` output into a shared buffer so a test can
    /// assert on what a connection did — and did not — write to the journal.
    #[derive(Clone)]
    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture buffer poisoned").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run one full handshake → `publish` exchange against a freshly spawned
    /// server and return the status code it answered with.
    async fn drive_publish(key: &str) -> Option<String> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (handle, _rx) = spawn_server(server);
        client_handshake(&mut client).await;
        let values = publish_with_key(&mut client, key).await;
        let code = values
            .get(3)
            .and_then(|v| v.get_property("code"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        drop(client);
        let _ = handle.await;
        code
    }

    #[tokio::test]
    async fn the_stream_key_never_reaches_the_log_at_any_level() {
        // Pins the security property itself — the ABSENCE of the credential
        // from captured output — rather than the wording of any log line, so it
        // survives a rewrite of the two publish log statements and catches the
        // regression that reintroduces this finding class (a `{values:?}` debug
        // line while troubleshooting: `values[3]` IS the key). A capturing
        // `tracing_subscriber` is used rather than a source-text check because
        // `tracing-subscriber` is already a dependency of this crate and a
        // runtime assertion cannot be defeated by an indirection the grep
        // wouldn't see. Precedent for this shape: `output_rtmp.rs`'s
        // `rtmp_output_event_scoped_and_never_leaks_stream_key`.
        //
        // TRACE level so a `debug!`/`trace!` added later is caught too. The
        // subscriber is a thread-local default and `#[tokio::test]` runs a
        // current-thread runtime, so the spawned connection task is polled on
        // this very thread and its events land in the buffer.
        //
        // The exchange runs TWICE, and only the second pass is measured. That
        // is not belt-and-braces, it is what makes the test deterministic:
        // `tracing` caches each callsite's interest process-globally the first
        // time that callsite is hit, and a callsite first hit by another test
        // running in parallel — with no subscriber installed — can cache
        // "never" and stay silent for the rest of the process. Pass one forces
        // every callsite on this path into the registry; re-installing the
        // subscriber then rebuilds every cached interest against it (documented
        // behaviour of `set_default`), and a registered callsite is never
        // re-registered, so nothing can race it back to "never". Without this,
        // the liveness control below fails roughly one run in twenty-five.
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = || {
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(CaptureWriter(captured.clone()))
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .finish(),
            )
        };

        let warm = subscriber();
        // Accepted publish, then a rejected one — a wrong guess must not be
        // logged either, or the journal becomes a dictionary of everything
        // anyone has ever tried.
        assert_eq!(
            drive_publish("s3cret-stream-key").await.as_deref(),
            Some("NetStream.Publish.Start")
        );
        assert_eq!(
            drive_publish("s3cret-stream-keZ").await.as_deref(),
            Some("NetStream.Publish.BadName")
        );
        drop(warm);

        let _guard = subscriber();
        captured.lock().expect("capture buffer poisoned").clear();
        assert_eq!(
            drive_publish("s3cret-stream-key").await.as_deref(),
            Some("NetStream.Publish.Start")
        );
        assert_eq!(
            drive_publish("s3cret-stream-keZ").await.as_deref(),
            Some("NetStream.Publish.BadName")
        );

        let text = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
        // Liveness control: an assertion about absence is worthless if nothing
        // was captured. Deliberately not a check for specific wording.
        assert!(
            !text.trim().is_empty(),
            "capture produced no output at all — the absence assertions below would be vacuous"
        );
        assert!(
            !text.contains("s3cret-stream-key"),
            "the configured stream key reached the log:\n{text}"
        );
        assert!(
            !text.contains("s3cret-stream-keZ"),
            "a rejected stream key guess reached the log:\n{text}"
        );
    }
}
