// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

// The modules below land in Phase 1 step 2 as foundation infrastructure.
// They are exercised by their own unit tests and will be wired into the
// ST 2110 input/output tasks in step 4. Suppress dead-code warnings until
// then to keep the build clean.
#![allow(dead_code)]

//! SMPTE ST 2110 support modules.
//!
//! This submodule contains the broadcast-essence support code shared by the
//! ST 2110-30 (PCM audio), ST 2110-31 (AES3 transparent audio), and ST 2110-40
//! (ancillary data) input/output tasks.
//!
//! ## Pure Rust
//!
//! All modules in this tree are 100% Rust. PTP synchronization is provided by
//! reading the `ptp4l` (linuxptp) management Unix socket externally — bilbycast
//! does not link a C PTP library. Hardware timestamps are extracted via
//! `SO_TIMESTAMPING` and `cmsghdr` parsing using `libc` FFI bindings. Optional
//! `--features ptp-internal` (placeholder for now) will integrate the pure-Rust
//! `statime` slave clock in a follow-up step.
//!
//! ## Phase 1 layout
//!
//! - [`ptp`]: PTP clock state reporter, ptp4l UDS reader, lock state model.
//! - [`hwts`]: hardware timestamp helpers (Linux SO_TIMESTAMPING).
//! - [`redblue`]: SMPTE 2022-7 dual-network ("Red"/"Blue") bind helpers.
//!
//! Packetizers (`audio.rs`, `aes3.rs`, `ancillary.rs`, `scte104.rs`,
//! `timecode.rs`, `captions.rs`) and the SDP module land in steps 3-4.

pub mod ancillary;
pub mod audio;
pub mod captions;
pub mod hwts;
pub mod ptp;
pub mod redblue;
pub mod scte104;
pub mod sdp;
pub mod timecode;
