//! MXL (Media eXchange Layer) integration — same-host cloud-native broadcast
//! composition (EBU / Linux Foundation, Apache-2.0).
//!
//! Gated by the `mxl` Cargo feature (off by default). The crate
//! [`bilbycast-mxl-rs`](../../../../bilbycast-mxl-rs) wraps the upstream
//! `dmf-mxl/mxl` v1.0.1 release. See
//! `bilbycast-edge/docs/mxl-integration-plan.md` for the architectural
//! rationale and `bilbycast-mxl-rs/CLAUDE.md` for the build prereq
//! footprint.
//!
//! M1 scope (this commit): boot-time probe + capability-bit advertisement
//! scaffold. Flow-level grain I/O lands in M2 (audio + ANC) and M3 (video).

pub mod ancillary;
pub mod audio;
pub mod domain;
pub mod grain_clock;
pub mod video;
