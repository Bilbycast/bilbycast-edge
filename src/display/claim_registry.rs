// Per-edge runtime registry for "who currently holds which physical
// display output." A display output identifies the physical port via
// `(device, audio_device)` — e.g. `("HDMI-A-1", "hw:0,3")`. KMS only
// lets one master lease a connector, and ALSA only lets one writer
// hold the PCM device, so this pair is the unit of contention.
//
// When two flows configure outputs that target the same pair, the
// first to start wins. The others register as FCFS waiters here and
// park inside `engine::output_display::run_display_output` until
// either they're promoted (the holder released) or their per-output
// `CancellationToken` fires.
//
// Promotion is single-flight: `Guard::drop` walks the waiter queue
// under the per-key shard lock, skipping any waiter whose cancel
// token has already fired, and notifies exactly one. A waiter that
// cancels mid-wait removes itself from the queue (or, if it was
// promoted between the cancel firing and the cancel arm running,
// releases the holder slot and promotes the next live waiter).
//
// The registry is constructed once next to `FlowManager` in `main.rs`
// and threaded through to the display spawner. Pure Rust — DashMap,
// Notify, CancellationToken — so it compiles fine on any host that
// has the `display` feature enabled.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Identifies one physical display output pair: a KMS connector name
/// (`"HDMI-A-1"`, `"DP-2"`, …) and an optional ALSA device (`"hw:0,3"`).
/// Two outputs that target the same pair contend for the same hardware.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ClaimKey {
    pub device: String,
    /// `""` when the output has no audio sink. Mirrors the validator
    /// in `config/validation.rs::validate_display_uniqueness`.
    pub audio_device: String,
}

