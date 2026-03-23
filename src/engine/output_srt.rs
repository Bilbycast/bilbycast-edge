use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use srt_transport::SrtSocket;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtMode, SrtOutputConfig};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_output, bind_srt_listener_for_redundancy,
    connect_srt_output, connect_srt_redundancy_leg,
};
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
/// Dispatches to listener-specific or caller-specific loops based on the
/// SRT mode. Listener mode binds once and re-accepts on disconnect; caller
/// mode reconnects with exponential back-off.
async fn srt_output_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_output_listener_loop(config, rx, stats, cancel).await,
        _ => srt_output_caller_loop(config, rx, stats, cancel).await,
    }
}

/// Listener-mode output: binds the listener once and re-accepts new callers
/// on disconnect, avoiding the port-rebind problem.
async fn srt_output_listener_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut listener = bind_srt_listener_for_output(config).await?;

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                tracing::info!(
                    "SRT output '{}' stopping (cancelled during accept)",
                    config.id
                );
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT output '{}' accept failed: {e}", config.id);
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT output '{}' connected: mode=listener local={}",
            config.id,
            config.local_addr,
        );

        let disconnected = srt_output_forward_loop(config, rx, &stats, &cancel, &socket).await?;
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        // Brief delay before re-accepting
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT output '{}' waiting for caller to reconnect...", config.id);
    }
}

/// Caller-mode output: reconnects with exponential back-off on disconnect.
async fn srt_output_caller_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    loop {
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

        let disconnected = srt_output_forward_loop(config, rx, &stats, &cancel, &socket).await?;
        let _ = socket.close().await;

        if !disconnected {
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

/// Inner packet forwarding loop shared by listener and caller output modes.
///
/// Bridges the broadcast receiver to the SRT socket via a bounded mpsc channel
/// and a dedicated Tokio send task. Returns `Ok(true)` if the connection was
/// lost (caller should reconnect), `Ok(false)` if the broadcast channel closed
/// (flow is shutting down).
async fn srt_output_forward_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
    socket: &Arc<SrtSocket>,
) -> anyhow::Result<bool> {
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

    let connection_lost = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping (cancelled)", config.id);
                drop(send_tx);
                let _ = send_handle.await;
                return Ok(false);
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        match send_tx.try_send(packet.data) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
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
                        return Ok(false);
                    }
                }
            }
        }
    };

    drop(send_tx);
    let _ = send_handle.await;
    Ok(connection_lost)
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
/// Listener-mode legs bind once and re-accept on disconnect; caller-mode legs
/// reconnect with exponential back-off.
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

    // Bind persistent listeners for any legs in listener mode
    let mut listener_leg1 = if config.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_output(config).await?)
    } else {
        None
    };
    let mut listener_leg2 = if redundancy.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_redundancy(redundancy).await?)
    } else {
        None
    };

    // Outer reconnection loop — restarts both legs on total connection loss
    loop {
        // Connect/accept leg 1
        let socket_leg1 = if let Some(ref mut listener) = listener_leg1 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => s,
                Err(_) if cancel.is_cancelled() => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match connect_srt_output(config, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        };
        tracing::info!(
            "SRT output '{}' leg1 connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr
        );

        // Connect/accept leg 2 (best-effort — fall back to single-leg if it fails)
        let socket_leg2 = if let Some(ref mut listener) = listener_leg2 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => Some(s),
                Err(_) if cancel.is_cancelled() => {
                    let _ = socket_leg1.close().await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        "SRT output '{}' leg2 accept failed: {e}, running single-leg",
                        config.id
                    );
                    None
                }
            }
        } else {
            match connect_srt_redundancy_leg(redundancy, &cancel).await {
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
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
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

        // Cleanup sockets before reconnect (listeners stay open)
        drop(send_tx_leg1);
        drop(send_tx_leg2_opt);
        let _ = send_handle1.await;
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        if broadcast_closed {
            if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
            if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
            return Ok(());
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' (redundant) stopping during reconnect delay", config.id);
                if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT output '{}' (redundant) attempting reconnection...", config.id);
    }
}
