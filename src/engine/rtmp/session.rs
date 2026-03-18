//! RTMP publish session — high-level state machine.
//!
//! Connects to an RTMP server (plain TCP or TLS), performs the handshake,
//! issues the `connect`, `releaseStream`, `FCPublish`, `createStream`, and
//! `publish` commands, then provides methods to send video/audio tags.
//!
//! ## Connection lifecycle
//!
//! 1. TCP connect (optionally wrapped in TLS)
//! 2. RTMP handshake (C0/C1/C2)
//! 3. Set Chunk Size (client -> server, 4096 bytes)
//! 4. `connect` command with app name
//! 5. Wait for `_result` (Window Ack Size, Set Peer Bandwidth may arrive first)
//! 6. `releaseStream` + `FCPublish` (non-blocking, some servers require these)
//! 7. `createStream` command, wait for `_result` with stream ID
//! 8. `publish` command on the new stream
//! 9. Stream audio/video tags as RTMP messages (type 8/9)
//! 10. `FCUnpublish` + `deleteStream` on close
//!
//! ## URL format
//!
//! `rtmp://host[:port]/app[/instance]`
//!
//! The stream key is passed separately and sent in the `publish` command.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::amf0::{self, Amf0Value};
use super::chunk::{ChunkReader, ChunkWriter, msg_type, DESIRED_CHUNK_SIZE};
use super::handshake;

/// Default RTMP port for plain TCP.
const DEFAULT_RTMP_PORT: u16 = 1935;

/// Chunk stream IDs.
const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;
const CSID_AUDIO: u32 = 4;
const CSID_VIDEO: u32 = 6;

/// Message stream ID 0 is used for control/command before createStream.
const CONTROL_STREAM_ID: u32 = 0;

/// An active RTMP publish session.
///
/// Wraps the TCP (or TLS) stream and provides methods to send FLV audio/video
/// tags as RTMP messages. Call [`RtmpSession::connect`] to establish.
pub struct RtmpSession {
    /// The transport stream, boxed for TLS/plain polymorphism.
    stream: Box<dyn AsyncReadWrite>,
    writer: ChunkWriter,
    reader: ChunkReader,
    /// The server-assigned message stream ID from `createStream`.
    publish_stream_id: u32,
    /// Next transaction ID for AMF commands.
    next_tx_id: f64,
}

/// Trait alias for a bidirectional async stream (tokio's AsyncRead + AsyncWrite + Unpin + Send).
trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

impl RtmpSession {
    /// Connect to an RTMP server, perform handshake, and start publishing.
    ///
    /// # Arguments
    /// - `url`: RTMP URL in the form `rtmp://host[:port]/app[/instance]`
    /// - `stream_key`: the stream key (sent in the `publish` command)
    /// - `use_tls`: if true, wraps the connection in TLS (requires `tls` feature)
    pub async fn connect(url: &str, stream_key: &str, use_tls: bool) -> Result<Self> {
        let parsed = parse_rtmp_url(url)?;
        tracing::info!(
            "RTMP connecting to {}:{} app='{}' stream_key='{}'",
            parsed.host,
            parsed.port,
            parsed.app,
            stream_key
        );

        // -- TCP connect --
        let addr = format!("{}:{}", parsed.host, parsed.port);
        let tcp = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("failed to connect to {addr}"))?;
        tcp.set_nodelay(true)?;

        let mut stream: Box<dyn AsyncReadWrite> = if use_tls {
            #[cfg(feature = "tls")]
            {
                use std::sync::Arc;
                use tokio_rustls::TlsConnector;
                use tokio_rustls::rustls::{ClientConfig, RootCertStore};
                use tokio_rustls::rustls::pki_types::ServerName;

                let mut root_store = RootCertStore::empty();
                // In a real deployment, load platform CA certs.
                // For now, use an empty store (you'd add webpki_roots here).
                let config = ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth();
                let connector = TlsConnector::from(Arc::new(config));
                let domain = ServerName::try_from(parsed.host.as_str())
                    .map_err(|_| anyhow::anyhow!("invalid DNS name: {}", parsed.host))?
                    .to_owned();
                let tls_stream = connector.connect(domain, tcp).await
                    .context("TLS handshake failed")?;
                Box::new(tls_stream)
            }
            #[cfg(not(feature = "tls"))]
            {
                bail!("TLS support requires the 'tls' feature (tokio-rustls). \
                       Recompile with --features tls, or use rtmp:// instead of rtmps://");
            }
        } else {
            Box::new(tcp)
        };

