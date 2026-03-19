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

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut, BufMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::amf0::{self, Amf0Value};
use super::chunk::{ChunkReader, ChunkWriter, msg_type, DESIRED_CHUNK_SIZE};

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
    Metadata {
        data: Bytes,
    },
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
    let listener = TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("RTMP server: failed to bind {}", config.listen_addr))?;

    tracing::info!("RTMP server listening on {}", config.listen_addr);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTMP server shutting down");
                return Ok(());
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        tracing::info!("RTMP client connected from {addr}");
                        stream.set_nodelay(true).ok();

                        let media_tx = media_tx.clone();
                        let cancel = cancel.child_token();
                        let expected_app = config.expected_app.clone();
                        let expected_key = config.expected_stream_key.clone();
                        let is_pub = is_publishing.clone();

                        tokio::spawn(async move {
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
    // 1. RTMP handshake (server side)
    server_handshake(&mut stream).await?;
    tracing::debug!("RTMP handshake complete with {addr}");

    let mut writer = ChunkWriter::new();
    let mut reader = ChunkReader::new();

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
            r = reader.read_message(&mut stream) => r?,
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
                        // Extract stream key
                        let stream_key = values.get(3).and_then(|v| v.as_str()).unwrap_or("");
                        tracing::info!("RTMP publish: app='{app_name}' key='{stream_key}'");

                        // Validate stream key
                        if let Some(expected) = expected_stream_key {
                            if stream_key != expected {
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
            r = reader.read_message(stream) => {
                match r {
                    Ok(m) => m,
                    Err(e) => {
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
                let _ = media_tx.send(RtmpMediaMessage::Metadata {
                    data: Bytes::from(msg.payload),
                }).await;
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
