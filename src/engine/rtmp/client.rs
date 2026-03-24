// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! RTMP client — connects to an RTMP server and publishes media.
//!
//! Performs the client-side RTMP handshake, `connect`, `createStream`,
//! and `publish` commands. Once publishing, provides methods to send
//! FLV-wrapped H.264 video and AAC audio tags.

use anyhow::{Context, Result, bail};
use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::amf0::{self, Amf0Value};
use super::chunk::{ChunkReader, ChunkWriter, msg_type, DESIRED_CHUNK_SIZE};

/// An RTMP client session connected to a server in publish mode.
pub struct RtmpClient {
    writer: ChunkWriter,
    reader: ChunkReader,
    stream: TcpStream,
    stream_id: u32,
}

impl RtmpClient {
    /// Connect to an RTMP server, perform handshake, and enter publish mode.
    ///
    /// `url` should be the full RTMP URL including app path, e.g.
    /// `rtmp://live.twitch.tv/app/stream_key` or `rtmp://host:1935/live`.
    /// `stream_key` is sent in the `publish` command.
    pub async fn connect(url: &str, stream_key: &str) -> Result<Self> {
        let (host, port, app, tc_url) = parse_rtmp_url(url)?;

        tracing::debug!("RTMP client connecting to {}:{} app='{}'", host, port, app);

        let stream = TcpStream::connect(format!("{host}:{port}"))
            .await
            .with_context(|| format!("RTMP: failed to connect to {host}:{port}"))?;
        stream.set_nodelay(true).ok();

        let mut client = Self {
            writer: ChunkWriter::new(),
            reader: ChunkReader::new(),
            stream,
            stream_id: 0,
        };

        // 1. Handshake
        client_handshake(&mut client.stream).await?;
        tracing::debug!("RTMP handshake complete");

        // 2. Set Chunk Size
        client.writer.write_set_chunk_size(&mut client.stream, DESIRED_CHUNK_SIZE).await?;

        // 3. Send Window Ack Size
        let mut buf = BytesMut::with_capacity(4);
        buf.put_u32(2_500_000);
        client.writer.write_message(&mut client.stream, 2, msg_type::WINDOW_ACK_SIZE, 0, 0, &buf).await?;
        client.stream.flush().await?;

        // 4. Send connect
        let connect_payload = amf0::encode_values(&[
            Amf0Value::String("connect".into()),
            Amf0Value::Number(1.0), // txId
            Amf0Value::Object(vec![
                ("app".into(), Amf0Value::String(app.clone())),
                ("type".into(), Amf0Value::String("nonprivate".into())),
                ("flashVer".into(), Amf0Value::String("FMLE/3.0 (compatible; bilbycast-edge)".into())),
                ("tcUrl".into(), Amf0Value::String(tc_url)),
            ]),
        ]);
        client.writer.write_message(&mut client.stream, 3, msg_type::COMMAND_AMF0, 0, 0, &connect_payload).await?;
        client.stream.flush().await?;

        // 5. Read until _result for connect
        client.wait_for_result(1.0).await
            .context("RTMP: connect rejected by server")?;
        tracing::debug!("RTMP connect accepted");

        // 6. releaseStream
        let payload = amf0::encode_values(&[
            Amf0Value::String("releaseStream".into()),
            Amf0Value::Number(2.0),
            Amf0Value::Null,
            Amf0Value::String(stream_key.into()),
        ]);
        client.writer.write_message(&mut client.stream, 3, msg_type::COMMAND_AMF0, 0, 0, &payload).await?;

        // 7. FCPublish
        let payload = amf0::encode_values(&[
            Amf0Value::String("FCPublish".into()),
            Amf0Value::Number(3.0),
            Amf0Value::Null,
            Amf0Value::String(stream_key.into()),
        ]);
        client.writer.write_message(&mut client.stream, 3, msg_type::COMMAND_AMF0, 0, 0, &payload).await?;

        // 8. createStream
        let payload = amf0::encode_values(&[
            Amf0Value::String("createStream".into()),
            Amf0Value::Number(4.0),
            Amf0Value::Null,
        ]);
        client.writer.write_message(&mut client.stream, 3, msg_type::COMMAND_AMF0, 0, 0, &payload).await?;
        client.stream.flush().await?;

        // 9. Read _result for createStream to get stream_id
        let stream_id = client.wait_for_create_stream_result(4.0).await
            .context("RTMP: createStream failed")?;
        client.stream_id = stream_id;
        tracing::debug!("RTMP createStream: stream_id={}", stream_id);

        // 10. publish
        let payload = amf0::encode_values(&[
            Amf0Value::String("publish".into()),
            Amf0Value::Number(5.0),
            Amf0Value::Null,
            Amf0Value::String(stream_key.into()),
            Amf0Value::String("live".into()),
        ]);
        client.writer.write_message(&mut client.stream, 3, msg_type::COMMAND_AMF0, stream_id, 0, &payload).await?;
        client.stream.flush().await?;

        // 11. Wait for onStatus NetStream.Publish.Start
        client.wait_for_publish_start().await
            .context("RTMP: publish rejected by server")?;
        tracing::info!("RTMP publish started on stream_id={}", stream_id);

        Ok(client)
    }

