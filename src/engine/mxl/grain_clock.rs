//! MXL grain timing helpers.
//!
//! libmxl identifies grains and sample windows by a monotonically-increasing
//! 64-bit `index`. The producer (writer) chooses the index; the consumer
//! (reader) typically asks for the writer's current head (via
//! `FlowReader::get_runtime_info().head_index()`) and walks forward from
//! there.
//!
//! This module centralises the small bit of math that converts between
//! `index` and bilbycast's per-flow master_clock (27 MHz PTP-anchored).
//! M2 reader/writer loops query `head_index()` at startup and advance
//! locally; richer PTP-aligned indexing is a Phase 2 follow-up once we
//! have hardware to verify against.

use std::time::Duration;

/// Default poll timeout when waiting for a new grain. Long enough that the
/// caller's tokio cancellation wins on shutdown, short enough that an
/// observable "no input" condition surfaces.
pub const DEFAULT_GRAIN_WAIT: Duration = Duration::from_millis(200);

/// Maximum samples in a single `get_samples` batch — bounds the I/O
/// thread's loop pace. At 48 kHz Float32 stereo with 1 ms packet_time_us,
/// that's 48 samples per channel per call — well under this cap. Used
/// by the future-wired audio_encode path; M2 spawn loop today uses
/// per-config packet_time directly.
#[allow(dead_code)]
pub const MAX_SAMPLES_PER_BATCH: usize = 4096;

/// Convert MXL grain rate (rational) → nominal frame duration in 27 MHz ticks.
/// Used by the future PTS/PCR generator paths; returns `None` for invalid
/// rationals (zero numerator or denominator).
#[allow(dead_code)] // wired by M2/M3 conversion code, kept here so the
                    // helper is ready when those land.
pub fn frame_duration_27mhz(numerator: u32, denominator: u32) -> Option<u64> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    // ticks = 27_000_000 * denominator / numerator
    Some(27_000_000u64 * denominator as u64 / numerator as u64)
}
