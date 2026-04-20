// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TS continuity fixer for seamless input switching.
//!
//! When a flow switches between active inputs, external receivers can lose
//! lock due to continuity counter (CC) jumps, PCR discontinuities, and
//! delayed PAT/PMT. This module sits in the input forwarder path and:
//!
//! 1. **Before first switch**: zero overhead — packets pass through unchanged.
//! 2. **On switch**: clears output-side CC state and injects the new input's
//!    cached PAT/PMTs for fast receiver re-acquisition, with their
//!    `version_number` rewritten from a per-fixer **monotonic counter**
//!    that advances on every switch (see [`TsContinuityFixer::on_switch`]
//!    for the motivation — the alternative `cached_version + 1` stamping
//!    was empirically broken for the `A → B → A` round-trip case). The
//!    CC state reset produces a natural CC jump on all PIDs, which
//!    receivers (VLC, ffplay, hardware decoders) detect as "packet loss"
//!    and handle via their standard recovery path (flush PES buffers,
//!    resync on next PUSI).
//! 3. **After switch**: rewrites CC values within the new input's packet
//!    sequence so that subsequent packets maintain continuity.
//!
//! All packets — video, audio, data — are forwarded immediately after a
//! switch. There is no keyframe gating: decoders handle the transition via
//! the CC jump (PES buffer flush) and pick up the new stream at the next
//! PES start. This keeps the fixer codec-agnostic and works unchanged for
//! H.264, HEVC, JPEG XS, SMPTE 2110 uncompressed, or any future format.
//!
//! The fixer is shared (via `Arc<Mutex<..>>`) across all per-input forwarders
//! within a single flow. Only the active forwarder calls [`process_packet`];
//! passive forwarders call [`observe_passive`] to pre-warm the PSI cache.
//! Dead-input periods — when the operator has switched to an input whose
//! source isn't currently transmitting — are handled at the forwarder
//! level (see `spawn_input_forwarder` in `flow.rs`), which emits 250 ms-
//! interval NULL-PID padding so downstream UDP sockets don't time out.

use std::collections::{HashMap, HashSet};

use bytes::Bytes;

use super::packet::RtpPacket;
use super::ts_parse::*;

/// Result of [`TsContinuityFixer::process_packet`].
pub enum ProcessResult {
    /// Forward the packet unchanged (hot path before first switch).
    Unchanged,
    /// Forward a rewritten version of the packet.
    Rewritten(Bytes),
}

#[cfg(test)]
impl ProcessResult {
    /// Unwrap a `Rewritten` variant, panicking otherwise.
    fn unwrap_rewritten(self) -> Bytes {
        match self {
            Self::Rewritten(b) => b,
            Self::Unchanged => panic!("expected Rewritten, got Unchanged"),
        }
    }
}

/// Per-PID state tracked on the output (downstream) side.
struct PidCcState {
    /// Last CC value emitted downstream.
    last_out_cc: u8,
}

/// Per-input PSI (PAT + PMTs) cache. Each input has its own cache so that
/// on switch we inject the *new* input's PSI, not whatever happened to be
/// cached last from any input.
struct InputPsiCache {
    /// Cached latest PAT packet (188 bytes) from this input.
    cached_pat: Option<[u8; TS_PACKET_SIZE]>,
    /// Cached latest PMT packets keyed by PMT PID from this input.
    cached_pmts: HashMap<u16, [u8; TS_PACKET_SIZE]>,
    /// PMT PIDs discovered from this input's most recent PAT.
    pmt_pids: HashSet<u16>,
}

impl InputPsiCache {
    fn new() -> Self {
        Self {
            cached_pat: None,
            cached_pmts: HashMap::new(),
            pmt_pids: HashSet::new(),
        }
    }

    /// Observe a single 188-byte TS packet for PSI (PAT/PMT) caching.
    fn observe_psi(&mut self, pkt: &[u8]) {
        let pid = ts_pid(pkt);

        if pid == PAT_PID && ts_pusi(pkt) {
            let mut cached = [0u8; TS_PACKET_SIZE];
            cached.copy_from_slice(pkt);
            self.cached_pat = Some(cached);

            // Extract PMT PIDs from the PAT.
            let pmt_pids: HashSet<u16> = parse_pat_pmt_pids(pkt).into_iter().collect();
            // Remove stale PMT cache entries for PIDs no longer in the PAT.
            self.cached_pmts.retain(|k, _| pmt_pids.contains(k));
            self.pmt_pids = pmt_pids;
        } else if self.pmt_pids.contains(&pid) && ts_pusi(pkt) {
            let mut cached = [0u8; TS_PACKET_SIZE];
            cached.copy_from_slice(pkt);
            self.cached_pmts.insert(pid, cached);
        }
    }
}