    /// Send a video tag (FLV format) to the server.
    pub async fn send_video(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.writer
            .write_message(&mut self.stream, 5, msg_type::VIDEO, self.stream_id, timestamp_ms, data)
            .await
    }

    /// Send an audio tag (FLV format) to the server.
    pub async fn send_audio(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.writer
            .write_message(&mut self.stream, 4, msg_type::AUDIO, self.stream_id, timestamp_ms, data)
            .await
    }

    /// Flush the underlying TCP stream.
    pub async fn flush(&mut self) -> Result<()> {
        self.stream.flush().await.context("RTMP flush failed")
    }

    /// Gracefully close the publish session.
    pub async fn close(&mut self) -> Result<()> {
        let payload = amf0::encode_values(&[
            Amf0Value::String("FCUnpublish".into()),
            Amf0Value::Number(6.0),
            Amf0Value::Null,
        ]);
        let _ = self.writer.write_message(&mut self.stream, 3, msg_type::COMMAND_AMF0, self.stream_id, 0, &payload).await;

        let payload = amf0::encode_values(&[
            Amf0Value::String("deleteStream".into()),
            Amf0Value::Number(7.0),
            Amf0Value::Null,
            Amf0Value::Number(self.stream_id as f64),
        ]);
        let _ = self.writer.write_message(&mut self.stream, 3, msg_type::COMMAND_AMF0, 0, 0, &payload).await;
        let _ = self.stream.flush().await;
        let _ = self.stream.shutdown().await;
        Ok(())
    }

    /// Read messages until we get a _result matching the given txId.
    async fn wait_for_result(&mut self, expected_tx_id: f64) -> Result<()> {
        for _ in 0..50 {
            let msg = self.reader.read_message(&mut self.stream).await?;
            if msg.msg_type == msg_type::COMMAND_AMF0 {
                if let Ok(values) = amf0::decode_all(&msg.payload) {
                    let cmd = values.first().and_then(|v| v.as_str()).unwrap_or("");
                    let tx_id = values.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if cmd == "_result" && tx_id == expected_tx_id {
                        return Ok(());
                    }
                    if cmd == "_error" && tx_id == expected_tx_id {
                        bail!("server returned _error for txId={}", expected_tx_id);
                    }
                }
            }
            // Ignore other messages (Window Ack Size, Set Peer Bandwidth, etc.)
        }
        bail!("timed out waiting for _result (txId={})", expected_tx_id);
    }

    /// Read messages until we get a _result for createStream, returning the stream_id.
    async fn wait_for_create_stream_result(&mut self, expected_tx_id: f64) -> Result<u32> {
        for _ in 0..50 {
            let msg = self.reader.read_message(&mut self.stream).await?;
            if msg.msg_type == msg_type::COMMAND_AMF0 {
                if let Ok(values) = amf0::decode_all(&msg.payload) {
                    let cmd = values.first().and_then(|v| v.as_str()).unwrap_or("");
                    let tx_id = values.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if cmd == "_result" && tx_id == expected_tx_id {
                        let sid = values.get(3).and_then(|v| v.as_f64()).unwrap_or(1.0);
                        return Ok(sid as u32);
                    }
                    if cmd == "_error" && tx_id == expected_tx_id {
                        bail!("server returned _error for createStream");
                    }
                }
            }
        }
        bail!("timed out waiting for createStream result");
    }

