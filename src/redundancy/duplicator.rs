//! SMPTE 2022-7 TX Duplicator -- sends each RTP packet to both SRT legs.
//!
//! The duplicator owns two mpsc senders (one per blocking send thread).
//! It uses `try_send` to avoid blocking the async task when either SRT leg
//! is congested, counting drops per-leg via atomic stats counters.
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::stats::collector::OutputStatsAccumulator;

/// Sends each RTP packet to both SRT legs using non-blocking `try_send`.
///
/// Holds a pair of `mpsc::Sender<Bytes>` channels, one per SRT leg. When
/// a channel is full (the downstream SRT send thread has not drained it),
/// the packet is dropped and the drop counter in [`OutputStatsAccumulator`]
/// is incremented.
pub struct SrtDuplicator {
    pub send_tx_leg1: mpsc::Sender<Bytes>,
    pub send_tx_leg2: mpsc::Sender<Bytes>,
}

impl SrtDuplicator {
    /// Create a new duplicator from two pre-built mpsc senders.
    ///
    /// Each sender feeds a dedicated blocking thread that performs the actual
    /// `srt_sendmsg` call on the corresponding SRT socket.
    pub fn new(
        send_tx_leg1: mpsc::Sender<Bytes>,
        send_tx_leg2: mpsc::Sender<Bytes>,
    ) -> Self {
        Self {
            send_tx_leg1,
            send_tx_leg2,
        }
    }

    /// Send packet data to both SRT legs.
    ///
    /// Returns `(leg1_ok, leg2_ok)` indicating whether each leg accepted the packet.
    /// Drops are counted in the stats accumulator.
    pub fn send(&self, data: Bytes, stats: &OutputStatsAccumulator) -> (bool, bool) {
        let leg1_ok = match self.send_tx_leg1.try_send(data.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        };

        let leg2_ok = match self.send_tx_leg2.try_send(data) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        };

        (leg1_ok, leg2_ok)
    }

    /// Check if both legs are still connected (neither channel has been closed).
    ///
    /// Returns `true` only when both SRT send threads are alive and their
    /// mpsc receivers have not been dropped.
    pub fn both_connected(&self) -> bool {
        !self.send_tx_leg1.is_closed() && !self.send_tx_leg2.is_closed()
    }

    /// Check if at least one leg is still connected.
    ///
    /// Useful for deciding whether the duplicator should keep running:
    /// as long as one leg can accept packets, the flow can continue.
    pub fn any_connected(&self) -> bool {
        !self.send_tx_leg1.is_closed() || !self.send_tx_leg2.is_closed()
    }
}