impl ClaimKey {
    pub fn new(device: impl Into<String>, audio_device: Option<impl Into<String>>) -> Self {
        Self {
            device: device.into(),
            audio_device: audio_device
                .map(Into::into)
                .filter(|s: &String| !s.is_empty())
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone)]
struct HolderInfo {
    flow_id: String,
    output_id: String,
}

struct Waiter {
    flow_id: String,
    output_id: String,
    notify: Arc<Notify>,
    cancel: CancellationToken,
}

#[derive(Default)]
struct KeyState {
    holder: Option<HolderInfo>,
    waiters: VecDeque<Waiter>,
}

/// Per-edge registry of active display claims.
pub struct DisplayClaimRegistry {
    keys: DashMap<ClaimKey, KeyState>,
}

impl DisplayClaimRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            keys: DashMap::new(),
        })
    }

    /// Snapshot the current holder for a key, if any. The spawner gets
    /// this info via `WaitTicket::info` at queue time; this accessor is
    /// kept for future diagnostic surfaces (e.g. a "show me who holds
    /// this connector" admin command). Racy by design — the holder can
    /// release the moment the lock drops.
    #[allow(dead_code)]
    pub fn holder_snapshot(&self, key: &ClaimKey) -> Option<(String, String)> {
        self.keys
            .get(key)
            .and_then(|s| s.holder.as_ref().map(|h| (h.flow_id.clone(), h.output_id.clone())))
    }

    /// Try to acquire the slot for `(key, flow_id, output_id)`.
    ///
    /// - If the slot is free, returns [`ClaimOutcome::Granted`] with a
    ///   [`Guard`] that releases the slot on drop.
    /// - Otherwise registers as the last waiter and returns
    ///   [`ClaimOutcome::Queued`] with a [`WaitTicket`] the caller can
    ///   `await` for promotion.
    pub fn try_claim(
        self: &Arc<Self>,
        key: ClaimKey,
        flow_id: String,
        output_id: String,
        cancel: CancellationToken,
    ) -> ClaimOutcome {
        let mut entry = self.keys.entry(key.clone()).or_default();
        let state: &mut KeyState = entry.value_mut();
        if state.holder.is_none() && state.waiters.is_empty() {
            state.holder = Some(HolderInfo {
                flow_id: flow_id.clone(),
                output_id: output_id.clone(),
            });
            ClaimOutcome::Granted(Guard {
                registry: Arc::clone(self),
                key,
                output_id,
                released: false,
            })
        } else {
            let notify = Arc::new(Notify::new());
            let queue_position = state.waiters.len();
            let holder = state.holder.clone();
            state.waiters.push_back(Waiter {
                flow_id: flow_id.clone(),
                output_id: output_id.clone(),
                notify: Arc::clone(&notify),
                cancel: cancel.clone(),
            });
            ClaimOutcome::Queued(WaitTicket {
                registry: Arc::clone(self),
                key,
                output_id,
                flow_id,
                notify,
                cancel,
                info: WaitInfo {
                    queue_position,
                    holder_flow_id: holder.as_ref().map(|h| h.flow_id.clone()),
                    holder_output_id: holder.as_ref().map(|h| h.output_id.clone()),
                },
            })
        }
    }

    /// Release the holder slot identified by `output_id` (if we are still
    /// the holder) and promote the next live waiter. Called from
    /// `Guard::drop` and from the cleanup path inside `wait_for_grant`
    /// when a grant raced with a cancellation. Idempotent — calling it
    /// when we are not the holder is a no-op.
    fn release_and_promote(&self, key: &ClaimKey, output_id: &str) {
        if let Some(mut state_ref) = self.keys.get_mut(key) {
            let state = state_ref.value_mut();
            let we_are_holder = state
                .holder
                .as_ref()
                .map(|h| h.output_id.as_str() == output_id)
                .unwrap_or(false);
            if !we_are_holder {
                return;
            }
            state.holder = None;
            while let Some(w) = state.waiters.pop_front() {
                if !w.cancel.is_cancelled() {
                    state.holder = Some(HolderInfo {
                        flow_id: w.flow_id,
                        output_id: w.output_id,
                    });
                    w.notify.notify_one();
                    return;
                }
                // Cancelled waiter — drop and try the next one.
            }
        }
    }

    /// Cancellation cleanup — either remove ourselves from the queue
    /// (if still parked) or release the holder slot (if we were
    /// promoted between our cancel firing and the cancel arm running
    /// in the spawner).
    fn cancel_or_release(&self, key: &ClaimKey, output_id: &str) {
        if let Some(mut state_ref) = self.keys.get_mut(key) {
            let state = state_ref.value_mut();
            if state
                .holder
                .as_ref()
                .map(|h| h.output_id.as_str() == output_id)
                .unwrap_or(false)
            {
                state.holder = None;
                while let Some(w) = state.waiters.pop_front() {
                    if !w.cancel.is_cancelled() {
                        state.holder = Some(HolderInfo {
                            flow_id: w.flow_id,
                            output_id: w.output_id,
                        });
                        w.notify.notify_one();
                        return;
                    }
                }
            } else {
                state.waiters.retain(|w| w.output_id != output_id);
            }
        }
    }
}

/// Outcome of [`DisplayClaimRegistry::try_claim`].
pub enum ClaimOutcome {
    Granted(Guard),
    Queued(WaitTicket),
}

/// RAII handle to an active display claim. Drops release the slot and
/// promote the next live waiter under the per-key shard lock.
pub struct Guard {
    registry: Arc<DisplayClaimRegistry>,
    key: ClaimKey,
    output_id: String,
    released: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.registry.release_and_promote(&self.key, &self.output_id);
        }
    }
}

/// Snapshot describing a queued waiter at the moment it was registered.
/// Surfaced to the caller so it can emit a `display_output_waiting`
/// event with structured details for the manager UI.
#[derive(Debug, Clone)]
pub struct WaitInfo {
    pub queue_position: usize,
    pub holder_flow_id: Option<String>,
    pub holder_output_id: Option<String>,
}

/// Returned from [`DisplayClaimRegistry::try_claim`] when the slot is
/// busy. `await` [`WaitTicket::wait_for_grant`] to receive a [`Guard`]
/// when the holder releases, or an error when the per-output cancel
/// token fires first.
pub struct WaitTicket {
    registry: Arc<DisplayClaimRegistry>,
    key: ClaimKey,
    output_id: String,
    flow_id: String,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    pub info: WaitInfo,
}