/// Tracks per-PID continuity counters and per-input PSI caches to ensure
/// seamless transitions when the active input changes.
pub struct TsContinuityFixer {
    /// Per-PID CC state keyed by PID (output-side, shared across all inputs).
    pid_state: HashMap<u16, PidCcState>,
    /// Per-input PSI cache, keyed by input ID.
    input_psi: HashMap<String, InputPsiCache>,
    /// Zero-cost gate: skip all processing until the first switch occurs.
    ever_switched: bool,
    /// Monotonic 5-bit PSI version counter used to stamp every phantom
    /// PAT/PMT emitted by [`on_switch`]. Each switch bumps this once so
    /// the version we hand to the receiver is **strictly different** from
    /// the one it saw last time — regardless of what version the cached
    /// packet originally carried. This fixes a bug where two different
    /// inputs' cached PSI (both with natural version=0) both bumped to
    /// version=1, so after `golf → reolink → golf` the second `version=1`
    /// phantom looked identical to the first and ffplay silently stopped
    /// re-parsing the PMT — leaving the audio decoder pointed at the
    /// previous input's format. Wrapping at 32 is safe because every
    /// consecutive switch still produces a *different* value (N vs N+1).
    next_psi_version: u8,
}

impl TsContinuityFixer {
    pub fn new() -> Self {
        Self {
            pid_state: HashMap::new(),
            input_psi: HashMap::new(),
            ever_switched: false,
            // Start at 0; the first `on_switch` bumps this to 1 before
            // stamping, matching the long-standing "bumped from cached
            // version 0 to 1" behaviour on the first switch.
            next_psi_version: 0,
        }
    }

    /// Return cached PAT + PMT packets suitable for a keepalive tick when
    /// the active input has no live data flowing. Preference order:
    ///
    /// 1. the active input's own cache (if it has one — typical when
    ///    the input has been live and just paused briefly);
    /// 2. any other input's cache (fallback when the active input is a
    ///    dead source — e.g. an RTP bind with nothing feeding it after
    ///    an operator flipped the switch to it).
    ///
    /// Stamped with the **current** monotonic version (not advanced), so
    /// the keepalive PSI doesn't force receivers to re-parse on every
    /// tick — it just reiterates the last-known-good structure so the
    /// decoder doesn't decide the stream has lost its PMT. The caller is
    /// expected to send these periodically when no natural packet has
    /// arrived recently.
    #[allow(dead_code)]
    pub fn keepalive_psi(&self, active_input_id: &str) -> Vec<RtpPacket> {
        let mut out = Vec::new();
        // First try the active input, then any other cached input.
        let source = self
            .input_psi
            .get(active_input_id)
            .filter(|c| c.cached_pat.is_some())
            .or_else(|| self.input_psi.values().find(|c| c.cached_pat.is_some()));
        let Some(cache) = source else {
            return out;
        };
        let stamp = self.next_psi_version;
        if let Some(mut pat) = cache.cached_pat {
            set_psi_version(&mut pat, stamp);
            out.push(RtpPacket {
                data: Bytes::copy_from_slice(&pat),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
            });
        }
        for mut pmt in cache.cached_pmts.values().copied() {
            set_psi_version(&mut pmt, stamp);
            out.push(RtpPacket {
                data: Bytes::copy_from_slice(&pmt),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
            });
        }
        out
    }

