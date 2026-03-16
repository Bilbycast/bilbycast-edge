//! Shared utilities: RTP parsing, UDP socket setup, and monotonic time.
//!
//! - [`rtp_parse`] -- Zero-copy extraction of RTP header fields: sequence number,
//!   timestamp, SSRC, payload type, and version. Includes heuristic validation
//!   (`is_likely_rtp`) for filtering non-RTP traffic.
//! - [`socket`] -- UDP socket creation with `SO_REUSEADDR`/`SO_REUSEPORT`,
//!   multicast group join and send interface binding.
//! - [`time`] -- Monotonic microsecond clock initialized once at startup via
//!   `init_epoch()`. Provides `now_us()` for low-overhead timestamping.

pub mod rtp_parse;
pub mod socket;
pub mod time;
