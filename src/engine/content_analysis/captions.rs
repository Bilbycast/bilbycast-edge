// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CEA-608 / CEA-708 closed-caption presence detection.
//!
//! Phase 1 scans for SEI payload type 4 (`user_data_registered_itu_t_t35`)
//! inside the video PES. The useful payload we're looking for starts with
//! `country_code = 0xB5` (US), provider_code `0x0031` (ATSC A/72), then a
//! `user_identifier` of `"GA94"` (0x47413934), followed by a `cc_data`
//! blob for CEA-708. This is the ATSC standard wrapper.
//!
//! Details we do **not** decode today (deferred to follow-up):
//!
//! - Individual caption services (SERVICE1 / SERVICE2 / ...).
//! - CEA-608 vs CEA-708 distinction beyond "GA94" presence.
//! - Teletext / DVB subtitle decode (different PIDs in the PMT; flag-only).
//!
//! The goal for Phase 1 is a boolean "are captions riding this stream?"
//! plus a packet counter so the manager UI can flag a sudden drop.

use std::time::{Duration, Instant};

use crate::stats::models::CaptionsStats;

const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);
const PEEK_CAP: usize = 2048;

pub struct CaptionsParser {
    packet_count: u64,
    last_seen_at: Option<Instant>,
    service_cea608: bool,
    service_cea708: bool,
    pes_peek: Vec<u8>,
    capturing_pes: bool,
}

impl CaptionsParser {
    pub fn new() -> Self {
        Self {
            packet_count: 0,
            last_seen_at: None,
            service_cea608: false,
            service_cea708: false,
            pes_peek: Vec::with_capacity(PEEK_CAP),
            capturing_pes: false,
        }
    }

    pub fn is_present(&self) -> bool {
        self.last_seen_at
            .map(|t| t.elapsed() <= PRESENCE_TIMEOUT)
            .unwrap_or(false)
    }

    pub fn observe_ts(&mut self, pusi: bool, payload: &[u8]) {
        if pusi {
            self.pes_peek.clear();
            self.capturing_pes = true;
        }
        if !self.capturing_pes {
            return;
        }
        let take = (PEEK_CAP - self.pes_peek.len()).min(payload.len());
        if take == 0 {
            self.capturing_pes = false;
            self.scan_peek();
            return;
        }
        self.pes_peek.extend_from_slice(&payload[..take]);
        if self.pes_peek.len() >= PEEK_CAP {
            self.capturing_pes = false;
            self.scan_peek();
        }
    }

    fn scan_peek(&mut self) {
        // Take ownership of the peek buffer so the scan can mutate self's
        // caption counters without a borrow conflict. The next PES clears
        // the buffer anyway, so discarding at scan-end is safe.
        let bytes = std::mem::take(&mut self.pes_peek);
        let mut i = 0;
        while i + 4 < bytes.len() {
            if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
                let nal = bytes[i + 3];
                if (nal & 0x1F) == 6 {
                    // AVC SEI NAL
                    if self.scan_avc_sei(&bytes[i + 4..]) {
                        return;
                    }
                }
                i += 4;
            } else {
                i += 1;
            }
        }
    }

    /// Walk an SEI RBSP looking for payload_type 4 with the ATSC A/72 wrapper.
    /// Returns `true` if captions were found (so the caller can short-circuit).
    fn scan_avc_sei(&mut self, sei: &[u8]) -> bool {
        let mut i = 0;
        while i < sei.len() {
            // Read payload_type (ff-byte-run + last byte)
            let mut payload_type: u32 = 0;
            while i < sei.len() && sei[i] == 0xFF {
                payload_type += 255;
                i += 1;
            }
            if i >= sei.len() {
                return false;
            }
            payload_type += sei[i] as u32;
            i += 1;
            let mut payload_size: u32 = 0;
            while i < sei.len() && sei[i] == 0xFF {
                payload_size += 255;
                i += 1;
            }
            if i >= sei.len() {
                return false;
            }
            payload_size += sei[i] as u32;
            i += 1;
            let end = i + payload_size as usize;
            if end > sei.len() {
                return false;
            }
            let payload = &sei[i..end];
            if payload_type == 4 && self.check_t35_captions(payload) {
                self.packet_count += 1;
                self.last_seen_at = Some(Instant::now());
                return true;
            }
            // NAL end signal: 0x80 byte (rbsp_trailing_bits). Stop.
            if i < sei.len() && sei[i] == 0x80 {
                return false;
            }
            i = end;
        }
        false
    }

    fn check_t35_captions(&mut self, payload: &[u8]) -> bool {
        // country_code (1) + provider_code (2) + user_identifier (4) + user_data (...)
        if payload.len() < 8 {
            return false;
        }
        if payload[0] != 0xB5 {
            return false;
        }
        let provider = ((payload[1] as u16) << 8) | payload[2] as u16;
        // ATSC provider codes: 0x0031 (user_data_type_structure)
        // or legacy 0x002F (SCTE).
        if provider != 0x0031 && provider != 0x002F {
            return false;
        }
        // user_identifier == "GA94" (0x47413934)
        if &payload[3..7] != b"GA94" {
            return false;
        }
        let user_data_type_code = payload[7];
        match user_data_type_code {
            // 0x03 = cc_data (CEA-708 with embedded CEA-608)
            0x03 => {
                self.service_cea708 = true;
                self.service_cea608 = true;
                true
            }
            // 0x04 = bar_data
            0x04 => false,
            _ => false,
        }
    }

    pub fn snapshot(&self) -> Option<CaptionsStats> {
        if self.packet_count == 0 && self.last_seen_at.is_none() {
            return None;
        }
        let mut services = Vec::new();
        if self.service_cea608 {
            services.push("cea-608".into());
        }
        if self.service_cea708 {
            services.push("cea-708".into());
        }
        Some(CaptionsStats {
            present: self.is_present(),
            packet_count: self.packet_count,
            services,
        })
    }
}