    /// Observe a packet from a passive (non-active) input to pre-warm the
    /// PAT/PMT cache. Called by passive forwarders so that when a switch
    /// occurs, the cached PSI is already available for injection.
    pub fn observe_passive(&mut self, input_id: &str, packet: &RtpPacket) {
        let psi = match self.input_psi.get_mut(input_id) {
            Some(psi) => psi,
            None => {
                self.input_psi.insert(input_id.to_string(), InputPsiCache::new());
                self.input_psi.get_mut(input_id).unwrap()
            }
        };
        let ts_data = strip_rtp_header(packet);
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_data.len() {
            let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }
            psi.observe_psi(pkt);
        }
    }

    /// Called when the active input changes. Clears output-side CC state
    /// so the new input's original CC values pass through, creating a
    /// natural CC jump that signals receivers to flush and resync. Also
    /// returns cached PSI packets (PAT + PMTs) from the *new* input for
    /// fast receiver re-acquisition.
    ///
    /// Every phantom PAT/PMT packet returned here is stamped with a
    /// **monotonically advancing** `version_number` taken from
    /// `next_psi_version` — not simply `cached_version + 1`. This is
    /// essential for the common case of switching back and forth between
    /// two inputs whose cached PSI both carry `version_number = 0`: with
    /// `+1`, both switches produce `version = 1` and ffplay treats the
    /// second phantom as "already seen, nothing to re-parse" — keeping its
    /// audio decoder pointed at the previous input's format and silently
    /// dropping audio from the returning input.
    ///
    /// The CC jump from clearing `pid_state` is the other half of the
    /// switch signal: receivers (VLC, ffplay, hardware decoders) detect it
    /// as "packet loss," flush PES buffers, and resync on the next PES
    /// start (PUSI=1). Many decoders handle CC jumps more robustly than
    /// mid-stream DI flags, and DI insertion on payload-only packets
    /// would corrupt ES/PES data.
    pub fn on_switch(&mut self, new_input_id: &str) -> Vec<RtpPacket> {
        self.ever_switched = true;

        // Clear all output-side CC state. When the new input's packets
        // arrive, process_packet() creates fresh entries using the new
        // input's CC values. This produces a natural CC jump on all PIDs
        // that the receiver detects and handles via its standard packet-
        // loss recovery path (flush PES, wait for PUSI).
        let old_pid_count = self.pid_state.len();
        self.pid_state.clear();

        // Advance the monotonic counter unconditionally — even if we end
        // up emitting nothing this switch. Otherwise switching through a
        // dead input (no PSI cache) would freeze the counter, and the
        // next "real" switch would reuse the previous stamp, triggering
        // the same "phantoms look identical" bug we're fixing for the
        // directly-consecutive case.
        self.next_psi_version = (self.next_psi_version + 1) & 0x1F;
        let stamp = self.next_psi_version;

        let mut injected = Vec::new();

        // Retrieve the PSI cache for the *incoming* input specifically.
        let (cached_pat, cached_pmts) = match self.input_psi.get(new_input_id) {
            Some(psi) => (
                psi.cached_pat,
                psi.cached_pmts.values().copied().collect::<Vec<_>>(),
            ),
            None => {
                tracing::warn!(
                    "TsContinuityFixer::on_switch('{new_input_id}'): no PSI cached for this input — \
                     PAT/PMT will not be injected (receiver must wait for stream PSI); \
                     monotonic counter advanced to {stamp} so the next switch produces a distinct version"
                );
                return injected;
            }
        };

        tracing::info!(
            "TsContinuityFixer::on_switch('{new_input_id}'): injecting PAT={}, PMTs={}, \
             version={stamp} (monotonic), cleared {old_pid_count} tracked PIDs",
            if cached_pat.is_some() { "yes" } else { "no" },
            cached_pmts.len(),
        );

        // Inject PAT from the new input's cache with the monotonic
        // version stamp.
        if let Some(mut pat) = cached_pat {
            set_psi_version(&mut pat, stamp);
            injected.push(RtpPacket {
                data: Bytes::copy_from_slice(&pat),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
            });
        }

        // Inject PMTs from the new input's cache with the same stamp.
        for mut pmt in cached_pmts {
            set_psi_version(&mut pmt, stamp);
            injected.push(RtpPacket {
                data: Bytes::copy_from_slice(&pmt),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
            });
        }

        injected
    }

    /// Process a packet from the currently active input.
    ///
    /// Returns [`ProcessResult::Unchanged`] on the hot path (before first
    /// switch), [`ProcessResult::Rewritten`] when CC/DI was modified.
    pub fn process_packet(&mut self, input_id: &str, packet: &RtpPacket) -> ProcessResult {
        if !self.ever_switched {
            // Hot path: no switch has ever occurred. Just observe PSI for
            // cache warmth and return None to forward unchanged.
            let psi = match self.input_psi.get_mut(input_id) {
                Some(psi) => psi,
                None => {
                    self.input_psi.insert(input_id.to_string(), InputPsiCache::new());
                    self.input_psi.get_mut(input_id).unwrap()
                }
            };
            let ts_data = strip_rtp_header(packet);
            let mut offset = 0;
            while offset + TS_PACKET_SIZE <= ts_data.len() {
                let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
                offset += TS_PACKET_SIZE;
                if pkt[0] != TS_SYNC_BYTE {
                    continue;
                }
                psi.observe_psi(pkt);
                // Also seed CC state so we have accurate last_out_cc.
                let pid = ts_pid(pkt);
                if pid != NULL_PID && ts_has_payload(pkt) {
                    let cc = ts_cc(pkt);
                    self.pid_state.entry(pid).or_insert(PidCcState {
                        last_out_cc: cc,
                    }).last_out_cc = cc;
                }
            }
            return ProcessResult::Unchanged;
        }

        // Post-switch: rewrite CC values for continuity.
        let ts_data = strip_rtp_header(packet);
        if ts_data.is_empty() {
            return ProcessResult::Unchanged;
        }

        let rtp_header_len = if packet.is_raw_ts {
            0
        } else {
            packet.data.len() - ts_data.len()
        };

        let mut out = Vec::with_capacity(packet.data.len());
        // Copy the RTP header unchanged if present.
        if rtp_header_len > 0 {
            out.extend_from_slice(&packet.data[..rtp_header_len]);
        }

        // Ensure this input has a PSI cache entry (allocates only on first
        // packet from this input — subsequent packets use get_mut).
        if !self.input_psi.contains_key(input_id) {
            self.input_psi.insert(input_id.to_string(), InputPsiCache::new());
        }

        let mut modified = false;
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_data.len() {
            let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                out.extend_from_slice(pkt);
                continue;
            }

            let pid = ts_pid(pkt);
            if pid == NULL_PID {
                out.extend_from_slice(pkt);
                continue;
            }

            // Observe PSI for cache updates (routed to this input's cache).
            // Borrow is scoped so it doesn't conflict with pid_state below.
            self.input_psi.get_mut(input_id).unwrap().observe_psi(pkt);

            if !ts_has_payload(pkt) {
                // Adaptation-only packets: CC does not increment per spec.
                // Pass through unchanged.
                out.extend_from_slice(pkt);
                continue;
            }

            // Payload-bearing packet: rewrite CC to maintain continuity.
            // After a switch, pid_state is empty (cleared by on_switch),
            // so the first packet of each PID creates a fresh entry using
            // the new input's CC. This produces a natural CC jump that
            // receivers detect and handle via packet-loss recovery.
            let mut ts_pkt = [0u8; TS_PACKET_SIZE];
            ts_pkt.copy_from_slice(pkt);

            let state = self.pid_state.entry(pid).or_insert(PidCcState {
                last_out_cc: ts_cc(pkt).wrapping_sub(1) & 0x0F,
            });

            // Rewrite CC to continue from last emitted value.
            let new_cc = state.last_out_cc.wrapping_add(1) & 0x0F;
            ts_pkt[3] = (ts_pkt[3] & 0xF0) | new_cc;
            state.last_out_cc = new_cc;
            modified = true;

            out.extend_from_slice(&ts_pkt);
        }

        // Append any trailing bytes that don't form a complete TS packet.
        if offset < ts_data.len() {
            out.extend_from_slice(&ts_data[offset..]);
        }

        if modified {
            ProcessResult::Rewritten(Bytes::from(out))
        } else {
            ProcessResult::Unchanged
        }
    }
}

