// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hot-path performance instrumentation helpers.
//!
//! Codec work on transcoding output paths runs synchronously inside
//! `tokio::task::block_in_place`. By design the broadcast channel is
//! drop-on-Lagged so a slow codec only impacts its own output, but a
//! sustained over-budget call still translates into permanent packet loss
//! on that output. The [`timed_block_in_place!`] macro wraps a
//! `block_in_place` body, measures its wall-clock duration, and emits a
//! sampled `tracing::warn!` if the threshold is exceeded — at most one
//! warning per call site per second so a pathological codec does not spam
//! the log.

/// Wrap `block_in_place` with a wall-clock timer and a per-call-site
/// rate-limited warning when the body exceeds `threshold_ms`.
///
/// Each macro invocation site has its own `static` rate-limit cell, so two
/// different sites don't share a warning budget.
///
/// # Example
///
/// ```ignore
/// use crate::timed_block_in_place;
///
/// timed_block_in_place!("audio_replacer", 50, {
///     replacer.process(filtered_bytes, &mut replace_scratch);
/// });
/// ```
#[macro_export]
macro_rules! timed_block_in_place {
    ($label:expr, $threshold_ms:expr, $body:block) => {{
        static __LAST_WARN_US: ::std::sync::atomic::AtomicU64 =
            ::std::sync::atomic::AtomicU64::new(0);
        let __start = ::std::time::Instant::now();
        let __r = ::tokio::task::block_in_place(|| $body);
        let __elapsed_ms = __start.elapsed().as_millis() as u64;
        if __elapsed_ms >= ($threshold_ms as u64) {
            let __now_us = $crate::util::time::now_us();
            let __last = __LAST_WARN_US.load(::std::sync::atomic::Ordering::Relaxed);
            if __now_us.saturating_sub(__last) >= 1_000_000
                && __LAST_WARN_US
                    .compare_exchange(
                        __last,
                        __now_us,
                        ::std::sync::atomic::Ordering::Relaxed,
                        ::std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
            {
                ::tracing::warn!(
                    site = $label,
                    elapsed_ms = __elapsed_ms,
                    threshold_ms = ($threshold_ms as u64),
                    "block_in_place exceeded threshold (sampled, max once/sec/site)",
                );
            }
        }
        __r
    }};
}

/// Default warning threshold for transcoding output paths, in milliseconds.
/// One AAC frame at 48 kHz is ~21 ms; a single H.264 keyframe encode can run
/// to ~100 ms on slower CPUs. 50 ms catches sustained codec stalls without
/// firing on healthy keyframe boundaries.
pub const TRANSCODE_BLOCK_WARN_MS: u64 = 50;
