// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(dead_code)]

//! SMPTE ST 2110 support modules.
//!
//! Broadcast-essence support code shared by the ST 2110-20/-23 (uncompressed
//! video), ST 2110-30 (PCM audio), ST 2110-31 (AES3 transparent audio), and
//! ST 2110-40 (ancillary data) input/output tasks.
//!
//! ## Pure Rust
//!
//! All modules in this tree are 100% Rust. PTP synchronisation is provided by
//! reading the `ptp4l` (linuxptp) management Unix socket externally — bilbycast
//! does not link a C PTP library.
//!
//! ## Modules
//!
//! - [`ptp`]: PTP clock state reporter, ptp4l UDS reader, lock state model.
//! - [`pacer`]: ST 2110-21 raster pacer producing `target_tx_time_ns` per
//!   RFC 4175 packet, anchored on `CLOCK_TAI` when PTP is locked.
//! - [`redblue`]: SMPTE 2022-7 dual-network ("Red"/"Blue") bind helpers.
//! - [`timeline`]: per-flow cross-essence media-timeline anchor so the
//!   2110 inputs' synthesised PES timelines share one origin (A/V sync).
//! - Packetisers (`audio`, `ancillary`, `scte104`, `timecode`, `captions`,
//!   `video`) and the SDP module.

pub mod ancillary;
pub mod audio;
pub mod captions;
pub mod pacer;
pub mod ptp;
pub mod redblue;
pub mod scte104;
pub mod sdp;
pub mod timecode;
pub mod timeline;
pub mod video;