/// Overwrite the `version_number` field in a PSI section (PAT or PMT)
/// carried in a single 188-byte TS packet with PUSI=1, then recompute
/// the CRC32. Forces receivers that cache tables by version to re-parse
/// the section — essential when switching between inputs that use the
/// same version number but have different content (e.g. different codecs
/// in the PMT).
///
/// Layout (for PUSI=1, pointer_field=0):
///   byte 4:  pointer_field (0x00)
///   byte 5:  table_id
///   byte 6-7: section_syntax_indicator + section_length
///   byte 8-9: transport_stream_id (PAT) or program_number (PMT)
///   byte 10: reserved(2) + version_number(5) + current_next_indicator(1)
///   ...
///   last 4 bytes of section: CRC32
///
/// `version` is masked to 5 bits and written in place; `current_next`
/// and the two reserved bits are preserved.
fn set_psi_version(pkt: &mut [u8; TS_PACKET_SIZE], version: u8) {
    // Verify this is a PUSI packet.
    if !ts_pusi(pkt) {
        return;
    }

    let pointer_field = pkt[4] as usize;
    let section_start = 5 + pointer_field;

    // Need at least: table_id(1) + section_length(2) + header(5) + CRC(4) = 12
    if section_start + 12 > TS_PACKET_SIZE {
        return;
    }

    // Parse section_length (12 bits after section_syntax_indicator).
    let section_length =
        (((pkt[section_start + 1] & 0x0F) as usize) << 8) | (pkt[section_start + 2] as usize);

    // Total section bytes = table_id(1) + 2 (indicator+length) + section_length
    let section_end = section_start + 3 + section_length;
    if section_end > TS_PACKET_SIZE || section_length < 9 {
        return; // Malformed or too short for version + CRC
    }

    // Write version_number (5 bits at offset +5 from section_start, bits [5:1]).
    let version_byte = &mut pkt[section_start + 5];
    let v = version & 0x1F;
    *version_byte = (*version_byte & 0xC1) | (v << 1);

    // Recalculate CRC32 over the section body (excluding the CRC itself).
    let crc_offset = section_end - 4;
    let crc = mpeg2_crc32(&pkt[section_start..crc_offset]);
    pkt[crc_offset] = (crc >> 24) as u8;
    pkt[crc_offset + 1] = (crc >> 16) as u8;
    pkt[crc_offset + 2] = (crc >> 8) as u8;
    pkt[crc_offset + 3] = crc as u8;
}