impl WaitTicket {
    pub async fn wait_for_grant(self) -> Result<Guard, WaitError> {
        let WaitTicket {
            registry,
            key,
            output_id,
            flow_id,
            notify,
            cancel,
            info: _,
        } = self;

        // Cleanup runs unless we successfully grant — handles both the
        // cancel arm and the case where the future is dropped before
        // either select branch fires. `armed = false` after a grant
        // means Guard::drop is now responsible for the slot.
        struct Cleanup {
            registry: Arc<DisplayClaimRegistry>,
            key: ClaimKey,
            output_id: String,
            armed: bool,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.armed {
                    self.registry.cancel_or_release(&self.key, &self.output_id);
                }
            }
        }
        let mut cleanup = Cleanup {
            registry: Arc::clone(&registry),
            key: key.clone(),
            output_id: output_id.clone(),
            armed: true,
        };

        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(WaitError::Cancelled),
            _ = notify.notified() => {
                cleanup.armed = false;
                let _ = flow_id; // retained for potential future telemetry
                Ok(Guard {
                    registry,
                    key,
                    output_id,
                    released: false,
                })
            }
        }
    }
}

#[derive(Debug)]
pub enum WaitError {
    Cancelled,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitError::Cancelled => f.write_str("display claim wait cancelled"),
        }
    }
}

