use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::SrtOutputConfig;
use crate::srt::connection::{connect_srt_output, connect_srt_redundancy_leg};
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets over an SRT connection. If redundancy is configured,
/// packets are duplicated to both SRT legs via SrtDuplicator.
pub fn spawn_srt_output(
    config: SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            srt_output_redundant_loop(&config, &mut rx, output_stats, cancel).await
        } else {
            srt_output_loop(&config, &mut rx, output_stats, cancel).await
        };
        if let Err(e) = result {
            tracing::error!("SRT output '{}' exited with error: {e}", config.id);
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT output (no redundancy)
// ---------------------------------------------------------------------------

/// Core send loop for a single-leg SRT output (no redundancy).
///
/// Connects to the SRT peer (caller or listener per config), then bridges
/// the async broadcast receiver to the SRT socket via a bounded mpsc channel
/// and a dedicated Tokio send task:
///
///   `[async task: broadcast recv] --mpsc try_send--> [tokio task: async SRT send]`
///
/// **Backpressure / drop behavior**: the mpsc channel has a capacity of 256.
/// The main task uses `try_send` (non-blocking) to enqueue packets. If the
/// SRT send task cannot keep up (e.g., the SRT congestion control is
/// throttling), the channel fills up and `try_send` returns `Full`. In that
/// case the packet is dropped and `stats.packets_dropped` is incremented.
/// This design ensures the broadcast receiver is never blocked by a slow
/// SRT destination.
///
/// If the send task encounters a send error (connection lost), it exits,
/// closing the mpsc channel. The main task detects this via
/// `TrySendError::Closed` and enters the reconnection path.
async fn srt_output_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // Outer reconnection loop
    loop {
        // Connect with retry (respects cancellation)
        let socket = match connect_srt_output(config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    tracing::info!(
                        "SRT output '{}' stopping (cancelled during connect)",
                        config.id
                    );
                    return Ok(());
                }
                tracing::error!("SRT output '{}' connection failed: {e}", config.id);
                return Err(e);
            }
        };

        tracing::info!(
            "SRT output '{}' connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr,
        );

        // Bridge the broadcast receiver to the async SRT send via a bounded
        // mpsc channel and a dedicated Tokio task. The mpsc channel (capacity
        // 256) acts as a small buffer. The main task uses `try_send` so it
        // never blocks.
        let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
        let send_socket = socket.clone();
        let send_stats = stats.clone();
        let output_id = config.id.clone();
        let send_handle = tokio::spawn(async move {
            while let Some(data) = send_rx.recv().await {
                match send_socket.send(&data).await {
                    Ok(sent) => {
                        send_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        send_stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("SRT output '{}' send error: {e}", output_id);
                        break;
                    }
                }
            }
        });

        // Inner packet forwarding loop — runs until disconnect or cancel
        let connection_lost = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT output '{}' stopping (cancelled)", config.id);
                    drop(send_tx); // close channel, send task will exit
                    let _ = send_handle.await;
                    let _ = socket.close().await;
                    return Ok(());
                }
                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            // Use try_send (non-blocking) to avoid stalling the async
                            // task. If the mpsc channel is full, SRT's send task
                            // cannot keep up with the input rate and we must drop
                            // the packet rather than block — blocking here would
                            // back-pressure the broadcast channel and affect all
                            // other outputs sharing the same input.
                            match send_tx.try_send(packet.data) {
                                Ok(()) => {}
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                    // Send task died (connection lost)
                                    tracing::warn!(
                                        "SRT output '{}' connection lost, will reconnect",
                                        config.id
                                    );
                                    break true;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                            tracing::warn!(
                                "SRT output '{}' lagged, dropped {n} packets",
                                config.id
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "SRT output '{}' broadcast channel closed",
                                config.id
                            );
                            drop(send_tx);
                            let _ = send_handle.await;
                            let _ = socket.close().await;
                            return Ok(());
                        }
                    }
                }
            }
        };

        // Close old socket before reconnecting
        drop(send_tx);
        let _ = send_handle.await;
        let _ = socket.close().await;

        if !connection_lost {
            break;
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT output '{}' attempting reconnection...", config.id);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dual-leg SRT output with SMPTE 2022-7 duplication
// ---------------------------------------------------------------------------

/// Core send loop for dual-leg SRT output with SMPTE 2022-7 packet duplication.
///
/// Connects two independent SRT legs (leg 1 from the main config, leg 2 from
/// the redundancy config). Each leg gets its own dedicated Tokio send task
/// and mpsc channel, following the same pattern as the single-leg output.
///
/// An [`SrtDuplicator`] wraps both mpsc senders and provides a single
/// `send()` method that duplicates every packet to both legs using
/// `try_send`. Each leg operates independently: if one leg's channel is full
/// (backpressure), the packet is dropped for that leg only; the other leg
/// still receives it. If one leg's send task dies (connection lost), the
/// duplicator continues sending to the surviving leg.
///
/// Stats counting: only leg 1 increments `packets_sent` and `bytes_sent` to
/// avoid double-counting, since both legs carry identical data.
async fn srt_output_redundant_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Outer reconnection loop — restarts both legs on total connection loss
    loop {
        // Connect leg 1
        let socket_leg1 = match connect_srt_output(config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                return Err(e);
            }
        };
        tracing::info!(
            "SRT output '{}' leg1 connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr
        );

        // Connect leg 2 (best-effort — fall back to single-leg if it fails)
        let socket_leg2 = match connect_srt_redundancy_leg(redundancy, &cancel).await {
            Ok(s) => Some(s),
            Err(e) => {
                if cancel.is_cancelled() {
                    let _ = socket_leg1.close().await;
                    return Ok(());
                }
                tracing::warn!(
                    "SRT output '{}' leg2 connection failed: {e}, running single-leg",
                    config.id
                );
                None
            }
        };
        if socket_leg2.is_some() {
            tracing::info!(
                "SRT output '{}' leg2 connected: mode={:?} local={}",
                config.id,
                redundancy.mode,
                redundancy.local_addr
            );
        }

        // Spawn async send tasks — one per leg.
        let (send_tx_leg1, mut send_rx_leg1) = tokio::sync::mpsc::channel::<Bytes>(256);

        let send_socket1 = socket_leg1.clone();
        let send_stats1 = stats.clone();
        let output_id1 = config.id.clone();
        let send_handle1 = tokio::spawn(async move {
            while let Some(data) = send_rx_leg1.recv().await {
                match send_socket1.send(&data).await {
                    Ok(sent) => {
                        send_stats1.packets_sent.fetch_add(1, Ordering::Relaxed);
                        send_stats1.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("SRT output '{}' leg1 send error: {e}", output_id1);
                        break;
                    }
                }
            }
        });

        // Only spawn leg2 send task if leg2 is connected
        let send_tx_leg2_opt = if let Some(ref sock2) = socket_leg2 {
            let (send_tx_leg2, mut send_rx_leg2) = tokio::sync::mpsc::channel::<Bytes>(256);
            let send_socket2 = sock2.clone();
            let output_id2 = config.id.clone();
            let send_handle2 = tokio::spawn(async move {
                while let Some(data) = send_rx_leg2.recv().await {
                    match send_socket2.send(&data).await {
                        Ok(_) => {
                            // Don't double-count — leg1 already counted
                        }
                        Err(e) => {
                            tracing::warn!("SRT output '{}' leg2 send error: {e}", output_id2);
                            break;
                        }
                    }
                }
            });
            let _ = send_handle2; // Task will exit when channel drops
            Some(send_tx_leg2)
        } else {
            None
        };

        // Packet forwarding loop — runs until both legs die or cancel
        let mut broadcast_closed = false;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT output '{}' (redundant) stopping (cancelled)", config.id);
                    drop(send_tx_leg1);
                    drop(send_tx_leg2_opt);
                    let _ = send_handle1.await;
                    let _ = socket_leg1.close().await;
                    if let Some(ref s) = socket_leg2 {
                        let _ = s.close().await;
                    }
                    return Ok(());
                }
                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            let leg1_ok = match send_tx_leg1.try_send(packet.data.clone()) {
                                Ok(()) => true,
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                    false
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                            };

                            let leg2_ok = if let Some(ref tx2) = send_tx_leg2_opt {
                                match tx2.try_send(packet.data) {
                                    Ok(()) => true,
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                        false
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                                }
                            } else {
                                false
                            };

                            if !leg1_ok && !leg2_ok {
                                tracing::warn!(
                                    "SRT output '{}' all legs lost, will reconnect",
                                    config.id
                                );
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                            tracing::warn!(
                                "SRT output '{}' (redundant) lagged, dropped {n} packets",
                                config.id
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "SRT output '{}' broadcast channel closed",
                                config.id
                            );
                            broadcast_closed = true;
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup before reconnect
        drop(send_tx_leg1);
        drop(send_tx_leg2_opt);
        let _ = send_handle1.await;
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        if broadcast_closed {
            return Ok(());
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' (redundant) stopping during reconnect delay", config.id);
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT output '{}' (redundant) attempting reconnection...", config.id);
    }
}