/// Back-compat wrapper: bump the `version_number` by 1 modulo 32 and fix
/// the CRC. Retained for any callers / tests that want the historical
/// "increment-from-current" behaviour. New production code on the switch
/// path uses [`set_psi_version`] with the monotonic counter instead.
#[cfg(test)]
fn bump_psi_version(pkt: &mut [u8; TS_PACKET_SIZE]) {
    if !ts_pusi(pkt) {
        return;
    }
    let pointer_field = pkt[4] as usize;
    let section_start = 5 + pointer_field;
    if section_start + 12 > TS_PACKET_SIZE {
        return;
    }
    let section_length =
        (((pkt[section_start + 1] & 0x0F) as usize) << 8) | (pkt[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > TS_PACKET_SIZE || section_length < 9 {
        return;
    }
    let old_version = (pkt[section_start + 5] >> 1) & 0x1F;
    set_psi_version(pkt, (old_version + 1) & 0x1F);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test helpers ────────────────────────────────────────────────────

    /// Build a 188-byte TS packet with payload, given PID and CC.
    fn build_ts_packet(pid: u16, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F); // AFC=01 (payload only), CC
        pkt
    }

    /// Build a synthetic PAT TS packet.
    fn build_pat(programs: &[(u16, u16)], cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0
        pkt[2] = 0x00;
        pkt[3] = 0x10 | (cc & 0x0F);
        pkt[4] = 0x00; // pointer_field
        let section_length = 5 + 4 * programs.len() + 4;
        pkt[5] = 0x00; // table_id = PAT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        let mut pos = 13;
        for (program_number, pmt_pid) in programs {
            pkt[pos] = (program_number >> 8) as u8;
            pkt[pos + 1] = (program_number & 0xFF) as u8;
            pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
            pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
            pos += 4;
        }
        let crc = mpeg2_crc32(&pkt[5..pos]);
        pkt[pos] = (crc >> 24) as u8;
        pkt[pos + 1] = (crc >> 16) as u8;
        pkt[pos + 2] = (crc >> 8) as u8;
        pkt[pos + 3] = crc as u8;
        pkt
    }

    /// Build a synthetic PMT TS packet.
    fn build_pmt(pmt_pid: u16, streams: &[(u8, u16)], cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F); // PUSI=1
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        pkt[4] = 0x00; // pointer_field
        let pcr_pid = streams.first().map(|(_, p)| *p).unwrap_or(0x1FFF);
        let section_length = 9 + 5 * streams.len() + 4;
        pkt[5] = 0x02; // table_id = PMT
        pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
        pkt[14] = (pcr_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        let mut pos = 17;
        for (stream_type, es_pid) in streams {
            pkt[pos] = *stream_type;
            pkt[pos + 1] = 0xE0 | (((es_pid >> 8) as u8) & 0x1F);
            pkt[pos + 2] = (es_pid & 0xFF) as u8;
            pkt[pos + 3] = 0xF0;
            pkt[pos + 4] = 0x00;
            pos += 5;
        }
        pkt
    }

    fn make_rtp_packet(ts_data: &[u8]) -> RtpPacket {
        RtpPacket {
            data: Bytes::copy_from_slice(ts_data),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────

    #[test]
    fn no_switch_returns_none() {
        let mut fixer = TsContinuityFixer::new();
        let ts = build_ts_packet(0x100, 0);
        let pkt = make_rtp_packet(&ts);
        assert!(matches!(fixer.process_packet("a", &pkt), ProcessResult::Unchanged));
    }

    #[test]
    fn cc_passes_through_after_switch() {
        let mut fixer = TsContinuityFixer::new();

        // Simulate input A: PID 0x100, CC=5,6,7
        for cc in 5..=7u8 {
            let ts = build_ts_packet(0x100, cc);
            fixer.process_packet("a", &make_rtp_packet(&ts));
        }

        // Switch to input B — pid_state is cleared.
        let _ = fixer.on_switch("b");

        // Input B sends PID 0x100 with CC=0 (its own sequence).
        // After pid_state clear, the first packet passes through with
        // its original CC, creating a CC jump (7 → 0) that the receiver
        // detects as packet loss and handles by flushing/resyncing.
        let ts = build_ts_packet(0x100, 0);
        let result = fixer.process_packet("b", &make_rtp_packet(&ts)).unwrap_rewritten();
        assert_eq!(ts_cc(&result[0..TS_PACKET_SIZE]), 0);

        // Second packet maintains continuity within B's sequence.
        let ts = build_ts_packet(0x100, 1);
        let result = fixer.process_packet("b", &make_rtp_packet(&ts)).unwrap_rewritten();
        assert_eq!(ts_cc(&result[0..TS_PACKET_SIZE]), 1);
    }

    #[test]
    fn cc_continuity_within_input_after_switch() {
        let mut fixer = TsContinuityFixer::new();

        let ts = build_ts_packet(0x100, 14);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        let _ = fixer.on_switch("b");

        // Input B: CC wraps around within its own sequence.
        let ts1 = build_ts_packet(0x100, 14);
        let result1 = fixer.process_packet("b", &make_rtp_packet(&ts1)).unwrap_rewritten();
        assert_eq!(ts_cc(&result1[..TS_PACKET_SIZE]), 14);

        let ts2 = build_ts_packet(0x100, 15);
        let result2 = fixer.process_packet("b", &make_rtp_packet(&ts2)).unwrap_rewritten();
        assert_eq!(ts_cc(&result2[..TS_PACKET_SIZE]), 15);

        let ts3 = build_ts_packet(0x100, 0);
        let result3 = fixer.process_packet("b", &make_rtp_packet(&ts3)).unwrap_rewritten();
        assert_eq!(ts_cc(&result3[..TS_PACKET_SIZE]), 0); // wraps
    }

    #[test]
    fn pid_state_cleared_on_switch() {
        let mut fixer = TsContinuityFixer::new();

        // Input A: two PIDs.
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 5)));
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x200, 10)));
        assert_eq!(fixer.pid_state.len(), 2);

        let _ = fixer.on_switch("b");

        // pid_state should be empty after switch.
        assert!(fixer.pid_state.is_empty(), "pid_state should be cleared on switch");
    }

    #[test]
    fn pat_pmt_cached_from_passive_input() {
        let mut fixer = TsContinuityFixer::new();

        // Active input sends some data so we have CC state.
        let ts = build_ts_packet(0x100, 0);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        // Passive input sends PAT + PMT.
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);

        let mut passive_data = Vec::new();
        passive_data.extend_from_slice(&pat);
        passive_data.extend_from_slice(&pmt);
        fixer.observe_passive("b", &make_rtp_packet(&passive_data));

        let psi = fixer.input_psi.get("b").unwrap();
        assert!(psi.cached_pat.is_some());
        assert!(psi.cached_pmts.contains_key(&0x1000));
    }

    #[test]
    fn on_switch_injects_pat_and_pmts() {
        let mut fixer = TsContinuityFixer::new();

        // Feed PAT + PMT through passive input B.
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt);
        fixer.observe_passive("b", &make_rtp_packet(&psi_data));

        // Feed some data through active input A.
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        // Switch to B.
        let injected = fixer.on_switch("b");

        // Should get PAT + PMT from input B (with original CC, no rewrite).
        assert_eq!(injected.len(), 2, "expected PAT + PMT injection");
        assert_eq!(ts_pid(&injected[0].data), PAT_PID);
        assert_eq!(ts_pid(&injected[1].data), 0x1000);
    }

    #[test]
    fn pat_injected_from_new_input_not_old() {
        let mut fixer = TsContinuityFixer::new();

        // Input A uses PMT PID 0x1000.
        let pat_a = build_pat(&[(1, 0x1000)], 0);
        fixer.process_packet("a", &make_rtp_packet(&pat_a));

        // Input B uses PMT PID 0x1100 (different PID structure).
        let pat_b = build_pat(&[(1, 0x1100)], 0);
        fixer.observe_passive("b", &make_rtp_packet(&pat_b));

        // Switch to B — should inject B's PAT (with PMT PID 0x1100).
        let injected = fixer.on_switch("b");
        assert!(!injected.is_empty(), "should inject at least PAT");

        let pmt_pids = parse_pat_pmt_pids(&injected[0].data);
        assert_eq!(pmt_pids, vec![0x1100], "injected PAT should reference new input's PMT PID");
    }

    #[test]
    fn switch_with_no_cached_psi() {
        let mut fixer = TsContinuityFixer::new();

        let ts = build_ts_packet(0x100, 0);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        // Switch to B — no PSI cached.
        let injected = fixer.on_switch("b");
        assert!(injected.is_empty(), "should inject nothing when no PSI cached for new input");

        // pid_state should still be cleared.
        assert!(fixer.pid_state.is_empty());
    }

    #[test]
    fn multiple_pmts_injected_on_switch() {
        let mut fixer = TsContinuityFixer::new();

        let pat = build_pat(&[(1, 0x1000), (2, 0x1001)], 0);
        let pmt1 = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let pmt2 = build_pmt(0x1001, &[(0x0F, 0x200)], 0);

        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt1);
        psi_data.extend_from_slice(&pmt2);
        fixer.observe_passive("b", &make_rtp_packet(&psi_data));

        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 3, "expected PAT + 2 PMTs");
        assert_eq!(ts_pid(&injected[0].data), PAT_PID);
        let mut pmt_pids: Vec<u16> = injected[1..].iter().map(|p| ts_pid(&p.data)).collect();
        pmt_pids.sort();
        assert_eq!(pmt_pids, vec![0x1000, 0x1001]);
    }

    #[test]
    fn multiple_switches_reset_cc_each_time() {
        let mut fixer = TsContinuityFixer::new();

        // Input A: PID 0x100, CC=0..4
        for cc in 0..=4u8 {
            fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, cc)));
        }

        // Switch to B — pid_state cleared. B's CC passes through.
        let _ = fixer.on_switch("b");
        let result = fixer.process_packet("b", &make_rtp_packet(&build_ts_packet(0x100, 10))).unwrap_rewritten();
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 10);

        let result = fixer.process_packet("b", &make_rtp_packet(&build_ts_packet(0x100, 11))).unwrap_rewritten();
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 11);

        // Switch back to A — pid_state cleared again. A's CC passes through.
        let _ = fixer.on_switch("a");
        let result = fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0))).unwrap_rewritten();
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 0);
    }

    #[test]
    fn multi_packet_datagram() {
        let mut fixer = TsContinuityFixer::new();

        // Input A: 3 packets for PID 0x100, CC=10,11,12
        let mut data = Vec::new();
        for cc in 10..=12u8 {
            data.extend_from_slice(&build_ts_packet(0x100, cc));
        }
        fixer.process_packet("a", &make_rtp_packet(&data));

        // Switch — pid_state cleared.
        let _ = fixer.on_switch("b");

        // Input B: 3 packets in one datagram. CC passes through.
        let mut data_b = Vec::new();
        for cc in 0..=2u8 {
            data_b.extend_from_slice(&build_ts_packet(0x100, cc));
        }
        let result = fixer.process_packet("b", &make_rtp_packet(&data_b)).unwrap_rewritten();

        for (i, expected_cc) in [0u8, 1, 2].iter().enumerate() {
            let pkt = &result[i * TS_PACKET_SIZE..(i + 1) * TS_PACKET_SIZE];
            assert_eq!(ts_cc(pkt), *expected_cc, "packet {i} CC mismatch");
        }
    }

    #[test]
    fn rtp_wrapped_packet_preserved() {
        let mut fixer = TsContinuityFixer::new();

        // Build an RTP-wrapped TS packet.
        let mut rtp_data = vec![0u8; RTP_HEADER_MIN_SIZE];
        rtp_data[0] = 0x80;
        rtp_data[1] = 33;
        let ts = build_ts_packet(0x100, 5);
        rtp_data.extend_from_slice(&ts);

        let pkt = RtpPacket {
            data: Bytes::from(rtp_data.clone()),
            sequence_number: 42,
            rtp_timestamp: 1000,
            recv_time_us: 500,
            is_raw_ts: false,
        };

        assert!(matches!(fixer.process_packet("a", &pkt), ProcessResult::Unchanged));

        let _ = fixer.on_switch("b");

        // New packet from input B, RTP-wrapped.
        let mut rtp_data2 = vec![0u8; RTP_HEADER_MIN_SIZE];
        rtp_data2[0] = 0x80;
        rtp_data2[1] = 33;
        let ts2 = build_ts_packet(0x100, 0);
        rtp_data2.extend_from_slice(&ts2);

        let pkt2 = RtpPacket {
            data: Bytes::from(rtp_data2),
            sequence_number: 100,
            rtp_timestamp: 2000,
            recv_time_us: 600,
            is_raw_ts: false,
        };

        let result = fixer.process_packet("b", &pkt2).unwrap_rewritten();
        // RTP header should be preserved.
        assert_eq!(result[0], 0x80);
        assert_eq!(result[1], 33);
        // CC passes through from new input (pid_state was cleared).
        let ts_start = RTP_HEADER_MIN_SIZE;
        assert_eq!(ts_cc(&result[ts_start..ts_start + TS_PACKET_SIZE]), 0);
    }

    #[test]
    fn new_pid_after_switch_gets_natural_cc() {
        let mut fixer = TsContinuityFixer::new();

        let ts = build_ts_packet(0x100, 3);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        let _ = fixer.on_switch("b");

        // Input B has PID 0x200. CC passes through.
        let ts = build_ts_packet(0x200, 7);
        let result = fixer.process_packet("b", &make_rtp_packet(&ts)).unwrap_rewritten();
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 7);
    }

    #[test]
    fn all_pids_forwarded_immediately_after_switch() {
        let mut fixer = TsContinuityFixer::new();

        // Feed PAT + PMT declaring video PID 0x100 and audio PID 0x101.
        let pat = build_pat(&[(1, 0x1000)], 0);
        fixer.process_packet("a", &make_rtp_packet(&pat));
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100), (0x0F, 0x101)], 0);
        fixer.process_packet("a", &make_rtp_packet(&pmt));

        for cc in 0..3u8 {
            fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, cc)));
            fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x101, cc)));
        }

        let _ = fixer.on_switch("b");

        // Video packet passes through immediately — no keyframe wait.
        let v = build_ts_packet(0x100, 0);
        let result = fixer.process_packet("b", &make_rtp_packet(&v)).unwrap_rewritten();
        assert_eq!(ts_pid(&result[..TS_PACKET_SIZE]), 0x100);

        // Audio packet also passes through immediately.
        let a = build_ts_packet(0x101, 0);
        let result = fixer.process_packet("b", &make_rtp_packet(&a)).unwrap_rewritten();
        assert_eq!(ts_pid(&result[..TS_PACKET_SIZE]), 0x101);
    }

    // ── PSI version bump tests ─────────────────────────────────────────

    #[test]
    fn bump_psi_version_increments_and_fixes_crc() {
        let mut pat = build_pat(&[(1, 0x1000)], 0);
        // Original version is 0 (set in build_pat: byte 10 = 0xC1 → version 0).
        let orig_version = (pat[10] >> 1) & 0x1F;
        assert_eq!(orig_version, 0);

        bump_psi_version(&mut pat);

        let new_version = (pat[10] >> 1) & 0x1F;
        assert_eq!(new_version, 1, "version should be bumped to 1");

        // CRC should be valid (mpeg2_crc32 over full section = 0).
        let section_length = (((pat[6] & 0x0F) as usize) << 8) | (pat[7] as usize);
        let section_end = 5 + 3 + section_length; // pointer_field(1) + table_id(1) + 2 + section_length
        let crc_check = mpeg2_crc32(&pat[5..section_end]);
        assert_eq!(crc_check, 0, "CRC should be valid after version bump");
    }

    #[test]
    fn injected_psi_has_bumped_version() {
        let mut fixer = TsContinuityFixer::new();

        // Input B has PAT + PMT with version 0.
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt);
        fixer.observe_passive("b", &make_rtp_packet(&psi_data));

        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 2);

        // PAT version should be bumped from 0 to 1.
        let pat_version = (injected[0].data[10] >> 1) & 0x1F;
        assert_eq!(pat_version, 1, "injected PAT should have bumped version");

        // PMT version should also be bumped from 0 to 1.
        // PMT has PUSI=1, pointer_field=0, so version is also at byte 10.
        let pmt_version = (injected[1].data[10] >> 1) & 0x1F;
        assert_eq!(pmt_version, 1, "injected PMT should have bumped version");
    }

    #[test]
    fn version_bump_wraps_at_31() {
        // Build PAT with version 31 (max 5-bit value).
        let mut pat = build_pat(&[(1, 0x1000)], 0);
        pat[10] = (pat[10] & 0xC1) | (31 << 1); // set version to 31
        // Fix CRC after manual version change.
        let section_length = (((pat[6] & 0x0F) as usize) << 8) | (pat[7] as usize);
        let section_end = 5 + 3 + section_length;
        let crc = mpeg2_crc32(&pat[5..section_end - 4]);
        pat[section_end - 4] = (crc >> 24) as u8;
        pat[section_end - 3] = (crc >> 16) as u8;
        pat[section_end - 2] = (crc >> 8) as u8;
        pat[section_end - 1] = crc as u8;

        bump_psi_version(&mut pat);
        let new_version = (pat[10] >> 1) & 0x1F;
        assert_eq!(new_version, 0, "version should wrap from 31 to 0");
    }

    /// Regression: two different inputs' cached PSI that both carry the
    /// same natural `version_number` (both 0, which is the case for every
    /// ffmpeg/srt-live-transmit-generated stream we test against) used to
    /// both bump to `version = 1`. After a `A → B → A` round-trip, the
    /// second phantom looked identical to the first and ffplay silently
    /// kept using B's PMT for A's stream — losing audio permanently.
    ///
    /// With the monotonic counter, consecutive switches always produce
    /// distinct version numbers even when the cached starting values
    /// collide.
    #[test]
    fn monotonic_version_differs_across_consecutive_switches() {
        let mut fixer = TsContinuityFixer::new();

        // Both inputs observe a PAT with natural version=0.
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt);
        fixer.observe_passive("a", &make_rtp_packet(&psi_data));
        fixer.observe_passive("b", &make_rtp_packet(&psi_data));

        // Drive a packet so ever_switched can be set later.
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        // First switch: a → b. Phantom PAT should carry version=1.
        let injected = fixer.on_switch("b");
        assert!(!injected.is_empty(), "first switch should inject PSI");
        let v1 = (injected[0].data[10] >> 1) & 0x1F;
        assert_eq!(v1, 1, "first phantom PAT version");

        // Second switch: b → a. Under the old +1 scheme both phantoms
        // would be version=1 — receivers ignored the second one. With the
        // monotonic counter, the second switch must produce a DIFFERENT
        // version so receivers always re-parse.
        let injected = fixer.on_switch("a");
        assert!(!injected.is_empty(), "second switch should inject PSI");
        let v2 = (injected[0].data[10] >> 1) & 0x1F;
        assert_ne!(
            v2, v1,
            "consecutive-switch phantom PATs must carry different version_numbers"
        );
        assert_eq!(v2, 2, "monotonic counter advances by 1 per switch");

        // Third switch back to b. Still distinct from the prior two.
        let injected = fixer.on_switch("b");
        let v3 = (injected[0].data[10] >> 1) & 0x1F;
        assert_ne!(v3, v2);
        assert_ne!(v3, v1);
        assert_eq!(v3, 3);

        // Every PMT emitted in a given switch shares the same monotonic
        // stamp as the PAT — one coherent version per switch event.
        assert_eq!(
            (injected[1].data[10] >> 1) & 0x1F,
            v3,
            "PMT must carry the same monotonic version as the PAT from the same switch",
        );
    }

    /// Switching through an input that has no cached PSI (a "dead"
    /// source with nothing feeding it) must still advance the monotonic
    /// counter. Otherwise `live → dead → live` would emit two phantoms
    /// with the same version stamp — exactly the collision this fix
    /// exists to prevent.
    #[test]
    fn dead_input_still_advances_monotonic_counter() {
        let mut fixer = TsContinuityFixer::new();

        // Only input "a" ever observes PSI. Inputs "dead1" and "dead2"
        // are never cached.
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt);
        fixer.observe_passive("a", &make_rtp_packet(&psi_data));

        // Trigger first switch.
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        // a → dead1: no phantom (no cache) but counter should bump.
        let injected = fixer.on_switch("dead1");
        assert!(injected.is_empty(), "no cached PSI → no phantom");

        // dead1 → a: phantom stamped with the NEXT version, not a stale
        // reuse of the previous "a" session's stamp.
        let injected = fixer.on_switch("a");
        assert!(!injected.is_empty());
        let v1 = (injected[0].data[10] >> 1) & 0x1F;
        assert_eq!(v1, 2, "counter advanced twice: once for dead1, once for a");

        // a → dead2: again, counter bumps even with empty phantom.
        let injected = fixer.on_switch("dead2");
        assert!(injected.is_empty());

        // dead2 → a: fourth bump, distinct from v1.
        let injected = fixer.on_switch("a");
        let v2 = (injected[0].data[10] >> 1) & 0x1F;
        assert_ne!(v2, v1, "returning to 'a' after a dead detour must use a fresh version");
        assert_eq!(v2, 4);
    }

    /// The monotonic counter wraps at 32 without going stale: after 32
    /// bumps we're back at 0 but the next switch still produces a value
    /// different from the *immediately previous* switch, which is what
    /// receivers care about.
    #[test]
    fn monotonic_version_wraps_cleanly_and_stays_distinct() {
        let mut fixer = TsContinuityFixer::new();
        let pat = build_pat(&[(1, 0x1000)], 0);
        let pmt = build_pmt(0x1000, &[(0x1B, 0x100)], 0);
        let mut psi_data = Vec::new();
        psi_data.extend_from_slice(&pat);
        psi_data.extend_from_slice(&pmt);
        fixer.observe_passive("x", &make_rtp_packet(&psi_data));

        let mut prev: Option<u8> = None;
        for i in 1..=33u32 {
            let injected = fixer.on_switch("x");
            assert!(!injected.is_empty());
            let v = (injected[0].data[10] >> 1) & 0x1F;
            if let Some(p) = prev {
                assert_ne!(v, p, "switch #{i}: version must differ from previous");
            }
            prev = Some(v);
        }
        // After 33 switches the counter has wrapped (started at 0, bumped
        // to 1..=32 mod 32 = 0 on the 32nd bump, then to 1 on the 33rd).
        assert_eq!(prev, Some(1));
    }
}