    /// Wait for onStatus with NetStream.Publish.Start.
    async fn wait_for_publish_start(&mut self) -> Result<()> {
        for _ in 0..50 {
            let msg = self.reader.read_message(&mut self.stream).await?;
            if msg.msg_type == msg_type::COMMAND_AMF0 {
                if let Ok(values) = amf0::decode_all(&msg.payload) {
                    let cmd = values.first().and_then(|v| v.as_str()).unwrap_or("");
                    if cmd == "onStatus" {
                        // Check for success or error
                        if let Some(info) = values.get(3) {
                            let code = info.get_property("code")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if code == "NetStream.Publish.Start" {
                                return Ok(());
                            }
                            if code.contains("Error") || code.contains("Reject") || code.contains("BadName") {
                                bail!("publish rejected: {}", code);
                            }
                        }
                        // Some servers send onStatus without code, treat as success
                        return Ok(());
                    }
                    if cmd == "_error" {
                        bail!("server returned _error during publish");
                    }
                }
            }
        }
        bail!("timed out waiting for publish start");
    }
}

/// Parse an RTMP URL into (host, port, app, tcUrl).
///
/// Supports: `rtmp://host[:port]/app[/extra]`
/// The stream key is NOT extracted from the URL here — it's a separate parameter.
fn parse_rtmp_url(url: &str) -> Result<(String, u16, String, String)> {
    let stripped = url.strip_prefix("rtmp://")
        .or_else(|| url.strip_prefix("rtmps://"))
        .ok_or_else(|| anyhow::anyhow!("RTMP URL must start with rtmp:// or rtmps://"))?;

    // Split host:port from path
    let (host_port, path) = stripped.split_once('/')
        .ok_or_else(|| anyhow::anyhow!("RTMP URL must contain an app path"))?;

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h.to_string(), p.parse::<u16>().context("invalid port in RTMP URL")?)
    } else {
        (host_port.to_string(), 1935)
    };

    // App is the first path segment; anything after is part of the stream key
    // But for tcUrl we use everything up to and including the app
    let app = path.split('/').next().unwrap_or(path).to_string();

    // tcUrl is the URL up to and including the app name
    let tc_url = format!("rtmp://{}:{}/{}", host, port, app);

    Ok((host, port, app, tc_url))
}

/// Perform the client-side RTMP handshake (C0/C1/C2).
async fn client_handshake(stream: &mut TcpStream) -> Result<()> {
    // Send C0 (version 3) + C1 (1536 bytes)
    let mut c0c1 = vec![0u8; 1 + 1536];
    c0c1[0] = 3; // RTMP version
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    c0c1[1..5].copy_from_slice(&ts.to_be_bytes());
    // c0c1[5..9] = zeros (already)
    // Fill random data
    for (i, byte) in c0c1[9..].iter_mut().enumerate() {
        *byte = (i * 41 + 97) as u8;
    }
    stream.write_all(&c0c1).await.context("failed to send C0+C1")?;
    stream.flush().await?;

    // Read S0 + S1 + S2 (1 + 1536 + 1536 bytes)
    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0).await.context("failed to read S0")?;
    if s0[0] != 3 {
        bail!("unexpected RTMP server version: {}", s0[0]);
    }

    let mut s1 = vec![0u8; 1536];
    stream.read_exact(&mut s1).await.context("failed to read S1")?;

    let mut s2 = vec![0u8; 1536];
    stream.read_exact(&mut s2).await.context("failed to read S2")?;

    // Send C2 (echo of S1)
    stream.write_all(&s1).await.context("failed to send C2")?;
    stream.flush().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rtmp_url_basic() {
        let (host, port, app, tc_url) = parse_rtmp_url("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!(host, "live.twitch.tv");
        assert_eq!(port, 1935);
        assert_eq!(app, "app");
        assert_eq!(tc_url, "rtmp://live.twitch.tv:1935/app");
    }

    #[test]
    fn test_parse_rtmp_url_with_port() {
        let (host, port, app, _) = parse_rtmp_url("rtmp://localhost:1936/live").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1936);
        assert_eq!(app, "live");
    }

    #[test]
    fn test_parse_rtmp_url_with_stream_key_in_path() {
        let (host, port, app, tc_url) = parse_rtmp_url("rtmp://syd.contribute.live-video.net/app/stream_key_here").unwrap();
        assert_eq!(host, "syd.contribute.live-video.net");
        assert_eq!(port, 1935);
        assert_eq!(app, "app");
        assert_eq!(tc_url, "rtmp://syd.contribute.live-video.net:1935/app");
    }
}