impl std::error::Error for WaitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn key(dev: &str) -> ClaimKey {
        ClaimKey::new(dev.to_string(), Some("hw:0,3".to_string()))
    }

    #[tokio::test]
    async fn single_claim_grants_immediately() {
        let reg = DisplayClaimRegistry::new();
        let cancel = CancellationToken::new();
        match reg.try_claim(key("HDMI-A-1"), "f1".into(), "o1".into(), cancel) {
            ClaimOutcome::Granted(_g) => {}
            ClaimOutcome::Queued(_) => panic!("expected immediate grant"),
        }
    }

    #[tokio::test]
    async fn second_claim_queues_and_wakes_on_release() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        let g1 = match reg.try_claim(k.clone(), "f1".into(), "o1".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => panic!("first claim must be granted"),
        };
        let ticket = match reg.try_claim(k.clone(), "f2".into(), "o2".into(), CancellationToken::new()) {
            ClaimOutcome::Queued(t) => t,
            _ => panic!("second claim must be queued"),
        };
        assert_eq!(ticket.info.queue_position, 0);
        assert_eq!(ticket.info.holder_output_id.as_deref(), Some("o1"));

        let waiter = tokio::spawn(async move { ticket.wait_for_grant().await });
        // Yield once so the waiter task gets to install its notified() future.
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(g1);
        let g2 = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter did not wake")
            .expect("join")
            .expect("grant");
        // Re-claiming with another flow now queues behind g2.
        match reg.try_claim(k, "f3".into(), "o3".into(), CancellationToken::new()) {
            ClaimOutcome::Queued(t) => assert_eq!(t.info.holder_output_id.as_deref(), Some("o2")),
            _ => panic!("expected queued behind g2"),
        }
        drop(g2);
    }

    #[tokio::test]
    async fn fcfs_three_deep_queue() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        let g = match reg.try_claim(k.clone(), "f0".into(), "o0".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        let mut order = Vec::new();
        let mut handles = Vec::new();
        for i in 1..=3 {
            let oid = format!("o{i}");
            let ticket = match reg.try_claim(
                k.clone(),
                format!("f{i}"),
                oid.clone(),
                CancellationToken::new(),
            ) {
                ClaimOutcome::Queued(t) => t,
                _ => panic!("expected queue"),
            };
            assert_eq!(ticket.info.queue_position, i - 1);
            handles.push((oid, tokio::spawn(async move { ticket.wait_for_grant().await })));
        }
        // Drain the queue in order — each drop wakes exactly one waiter.
        let mut current = g;
        for (oid, handle) in handles {
            drop(current);
            let granted = tokio::time::timeout(Duration::from_secs(1), handle)
                .await
                .expect("waiter did not wake")
                .expect("join")
                .expect("grant");
            order.push(oid);
            current = granted;
        }
        drop(current);
        assert_eq!(order, vec!["o1", "o2", "o3"]);
    }

    #[tokio::test]
    async fn cancelled_middle_waiter_is_skipped() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        let g = match reg.try_claim(k.clone(), "f0".into(), "o0".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        let cancel_mid = CancellationToken::new();
        let t1 = match reg.try_claim(k.clone(), "f1".into(), "o1".into(), cancel_mid.clone()) {
            ClaimOutcome::Queued(t) => t,
            _ => unreachable!(),
        };
        let t2 = match reg.try_claim(
            k.clone(),
            "f2".into(),
            "o2".into(),
            CancellationToken::new(),
        ) {
            ClaimOutcome::Queued(t) => t,
            _ => unreachable!(),
        };

        let h1 = tokio::spawn(async move { t1.wait_for_grant().await });
        let h2 = tokio::spawn(async move { t2.wait_for_grant().await });
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cancel the middle waiter — it removes itself from the queue.
        cancel_mid.cancel();
        let r1 = tokio::time::timeout(Duration::from_secs(1), h1)
            .await
            .expect("h1 did not resolve")
            .expect("join");
        assert!(matches!(r1, Err(WaitError::Cancelled)));

        // Now release the holder. Promotion must skip nothing (o1 already
        // gone) and grant straight to o2.
        drop(g);
        let g2 = tokio::time::timeout(Duration::from_secs(1), h2)
            .await
            .expect("h2 did not wake")
            .expect("join")
            .expect("grant");
        drop(g2);
    }

    #[tokio::test]
    async fn drop_with_empty_queue_leaves_key_vacant() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        let g = match reg.try_claim(k.clone(), "f0".into(), "o0".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        drop(g);
        match reg.try_claim(k, "f1".into(), "o1".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(_) => {}
            _ => panic!("expected re-grant after release"),
        }
    }

    #[tokio::test]
    async fn promoted_then_cancelled_releases_to_next() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        let g = match reg.try_claim(k.clone(), "f0".into(), "o0".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        let cancel_promoted = CancellationToken::new();
        let t1 = match reg.try_claim(k.clone(), "f1".into(), "o1".into(), cancel_promoted.clone()) {
            ClaimOutcome::Queued(t) => t,
            _ => unreachable!(),
        };
        let t2 = match reg.try_claim(
            k.clone(),
            "f2".into(),
            "o2".into(),
            CancellationToken::new(),
        ) {
            ClaimOutcome::Queued(t) => t,
            _ => unreachable!(),
        };

        // Cancel the front waiter *before* it has a chance to be polled,
        // then drop the holder. The biased select picks cancel first, so
        // the cleanup path removes o1 and promotion lands on o2.
        let h1 = tokio::spawn(async move { t1.wait_for_grant().await });
        let h2 = tokio::spawn(async move { t2.wait_for_grant().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_promoted.cancel();
        drop(g);

        let r1 = tokio::time::timeout(Duration::from_secs(1), h1)
            .await
            .expect("h1 did not resolve")
            .expect("join");
        assert!(matches!(r1, Err(WaitError::Cancelled)));
        let g2 = tokio::time::timeout(Duration::from_secs(1), h2)
            .await
            .expect("h2 did not wake")
            .expect("join")
            .expect("grant");
        drop(g2);
    }

    #[tokio::test]
    async fn distinct_keys_do_not_contend() {
        let reg = DisplayClaimRegistry::new();
        let g1 = match reg.try_claim(key("HDMI-A-1"), "f1".into(), "o1".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        let g2 = match reg.try_claim(key("DP-1"), "f2".into(), "o2".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        drop(g1);
        drop(g2);
    }

    #[tokio::test]
    async fn holder_snapshot_round_trips() {
        let reg = DisplayClaimRegistry::new();
        let k = key("HDMI-A-1");
        assert!(reg.holder_snapshot(&k).is_none());
        let g = match reg.try_claim(k.clone(), "fA".into(), "oA".into(), CancellationToken::new()) {
            ClaimOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        assert_eq!(
            reg.holder_snapshot(&k),
            Some(("fA".into(), "oA".into()))
        );
        drop(g);
        assert!(reg.holder_snapshot(&k).is_none());
    }
}