        // -- RTMP handshake --
        handshake::perform_handshake(&mut stream).await?;

        let mut writer = ChunkWriter::new();
        let reader = ChunkReader::new();

        // -- Set chunk size (client -> server) --
        writer.write_set_chunk_size(&mut stream, DESIRED_CHUNK_SIZE).await?;
        stream.flush().await?;

        // -- connect command --
        let mut session = Self {
            stream,
            writer,
            reader,
            publish_stream_id: 0,
            next_tx_id: 1.0,
        };

        let tc_url = if parsed.port == DEFAULT_RTMP_PORT {
            format!("rtmp://{}/{}", parsed.host, parsed.app)
        } else {
            format!("rtmp://{}:{}/{}", parsed.host, parsed.port, parsed.app)
        };

        session.send_connect_command(&parsed.app, &tc_url).await?;
        session.wait_for_connect_result().await?;

        // -- releaseStream, FCPublish (best-effort, some servers need them) --
        session.send_release_stream(stream_key).await?;
        session.send_fc_publish(stream_key).await?;

        // -- createStream --
        let stream_id = session.send_create_stream().await?;
        session.publish_stream_id = stream_id;

        // -- publish --
        session.send_publish(stream_key).await?;

        tracing::info!("RTMP publish session established (stream_id={stream_id})");
        Ok(session)
    }

    /// Send a video FLV tag body as an RTMP video message.
    ///
    /// `data` should be a complete FLV video tag body (as produced by
    /// [`flv::build_avc_sequence_header`] or [`flv::build_avc_nalu_tag`]).
    pub async fn send_video_tag(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.writer
            .write_message(
                &mut self.stream,
                CSID_VIDEO,
                msg_type::VIDEO,
                self.publish_stream_id,
                timestamp_ms,
                data,
            )
            .await
            .context("failed to send video tag")?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Send an audio FLV tag body as an RTMP audio message.
    ///
    /// `data` should be a complete FLV audio tag body (as produced by
    /// [`flv::build_aac_sequence_header`] or [`flv::build_aac_raw_tag`]).
    pub async fn send_audio_tag(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.writer
            .write_message(
                &mut self.stream,
                CSID_AUDIO,
                msg_type::AUDIO,
                self.publish_stream_id,
                timestamp_ms,
                data,
            )
            .await
            .context("failed to send audio tag")?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Send an `@setDataFrame` + `onMetaData` data message with stream metadata.
    ///
    /// This is optional but recommended. Many players use it for display.
    pub async fn send_metadata(
        &mut self,
        width: f64,
        height: f64,
        video_bitrate_kbps: f64,
        audio_bitrate_kbps: f64,
        framerate: f64,
        audio_sample_rate: f64,
        stereo: bool,
    ) -> Result<()> {
        let metadata = amf0::encode_values(&[
            Amf0Value::String("@setDataFrame".into()),
            Amf0Value::String("onMetaData".into()),
            Amf0Value::Object(vec![
                ("width".into(), Amf0Value::Number(width)),
                ("height".into(), Amf0Value::Number(height)),
                ("videodatarate".into(), Amf0Value::Number(video_bitrate_kbps)),
                ("audiodatarate".into(), Amf0Value::Number(audio_bitrate_kbps)),
                ("framerate".into(), Amf0Value::Number(framerate)),
                ("videocodecid".into(), Amf0Value::Number(7.0)), // AVC
                ("audiocodecid".into(), Amf0Value::Number(10.0)), // AAC
                ("audiosamplerate".into(), Amf0Value::Number(audio_sample_rate)),
                ("stereo".into(), Amf0Value::Boolean(stereo)),
            ]),
        ]);

        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::DATA_AMF0,
                self.publish_stream_id,
                0,
                &metadata,
            )
            .await
            .context("failed to send metadata")?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Gracefully close the publish session.
    ///
    /// Sends `FCUnpublish` and `deleteStream`, then closes the TCP connection.
    pub async fn close(mut self) -> Result<()> {
        // FCUnpublish (best-effort).
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("FCUnpublish".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
        ]);
        let _ = self
            .writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await;

        // deleteStream.
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("deleteStream".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
            Amf0Value::Number(self.publish_stream_id as f64),
        ]);
        let _ = self
            .writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await;

        let _ = self.stream.flush().await;
        let _ = self.stream.shutdown().await;
        tracing::info!("RTMP session closed");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal command helpers
    // -----------------------------------------------------------------------

    fn next_tx(&mut self) -> f64 {
        let id = self.next_tx_id;
        self.next_tx_id += 1.0;
        id
    }

    /// Send the `connect` AMF command.
    async fn send_connect_command(&mut self, app: &str, tc_url: &str) -> Result<()> {
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("connect".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Object(vec![
                ("app".into(), Amf0Value::String(app.to_string())),
                ("type".into(), Amf0Value::String("nonprivate".into())),
                ("flashVer".into(), Amf0Value::String("FMLE/3.0 (compatible; bilbycast-edge)".into())),
                ("tcUrl".into(), Amf0Value::String(tc_url.to_string())),
            ]),
        ]);

        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await
            .context("failed to send connect command")?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Wait for the `_result` response to the `connect` command.
    ///
    /// Also handles protocol control messages (Window Ack Size, Set Peer Bandwidth,
    /// Set Chunk Size) that the server typically sends before the _result.
    async fn wait_for_connect_result(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

        loop {
            let msg = tokio::time::timeout_at(deadline, self.reader.read_message(&mut self.stream))
                .await
                .context("timeout waiting for connect _result")?
                .context("failed reading message during connect")?;

            match msg.msg_type {
                msg_type::WINDOW_ACK_SIZE => {
                    if msg.payload.len() >= 4 {
                        let ack_size = u32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
                        tracing::debug!("server Window Ack Size = {ack_size}");
                    }
                }
                msg_type::SET_PEER_BANDWIDTH => {
                    tracing::debug!("server Set Peer Bandwidth received");
                    // Respond with Window Acknowledgement Size (same value).
                    if msg.payload.len() >= 4 {
                        let ack_size_bytes = msg.payload[0..4].to_vec();
                        self.writer
                            .write_message(
                                &mut self.stream,
                                CSID_CONTROL,
                                msg_type::WINDOW_ACK_SIZE,
                                CONTROL_STREAM_ID,
                                0,
                                &ack_size_bytes,
                            )
                            .await?;
                        self.stream.flush().await?;
                    }
                }
                msg_type::SET_CHUNK_SIZE => {
                    // ChunkReader handles this internally, but log it.
                    tracing::debug!("server Set Chunk Size received");
                }
                msg_type::COMMAND_AMF0 => {
                    let values = amf0::decode_all(&msg.payload)
                        .context("failed to decode connect response AMF0")?;
                    if let Some(cmd_name) = values.first().and_then(|v| v.as_str()) {
                        match cmd_name {
                            "_result" => {
                                tracing::debug!("connect _result received");
                                return Ok(());
                            }
                            "_error" => {
                                let desc = values
                                    .get(3)
                                    .and_then(|v| v.get_property("description"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown error");
                                bail!("RTMP connect rejected: {desc}");
                            }
                            other => {
                                tracing::debug!("ignoring command '{other}' during connect");
                            }
                        }
                    }
                }
                _ => {
                    tracing::trace!(
                        "ignoring message type {} during connect handshake",
                        msg.msg_type
                    );
                }
            }
        }
    }

    /// Send `releaseStream` (transaction, best-effort).
    async fn send_release_stream(&mut self, stream_key: &str) -> Result<()> {
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("releaseStream".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
            Amf0Value::String(stream_key.to_string()),
        ]);
        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Send `FCPublish` (transaction, best-effort).
    async fn send_fc_publish(&mut self, stream_key: &str) -> Result<()> {
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("FCPublish".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
            Amf0Value::String(stream_key.to_string()),
        ]);
        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Send `createStream` and wait for the `_result` containing the stream ID.
    async fn send_create_stream(&mut self) -> Result<u32> {
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("createStream".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
        ]);
        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                CONTROL_STREAM_ID,
                0,
                &payload,
            )
            .await?;
        self.stream.flush().await?;

        // Wait for _result with stream ID.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.reader.read_message(&mut self.stream))
                .await
                .context("timeout waiting for createStream _result")?
                .context("failed reading message during createStream")?;

            if msg.msg_type == msg_type::COMMAND_AMF0 {
                let values = amf0::decode_all(&msg.payload)?;
                if let Some(cmd) = values.first().and_then(|v| v.as_str()) {
                    if cmd == "_result" {
                        // The stream ID is the 4th value (index 3).
                        let stream_id = values
                            .get(3)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(1.0) as u32;
                        tracing::debug!("createStream _result: stream_id={stream_id}");
                        return Ok(stream_id);
                    } else if cmd == "_error" {
                        bail!("createStream failed");
                    }
                }
            }
            // Ignore other messages (onBWDone, _result for releaseStream, etc.).
        }
    }

    /// Send the `publish` command.
    async fn send_publish(&mut self, stream_key: &str) -> Result<()> {
        let tx_id = self.next_tx();
        let payload = amf0::encode_values(&[
            Amf0Value::String("publish".into()),
            Amf0Value::Number(tx_id),
            Amf0Value::Null,
            Amf0Value::String(stream_key.to_string()),
            Amf0Value::String("live".into()), // publish type
        ]);

        self.writer
            .write_message(
                &mut self.stream,
                CSID_COMMAND,
                msg_type::COMMAND_AMF0,
                self.publish_stream_id,
                0,
                &payload,
            )
            .await
            .context("failed to send publish command")?;
        self.stream.flush().await?;

        // Wait for onStatus confirming publish. Some servers send it, some don't.
        // We use a short timeout and consider success if we time out (server may not reply).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, self.reader.read_message(&mut self.stream)).await;

            match result {
                Ok(Ok(msg)) => {
                    if msg.msg_type == msg_type::COMMAND_AMF0 {
                        let values = amf0::decode_all(&msg.payload).unwrap_or_default();
                        if let Some(cmd) = values.first().and_then(|v| v.as_str()) {
                            if cmd == "onStatus" {
                                // Check if it's an error.
                                if let Some(info) = values.get(3) {
                                    let level = info
                                        .get_property("level")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let code = info
                                        .get_property("code")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    tracing::debug!("publish onStatus: level={level} code={code}");
                                    if level == "error" {
                                        bail!("publish rejected: {code}");
                                    }
                                }
                                return Ok(());
                            } else if cmd == "_error" {
                                bail!("publish command returned _error");
                            }
                        }
                    }
                    // Ignore other messages (e.g., onBWDone).
                }
                Ok(Err(e)) => {
                    return Err(e).context("error while waiting for publish response");
                }
                Err(_) => {
                    // Timeout — assume publish succeeded (common with some RTMP servers).
                    tracing::debug!("no onStatus response for publish (timeout), assuming success");
                    return Ok(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reconnecting wrapper
// ---------------------------------------------------------------------------

/// An RTMP session wrapper that automatically reconnects on failure.
///
/// Useful for long-running publish streams where transient network issues
/// should not be fatal.
pub struct ReconnectingRtmpSession {
    url: String,
    stream_key: String,
    use_tls: bool,
    session: Option<RtmpSession>,
    max_retries: u32,
    retry_delay: Duration,
}

impl ReconnectingRtmpSession {
    /// Create a new reconnecting session (does not connect immediately).
    pub fn new(url: &str, stream_key: &str, use_tls: bool) -> Self {
        Self {
            url: url.to_string(),
            stream_key: stream_key.to_string(),
            use_tls,
            session: None,
            max_retries: 5,
            retry_delay: Duration::from_secs(2),
        }
    }

    /// Set the maximum number of consecutive reconnection attempts.
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Set the base delay between reconnection attempts (doubles each retry).
    pub fn with_retry_delay(mut self, d: Duration) -> Self {
        self.retry_delay = d;
        self
    }

    /// Ensure we have an active session, reconnecting if necessary.
    pub async fn ensure_connected(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }

        let mut delay = self.retry_delay;
        for attempt in 1..=self.max_retries {
            tracing::info!("RTMP connect attempt {}/{}", attempt, self.max_retries);
            match RtmpSession::connect(&self.url, &self.stream_key, self.use_tls).await {
                Ok(s) => {
                    self.session = Some(s);
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("RTMP connect attempt {attempt} failed: {e:#}");
                    if attempt < self.max_retries {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
        bail!(
            "RTMP: failed to connect after {} attempts",
            self.max_retries
        );
    }

    /// Send a video tag, reconnecting if the send fails.
    pub async fn send_video_tag(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.ensure_connected().await?;
        if let Some(ref mut session) = self.session {
            match session.send_video_tag(data, timestamp_ms).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("RTMP send_video_tag failed: {e:#}, will reconnect");
                    self.session = None;
                }
            }
        }
        // Retry once after reconnect.
        self.ensure_connected().await?;
        if let Some(ref mut session) = self.session {
            session.send_video_tag(data, timestamp_ms).await
        } else {
            bail!("RTMP session not available after reconnect");
        }
    }

    /// Send an audio tag, reconnecting if the send fails.
    pub async fn send_audio_tag(&mut self, data: &[u8], timestamp_ms: u32) -> Result<()> {
        self.ensure_connected().await?;
        if let Some(ref mut session) = self.session {
            match session.send_audio_tag(data, timestamp_ms).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("RTMP send_audio_tag failed: {e:#}, will reconnect");
                    self.session = None;
                }
            }
        }
        self.ensure_connected().await?;
        if let Some(ref mut session) = self.session {
            session.send_audio_tag(data, timestamp_ms).await
        } else {
            bail!("RTMP session not available after reconnect");
        }
    }

    /// Gracefully close the session.
    pub async fn close(mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// URL parser
// ---------------------------------------------------------------------------

struct ParsedRtmpUrl {
    host: String,
    port: u16,
    app: String,
}

/// Parse an RTMP URL into host, port, and app name.
///
/// Supports: `rtmp://host/app`, `rtmp://host:port/app`, `rtmp://host/app/instance`
fn parse_rtmp_url(url: &str) -> Result<ParsedRtmpUrl> {
    let url = url.trim();

    // Strip scheme.
    let rest = if let Some(r) = url.strip_prefix("rtmp://") {
        r
    } else if let Some(r) = url.strip_prefix("rtmps://") {
        r
    } else {
        bail!("RTMP URL must start with rtmp:// or rtmps://");
    };

    // Split host[:port] from path.
    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx + 1..]),
        None => bail!("RTMP URL must contain an app path: {url}"),
    };

    // Parse host and port.
    let (host, port) = if let Some(idx) = host_port.rfind(':') {
        let h = &host_port[..idx];
        let p: u16 = host_port[idx + 1..]
            .parse()
            .with_context(|| format!("invalid port in RTMP URL: {url}"))?;
        (h.to_string(), p)
    } else {
        (host_port.to_string(), DEFAULT_RTMP_PORT)
    };

    if host.is_empty() {
        bail!("RTMP URL has empty host: {url}");
    }

    // The app is the first path segment (or first two for app/instance).
    // RTMP convention: everything after the host+port up to the stream key is the "app".
    // Since the stream key is passed separately, the entire path is the app name.
    let app = path.to_string();
    if app.is_empty() {
        bail!("RTMP URL has empty app name: {url}");
    }

    Ok(ParsedRtmpUrl { host, port, app })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_url() {
        let p = parse_rtmp_url("rtmp://live.example.com/live").unwrap();
        assert_eq!(p.host, "live.example.com");
        assert_eq!(p.port, 1935);
        assert_eq!(p.app, "live");
    }

    #[test]
    fn parse_url_with_port() {
        let p = parse_rtmp_url("rtmp://192.168.1.1:1936/app/instance").unwrap();
        assert_eq!(p.host, "192.168.1.1");
        assert_eq!(p.port, 1936);
        assert_eq!(p.app, "app/instance");
    }

    #[test]
    fn parse_rtmps_url() {
        let p = parse_rtmp_url("rtmps://ingest.example.tv/live").unwrap();
        assert_eq!(p.host, "ingest.example.tv");
        assert_eq!(p.port, 1935);
        assert_eq!(p.app, "live");
    }

    #[test]
    fn parse_missing_app_fails() {
        assert!(parse_rtmp_url("rtmp://host").is_err());
    }

    #[test]
    fn parse_bad_scheme_fails() {
        assert!(parse_rtmp_url("http://host/app").is_err());
    }
}
