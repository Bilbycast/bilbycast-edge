// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP push upload helpers for CMAF.
//!
//! - [`http_put`]: standard whole-body PUT for init segments, finished
//!   segments, and manifest files.
//! - [`ChunkedPutTask`]: LL-CMAF streaming PUT — opens a long-lived
//!   request with `Transfer-Encoding: chunked` and forwards chunks
//!   submitted via a bounded mpsc channel. Backpressure is handled by
//!   `try_send` with drop-on-full so the subscriber task is never
//!   blocked on a slow ingest.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

/// Process-wide HTTP client shared across all CMAF outputs.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client")
    })
}

/// PUT `body` to `url`. Returns Err on non-2xx responses.
pub async fn http_put(
    url: &str,
    body: Vec<u8>,
    content_type: &str,
    auth_token: Option<&str>,
) -> Result<()> {
    let mut req = client()
        .put(url)
        .header("Content-Type", content_type)
        .body(body);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "PUT {url} returned HTTP {} — {}",
            status.as_u16(),
            body.chars().take(500).collect::<String>()
        );
    }
    Ok(())
}

/// A running chunked PUT task. Use [`ChunkedPutHandle::send_chunk`] to
/// push bytes into the body stream; drop / finish the handle to close
/// the stream and wait for the server's final status.
pub struct ChunkedPutHandle {
    /// mpsc sender backing the streaming request body. `None` after
    /// [`finish`] has been called.
    tx: Option<mpsc::Sender<Result<Bytes, std::io::Error>>>,
    /// Task running the `reqwest::put(...).body(stream).send().await`.
    task: JoinHandle<Result<()>>,
    /// Number of in-flight chunks that could not be enqueued (depth
    /// overflowed). Exposed for stats.
    pub dropped_chunks: u64,
    /// Bytes successfully enqueued (not bytes confirmed by the peer).
    pub sent_bytes: u64,
    /// Target URL — kept for diagnostics only.
    #[allow(dead_code)]
    url: String,
}

impl ChunkedPutHandle {
    /// Enqueue a chunk. Returns `Ok(())` on success, `Err(())` if the
    /// channel is full or closed. The caller should treat full as a
    /// backpressure event: drop the chunk, emit a warning, and abort
    /// the current segment.
    pub fn send_chunk(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(());
        };
        let len = bytes.len() as u64;
        match tx.try_send(Ok(Bytes::from(bytes))) {
            Ok(()) => {
                self.sent_bytes += len;
                Ok(())
            }
            Err(_) => {
                self.dropped_chunks += 1;
                Err(())
            }
        }
    }

    /// Close the body stream and await the PUT's final response.
    pub async fn finish(mut self) -> Result<()> {
        // Drop the tx side — this signals the ReceiverStream to close,
        // which lets reqwest finish the request body.
        self.tx = None;
        match self.task.await {
            Ok(r) => r,
            Err(e) => bail!("chunked PUT task panicked: {e}"),
        }
    }

    /// Abort the in-flight request. Used on stall/timeout.
    pub fn abort(mut self) {
        self.tx = None;
        self.task.abort();
    }
}

/// Open a chunked PUT request. Returns immediately; the caller feeds
/// bytes via [`ChunkedPutHandle::send_chunk`] and finalises with
/// [`ChunkedPutHandle::finish`].
pub fn chunked_put(
    url: &str,
    content_type: &str,
    auth_token: Option<&str>,
    channel_depth: usize,
) -> ChunkedPutHandle {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(channel_depth.max(2));
    let stream = ReceiverStream::new(rx);
    let body = reqwest::Body::wrap_stream(stream);

    let mut req = client()
        .put(url)
        .header("Content-Type", content_type)
        // Explicit Transfer-Encoding hint; reqwest would otherwise add
        // it because the body is unsized, but being explicit helps
        // debug with curl -v.
        .header("Transfer-Encoding", "chunked")
        .body(body);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let url_owned = url.to_string();
    let task: JoinHandle<Result<()>> = tokio::spawn(async move {
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "chunked PUT returned HTTP {} — {}",
                status.as_u16(),
                body.chars().take(500).collect::<String>()
            );
        }
        Ok(())
    });

    ChunkedPutHandle {
        tx: Some(tx),
        task,
        dropped_chunks: 0,
        sent_bytes: 0,
        url: url_owned,
    }
}
