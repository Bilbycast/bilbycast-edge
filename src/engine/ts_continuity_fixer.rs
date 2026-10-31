// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TS continuity fixer for seamless input switching.
//!
//! When a flow switches between active inputs, external receivers can lose
//! lock due to continuity counter (CC) jumps, PCR discontinuities, and
//! delayed PAT/PMT. This module sits in the input forwarder path and:
//!
//! 1. **Outside a pending DI**: zero overhead — packets pass through
//!    unchanged (PSI observed for the switch cache).
//! 2. **On switch**: lets the new input's CCs through (one CC jump per
//!    PID) and injects the new input's cached PAT/PMTs for fast receiver
//!    re-acquisition, with their `version_number` rewritten from a
//!    per-fixer **monotonic counter**
//!    that advances on every switch (see [`TsContinuityFixer::on_switch`]
//!    for the motivation — the alternative `cached_version + 1` stamping
//!    was empirically broken for the `A → B → A` round-trip case). The
//!    new input's own CCs produce a natural CC jump on all PIDs, which
//!    receivers (VLC, ffplay, hardware decoders) detect as "packet loss"
//!    and handle via their standard recovery path (flush PES buffers,
//!    resync on next PUSI). The fixer also arms a one-shot
//!    `pending_di_on_pcr` flag — the next PCR-bearing TS packet from the
//!    new input has `discontinuity_indicator` (AF flags 0x80) set, as has
//!    the first PCR on every other PCR PID its cached PMTs name (each
//!    program of an MPTS has a clock of its own), so
//!    receivers see the spec-defined "next PCR is a fresh STC anchor"
//!    signal instead of treating the post-switch PCR-epoch jump as a
//!    clock fault. CC alone is empirically not enough for receivers that
//!    strictly track PCR continuity (broadcast-grade decoders, VLC,
//!    ffplay) when inputs A and B carry different PTS/PCR epochs — the
//!    common case for live-camera switching, test-pattern ↔ media-player,
//!    or any per-input transcoder pipeline.
//! 3. **After switch**: CC passes through untouched, so loss on the new
//!    input still shows downstream and in the flow's TR 101 290
//!    `cc_errors`. The fixer never rewrites a CC: the only bit it ever
//!    changes in a media packet is the DI on the PCR that takes a pending
//!    one. (It used to renumber every payload CC after the first switch or
//!    source PCR jump, for the rest of the flow, which erased every later
//!    gap — lost packets, legal duplicates, a source's own CC reset —
//!    edge#214.)
//!
//! All packets — video, audio, data — are forwarded immediately after a
//! switch. There is no keyframe gating: decoders handle the transition via
//! the CC jump (PES buffer flush) and pick up the new stream at the next
//! PES start. This keeps the fixer codec-agnostic and works unchanged for
//! H.264, HEVC, JPEG XS, SMPTE 2110 uncompressed, or any future format.
//! A non-TS payload (ST 2110-30/-31/-40, `rtp_audio`, MXL audio without
//! `audio_encode`) never reaches it: the fixer task forwards such an
//! input's datagrams as they came (`flow::FixerCommand::OpaquePacket`).
//! Read in 188-byte strides, PCM or ANC bytes that happened to start 0x47
//! and parse as a PCR used to take a pending DI bit, and a cached "PAT" of
//! such noise could be injected at the next switch.
//!
//! The fixer is owned by the per-flow fixer task (`flow::ts_fixer_task`),
//! which every per-input forwarder of the flow feeds over a channel.
//! Packets of the active input go through [`process_packet`]; those of
//! passive inputs go to [`observe_passive`] to pre-warm the PSI cache. The
//! task also keeps the flow's broadcast in one TS framing — RTP-wrapped or
//! raw — whichever inputs it switches between ([`FlowFraming`]), and sends
//! a switch's PSI in the framing of the datagram it precedes
//! ([`frame_switch_psi`]).
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
    /// Forward the packet unchanged: every packet except one that carries
    /// the PCR taking a pending DI.
    Unchanged,
    /// Forward this copy of the datagram instead: byte for byte the one
    /// received, with the DI bit set on the PCR that took a pending one.
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

    /// The bytes the fixer task forwards for `pkt`.
    fn forwarded(self, pkt: &RtpPacket) -> Bytes {
        match self {
            Self::Rewritten(b) => b,
            Self::Unchanged => pkt.data.clone(),
        }
    }
}

/// Per-input PSI (PAT + PMTs) cache. Each input has its own cache so that
/// on switch we inject the *new* input's PSI, not whatever happened to be
/// cached last from any input.
struct InputPsiCache {
    /// Cached latest PAT packet (188 bytes) from this input.
    cached_pat: Option<[u8; TS_PACKET_SIZE]>,
    /// Cached latest complete PMT-PID unit (every packet of it — a PMT may
    /// span packets) keyed by PMT PID from this input.
    cached_pmts: HashMap<u16, Vec<[u8; TS_PACKET_SIZE]>>,
    /// Unit being collected per PMT PID (see [`PmtUnitCollector`]).
    pmt_pending: HashMap<u16, PmtUnitCollector>,
    /// PMT PIDs discovered from this input's most recent PAT.
    pmt_pids: HashSet<u16>,
}

impl InputPsiCache {
    fn new() -> Self {
        Self {
            cached_pat: None,
            cached_pmts: HashMap::new(),
            pmt_pending: HashMap::new(),
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
            self.pmt_pending.retain(|k, _| pmt_pids.contains(k));
            self.pmt_pids = pmt_pids;
        } else if self.pmt_pids.contains(&pid) {
            // Collect the whole unit: a PMT that spans packets is only
            // useful to inject complete. The unit replaces the cached one
            // only when a PMT section with a valid CRC completed in it and
            // nothing aborted it (`PmtUnitCollector`).
            if let Some(unit) = self.pmt_pending.entry(pid).or_default().push_packet(pkt, |_| {}) {
                self.cached_pmts.insert(pid, unit);
            }
        }
    }
}

/// Keeps per-input PSI caches and the pending discontinuity indicators that
/// make transitions seamless when the active input changes. CC is never
/// tracked or rewritten.
pub struct TsContinuityFixer {
    /// Per-input PSI cache, keyed by input ID.
    input_psi: HashMap<String, InputPsiCache>,
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
    /// One-shot flag: set by [`on_switch`], cleared the next time
    /// [`process_packet`] sees a PCR-bearing TS packet, on any PID (the
    /// switch also arms every PCR PID the new input's cached PMTs name in
    /// `pending_di_pids`, so on an MPTS each program's first PCR after the
    /// cut takes one) — at which point
    /// the `discontinuity_indicator` bit (AF flags 0x80) is OR'd into
    /// that packet's adaptation field, the only change the fixer makes to
    /// a media packet (its CC goes out as the source sent it). While it,
    /// or an entry of `pending_di_pids`, is armed, `process_packet` reads
    /// each packet's PCR; otherwise it reads nothing but PSI. Tells the
    /// receiver "the next PCR is a fresh STC anchor, throw away your old
    /// timestamp tracking" — without it, professional and prosumer
    /// decoders (VLC, ffplay, Appear-style hardware) treat the post-switch
    /// PCR jump (which happens whenever inputs A and B have different
    /// epochs — the common case for live-camera switching, test-pattern ↔
    /// media-player, or any per-input transcoder pipeline) as a clock
    /// fault and lock up. Set on every switch (including dead-input
    /// switches) so the next real packet picks it up regardless of which
    /// input finally produces data first.
    pending_di_on_pcr: bool,
    /// PCR PIDs whose next PCR must carry DI — a source discontinuity the
    /// watcher saw on that PID's own clock, or a switch to an input whose
    /// cached PMTs name that PID as a PCR_PID. On an MPTS each program has
    /// its own PCR, so the DI belongs on each program whose clock moved,
    /// not on whichever PCR happens to come next.
    pending_di_pids: Vec<u16>,
}

impl TsContinuityFixer {
    pub fn new() -> Self {
        Self {
            input_psi: HashMap::new(),
            // Start at 0; the first `on_switch` bumps this to 1 before
            // stamping, matching the long-standing "bumped from cached
            // version 0 to 1" behaviour on the first switch.
            next_psi_version: 0,
            pending_di_on_pcr: false,
            pending_di_pids: Vec::new(),
        }
    }

    /// Signal an upstream-source-side discontinuity (a PCR jump observed
    /// by `pcr_ingress_sampler::spawn_source_discontinuity_watch`). Arms
    /// the one-shot `discontinuity_indicator` for that PCR PID's next PCR
    /// through the fixer — DI=1, telling receivers "this PCR is a fresh STC
    /// anchor, flush old timestamp tracking". Without this, professional
    /// and prosumer decoders treat the backward jump as a clock fault and
    /// either silently slide A/V or stall. Only that PID's PCR takes it: on
    /// an MPTS each program has its own clock.
    ///
    /// Kept apart from the operator-input-switch flag, so a switch
    /// happening concurrently with a source discontinuity loses neither.
    ///
    /// It arms DI on that PID's next PCR and nothing else: CC goes out as
    /// the source sent it. It used to switch on the post-switch path,
    /// because only that path applied DI (c7014f8), and that renumbered CC
    /// for the rest of the flow, hiding every later loss (edge#214).
    pub fn signal_source_discontinuity(&mut self, pcr_pid: u16) {
        if !self.pending_di_pids.contains(&pcr_pid) {
            self.pending_di_pids.push(pcr_pid);
        }
    }

    /// Whether a PCR-bearing packet on `pid` takes a pending DI, and
    /// consume it if so: the switch's one-shot `on_pcr`, or `pid`'s own
    /// entry in `pids`. An associated fn over the two fields, so
    /// [`Self::process_packet`] can call it while it holds the input's PSI
    /// cache.
    fn take_pending_di(on_pcr: &mut bool, pids: &mut Vec<u16>, pid: u16) -> bool {
        let targeted = match pids.iter().position(|p| *p == pid) {
            Some(i) => {
                pids.swap_remove(i);
                true
            }
            None => false,
        };
        std::mem::take(on_pcr) || targeted
    }

    /// Forget the per-input PSI cache for `input_id`.
    ///
    /// Called when an input is hot-removed from a flow so that a later
    /// hot-add of an input under the same id doesn't carry stale PAT/PMT
    /// from a different source.
    ///
    /// Idempotent: dropping an unknown input id is a no-op.
    pub fn forget_input(&mut self, input_id: &str) {
        self.input_psi.remove(input_id);
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

    /// Called when the active input changes. The new input's packets go
    /// out with their own CCs, so the cut shows as one CC jump per PID —
    /// a natural jump that signals receivers to flush and resync — and
    /// every later gap is real loss. Also returns cached PSI packets
    /// (PAT + PMTs) from the *new* input for fast receiver
    /// re-acquisition; they keep the CC they were cached with.
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
    /// The CC jump between the old input's CCs and the new input's is the
    /// other half of the switch signal: receivers (VLC, ffplay, hardware
    /// decoders) detect it as "packet loss," flush PES buffers, and resync
    /// on the next PES start (PUSI=1). Nothing forces it: a PID whose new
    /// CC happens to follow (or repeat) the old one shows no jump, a 1 in
    /// 16 chance each.
    ///
    /// On top of the CC jump and the version-bumped phantom PSI, we also
    /// arm `pending_di_on_pcr` so the **first** PCR-bearing TS packet that
    /// flows from the new input has its `discontinuity_indicator` bit set,
    /// and arm every PCR_PID the input's cached PMTs name, so on an MPTS
    /// the first PCR of each program after the cut carries it too.
    /// CC alone is empirically not enough on the passthrough output path —
    /// receivers strictly tracking PCR continuity (the broadcast-grade
    /// case) interpret the post-switch PCR-epoch jump as a clock fault
    /// rather than a stream change and freeze. DI=1 is the spec-defined
    /// "next PCR is a fresh anchor" signal and unblocks recovery on every
    /// receiver we've tested. The flag is set on every switch (including
    /// dead-source switches) so the next real packet picks it up whenever
    /// the new input eventually produces data.
    pub fn on_switch(&mut self, new_input_id: &str) -> Vec<RtpPacket> {
        self.pending_di_on_pcr = true;

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
                psi.cached_pmts.values().cloned().collect::<Vec<_>>(),
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
             version={stamp} (monotonic)",
            if cached_pat.is_some() { "yes" } else { "no" },
            cached_pmts.len(),
        );

        // Every program's clock changes at the cut, not only the one whose
        // PCR comes first: on an MPTS each PCR PID's first PCR after it
        // takes a DI. With only the one-shot flag, the other programs'
        // first PCRs went out without one, and the source-discontinuity
        // watch then stamped theirs a PCR late.
        for pid in pcr_pids(&cached_pmts) {
            if !self.pending_di_pids.contains(&pid) {
                self.pending_di_pids.push(pid);
            }
        }

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
                upstream_seq: None,
                upstream_leg_id: None,
                sender_timestamp_us: None,
            });
        }

        // Inject PMTs from the new input's cache with the same stamp.
        for pmt in cached_pmts
            .iter()
            .flat_map(|unit| super::ts_pmt_edit::restamp_pmt_unit(unit, stamp))
        {
            injected.push(RtpPacket {
                data: Bytes::copy_from_slice(&pmt),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
                upstream_seq: None,
                upstream_leg_id: None,
                sender_timestamp_us: None,
            });
        }

        injected
    }

    /// Process a packet from the currently active input.
    ///
    /// Returns [`ProcessResult::Unchanged`] for every datagram except one
    /// holding the PCR that takes a pending DI (a switch's, or a source
    /// discontinuity's on that PCR's PID), for which it returns
    /// [`ProcessResult::Rewritten`]: a copy of the datagram with that one
    /// bit set. CC is never rewritten, so every gap in the source — lost
    /// packets, legal duplicates, the source's own CC reset — goes out as
    /// it came (edge#214). PSI is observed on every packet for the switch
    /// cache.
    pub fn process_packet(&mut self, input_id: &str, packet: &RtpPacket) -> ProcessResult {
        let Self { input_psi, pending_di_on_pcr, pending_di_pids, .. } = self;
        let psi = match input_psi.get_mut(input_id) {
            Some(psi) => psi,
            None => {
                input_psi.insert(input_id.to_string(), InputPsiCache::new());
                input_psi.get_mut(input_id).unwrap()
            }
        };
        let ts_data = strip_rtp_header(packet);
        // The TS bytes are the datagram's tail.
        let ts_start = packet.data.len() - ts_data.len();
        let mut out: Option<Vec<u8>> = None;
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= ts_data.len() {
            let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
            let at = ts_start + offset;
            offset += TS_PACKET_SIZE;
            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }
            psi.observe_psi(pkt);
            // DI goes on the first PCR-bearing packet that takes a pending
            // one. PCR may ride either a payload-bearing PUSI packet or an
            // AF-only packet (common for software encoders that decouple
            // PCR cadence from ES boundaries). Only a PCR consumes the
            // flag, so a packet that cannot carry DI never burns it.
            let pid = ts_pid(pkt);
            if (*pending_di_on_pcr || !pending_di_pids.is_empty())
                && pid != NULL_PID
                && extract_pcr(pkt).is_some()
                && Self::take_pending_di(pending_di_on_pcr, pending_di_pids, pid)
            {
                // One copy of the datagram, written in place: the RTP
                // header and any trailing partial bytes stay byte-identical.
                let buf = out.get_or_insert_with(|| packet.data.to_vec());
                set_discontinuity_indicator(&mut buf[at..at + TS_PACKET_SIZE]);
            }
        }
        match out {
            Some(buf) => ProcessResult::Rewritten(Bytes::from(buf)),
            None => ProcessResult::Unchanged,
        }
    }
}

/// TS packets in an RTP datagram the edge builds itself: a switch's PSI on
/// an RTP-framed input ([`frame_switch_psi`]), and a raw-TS input's
/// datagrams on an RTP-framed flow ([`FlowFraming`]). Six, not the seven of
/// a raw-TS datagram: the 12-byte RTP header and seven packets come to 1328
/// bytes, past the 1316-byte live message an SRT output sends each item in,
/// which would split the datagram and leave its tail without a header.
pub(crate) const EDGE_RTP_TS_PACKETS: usize = 6;

/// One RTP datagram of `ts` under the header fields given, as the flow
/// broadcast carries it, received with `template` and carrying
/// `sender_timestamp_us`. A sender timestamp is the sampler's per-datagram
/// clock sample, so only one datagram may carry each.
fn edge_rtp_datagram(
    ts: &[u8],
    payload_type: u8,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    template: &RtpPacket,
    sender_timestamp_us: Option<i64>,
) -> RtpPacket {
    let mut data = Vec::with_capacity(RTP_HEADER_MIN_SIZE + ts.len());
    data.extend_from_slice(&[0x80, payload_type & 0x7F]);
    data.extend_from_slice(&seq.to_be_bytes());
    data.extend_from_slice(&timestamp.to_be_bytes());
    data.extend_from_slice(&ssrc.to_be_bytes());
    data.extend_from_slice(ts);
    RtpPacket {
        data: Bytes::from(data),
        sequence_number: seq,
        rtp_timestamp: timestamp,
        recv_time_us: template.recv_time_us,
        is_raw_ts: false,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us,
    }
}

/// Frame the PSI a switch injects ([`TsContinuityFixer::on_switch`], one
/// raw-TS item per packet) like `first`, the newly active input's first
/// datagram, which goes out right after it.
///
/// Raw TS when `first` is raw TS. When it is RTP, the PSI goes out as RTP
/// too — datagrams of up to [`EDGE_RTP_TS_PACKETS`] packets under `first`'s
/// SSRC, payload type and timestamp, numbered up to the one before
/// `first`'s, so a receiver sees them as the input's own stream running
/// into its first datagram. Those numbers are the input's datagrams just
/// before it, which never reached this flow's outputs while the input was
/// passive. Injected as raw TS, the PSI was the first datagram of the new
/// input on an RTP-framed flow, and an SRT output passes framing through:
/// a receiver that (re)connected then and settles its framing on the first
/// datagram it gets settled raw TS, and dropped every RTP datagram after.
///
/// An RTP `first` too short to hold a header injects nothing: raw PSI ahead
/// of RTP is the mix this exists to prevent, and the receiver takes the
/// PSI from the stream itself within a repetition interval.
pub fn frame_switch_psi(injected: Vec<RtpPacket>, first: &RtpPacket) -> Vec<RtpPacket> {
    if first.is_raw_ts || injected.is_empty() {
        return injected;
    }
    let header = &first.data;
    if header.len() < RTP_HEADER_MIN_SIZE || header[0] >> 6 != 2 {
        return Vec::new();
    }
    let payload_type = header[1] & 0x7F;
    let seq = u16::from_be_bytes([header[2], header[3]]);
    let timestamp = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let ssrc = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let ts: Vec<u8> = injected.iter().flat_map(|p| p.data.iter().copied()).collect();
    let chunks: Vec<&[u8]> = ts.chunks(EDGE_RTP_TS_PACKETS * TS_PACKET_SIZE).collect();
    let n = chunks.len() as u16;
    chunks
        .into_iter()
        .zip(0u16..)
        .map(|(chunk, i)| {
            edge_rtp_datagram(chunk, payload_type, seq.wrapping_sub(n - i), timestamp, ssrc, first, None)
        })
        .collect()
}

/// The PSI a switch injects ([`TsContinuityFixer::on_switch`], one item per
/// packet) as raw-TS items of up to seven packets, under `first`'s
/// timestamp and receive time: for a flow whose framing `first`, the
/// datagram it goes out ahead of, is not in, where [`FlowFraming::conform`]
/// puts the PSI in the flow's framing with `first` — wrapped six packets to
/// a datagram rather than one, or passed.
pub fn bundle_switch_psi(injected: Vec<RtpPacket>, first: &RtpPacket) -> Vec<RtpPacket> {
    super::packet::bundle_ts_packets(injected.into_iter().map(|p| p.data).collect())
        .into_iter()
        .map(|data| RtpPacket {
            data,
            sequence_number: 0,
            rtp_timestamp: first.rtp_timestamp,
            recv_time_us: first.recv_time_us,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        })
        .collect()
}

/// The one TS framing a flow's broadcast carries: the framing of the first
/// media datagram its fixer task forwards — RTP-wrapped or raw TS.
///
/// A multi-input flow may switch between an RTP-framed input and a raw-TS
/// one, and an output that passes framing through (SRT) would then carry
/// both on one connection; a receiver that settles its framing on the first
/// datagram it gets drops every datagram in the other. So a datagram in the
/// other framing is converted here, once, for every output: an RTP
/// datagram on a raw-TS flow loses its RTP header, and a raw-TS datagram on
/// an RTP-framed flow is wrapped in the flow's own RTP header — its own
/// SSRC and sequence numbers, [`EDGE_RTP_TS_PACKETS`] packets to a
/// datagram. That SSRC changes at the Take like any switch between two RTP
/// inputs; it is never interleaved with an input's. A flow whose inputs
/// all share one framing never converts anything.
///
/// The idle keepalive (`flow::null_ts_packet`) is not conformed: it is raw
/// TS on every flow, which an SRT output holds back from an RTP-framed
/// stream (`output_srt::KeepaliveGate`) and an RTP output wraps itself.
pub struct FlowFraming {
    /// Whether the flow carries raw TS; `None` before its first media
    /// datagram.
    raw_ts: Option<bool>,
    /// The flow's own RTP header, for raw-TS datagrams on an RTP-framed
    /// flow.
    wrap: super::output_rtp::RtpWrapState,
}

impl FlowFraming {
    pub fn new() -> Self {
        Self { raw_ts: None, wrap: super::output_rtp::RtpWrapState::new() }
    }

    /// Take `pkt`'s framing for the flow's if it has none yet: an active
    /// input's datagram, before it — and anything a switch injects ahead
    /// of it — is conformed. NULL packets alone settle nothing: a raw-TS
    /// datagram of them says nothing of how the input frames its media.
    pub fn observe(&mut self, pkt: &RtpPacket) {
        if self.raw_ts.is_none() && !(pkt.is_raw_ts && is_null_ts_only(&pkt.data)) {
            self.raw_ts = Some(pkt.is_raw_ts);
        }
    }

    /// Whether `pkt` goes out in the framing it came in.
    pub fn passes(&self, pkt: &RtpPacket) -> bool {
        self.raw_ts.is_none_or(|raw| raw == pkt.is_raw_ts)
    }

    /// Hand `pkt` to `emit` in the flow's framing: as it came when it is
    /// already in it (the only case on a flow whose inputs share one),
    /// otherwise stripped of its RTP header or wrapped in the flow's.
    pub fn conform(&mut self, pkt: RtpPacket, mut emit: impl FnMut(RtpPacket)) {
        match self.raw_ts {
            Some(true) if !pkt.is_raw_ts => {
                let start = pkt.data.len() - strip_rtp_header(&pkt).len();
                if start < pkt.data.len() {
                    emit(RtpPacket { data: pkt.data.slice(start..), is_raw_ts: true, ..pkt });
                }
            }
            Some(false) if pkt.is_raw_ts => {
                let mut sender_timestamp_us = pkt.sender_timestamp_us;
                for chunk in pkt.data.chunks(EDGE_RTP_TS_PACKETS * TS_PACKET_SIZE) {
                    let header = self.wrap.build_header(pkt.rtp_timestamp);
                    let seq = u16::from_be_bytes([header[2], header[3]]);
                    let timestamp = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                    let ssrc = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
                    emit(edge_rtp_datagram(
                        chunk,
                        header[1],
                        seq,
                        timestamp,
                        ssrc,
                        &pkt,
                        sender_timestamp_us.take(),
                    ));
                }
            }
            _ => emit(pkt),
        }
    }
}

/// The PCR_PID each valid PMT in `units` names (NULL PID — no PCR —
/// left out): the programs whose clock a switch to the input they were
/// cached from moves.
fn pcr_pids(units: &[Vec<[u8; TS_PACKET_SIZE]>]) -> Vec<u16> {
    let mut pids = Vec::new();
    for unit in units {
        let mut asm = SectionAssembler::new();
        for pkt in unit {
            for sec in asm.push_packet(pkt) {
                if sec.first() == Some(&0x02) && sec.len() >= 16 && mpeg2_crc32(sec) == 0 {
                    let pid = u16::from_be_bytes([sec[8], sec[9]]) & 0x1FFF;
                    if pid != NULL_PID && !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids
}

/// Back-compat wrapper: bump the `version_number` by 1 modulo 32 and fix
/// the CRC. Retained for any callers / tests that want the historical
/// "increment-from-current" behaviour. New production code on the switch
/// path uses [`set_psi_version`] (re-exported from `ts_parse`) with the
/// monotonic counter instead.
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
        // A real PMT carries a CRC_32; the switch cache only keeps PMTs
        // whose CRC verifies.
        let crc = mpeg2_crc32(&pkt[5..pos]);
        pkt[pos..pos + 4].copy_from_slice(&crc.to_be_bytes());
        pkt
    }

    fn make_rtp_packet(ts_data: &[u8]) -> RtpPacket {
        RtpPacket {
            data: Bytes::copy_from_slice(ts_data),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────

    /// A PMT spanning two packets is cached as a whole unit and injected
    /// complete, with the monotonic version stamped into the section (whose
    /// CRC lives in the second packet) — it used to inject the first packet
    /// alone, which no receiver can use.
    #[test]
    fn on_switch_injects_a_two_packet_pmt_whole_and_stamped() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, two_packet_pmt};
        let (sec, _) = two_packet_pmt(1, 0);
        let pmt = packetize_sections(0x40, &[&sec], 0);
        let mut data = pat_packet(&[(1, 0x40)], 0, 0).to_vec();
        data.extend_from_slice(&pmt[0]);
        data.extend_from_slice(&pmt[1]);
        let mut fixer = TsContinuityFixer::new();
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 3, "PAT + both PMT packets");
        let mut asm = SectionAssembler::new();
        let mut secs = Vec::new();
        for p in &injected[1..] {
            secs.extend(asm.push_packet(&p.data).map(|s| s.to_vec()));
        }
        assert_eq!(secs.len(), 1);
        assert_eq!((secs[0][5] >> 1) & 0x1F, 1, "stamped");
        assert_eq!(mpeg2_crc32(&secs[0]), 0, "valid CRC");
    }

    /// A CC gap that aborts a later unit mid-way must not replace the last
    /// good cached unit: the fixer used to cache [PUSI, gapped
    /// continuation] as soon as the assembler stopped being in flight, and
    /// the next switch injected that truncated, unstampable PMT.
    #[test]
    fn a_unit_aborted_by_a_cc_gap_does_not_replace_the_cached_pmt() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        // A PMT spanning three packets.
        let descs: Vec<Vec<u8>> =
            (0..30u8).map(|i| vec![0x0A, 0x04, b'e', b'n', b'a' + i % 26, 0, 0x52, 0x01, i]).collect();
        let es: Vec<(u8, u16, &[u8])> =
            descs.iter().enumerate().map(|(i, d)| (0x06, 0x200 + i as u16, d.as_slice())).collect();
        let sec = pmt_section(1, 0, 0x200, &[], &es);
        let good = packetize_sections(0x40, &[&sec], 0);
        assert_eq!(good.len(), 3);
        // The next repetition loses its middle packet: P1 (CC 3), P3 (CC 5).
        let later = packetize_sections(0x40, &[&sec], 3);
        let mut data = pat_packet(&[(1, 0x40)], 0, 0).to_vec();
        for p in [good[0], good[1], good[2], later[0], later[2]] {
            data.extend_from_slice(&p);
        }
        let mut fixer = TsContinuityFixer::new();
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 4, "PAT + the good three-packet PMT");
        let mut asm = SectionAssembler::new();
        let mut secs = Vec::new();
        for p in &injected[1..] {
            secs.extend(asm.push_packet(&p.data).map(|s| s.to_vec()));
        }
        assert_eq!(secs.len(), 1, "the injected unit reassembles");
        assert_eq!(mpeg2_crc32(&secs[0]), 0, "valid CRC");
        assert_eq!((secs[0][5] >> 1) & 0x1F, 1, "and is stamped");
    }

    /// A unit whose PMT is complete in its first packet but whose next
    /// section (a long private table) loses a packet: the unit is aborted
    /// as a whole and the last good one stays cached. Committing it would
    /// have cached [PUSI, gapped continuation], which the switch then
    /// injected unstamped and unreassemblable.
    #[test]
    fn a_unit_aborted_after_its_pmt_completed_is_not_cached() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        // A long-form private section (table 0xC1) spanning into two more
        // packets.
        let len = 5 + 400 + 4;
        let mut private = vec![0xC1, 0xB0 | (len >> 8) as u8, len as u8, 0, 1, 0xC1, 0, 0];
        private.resize(3 + len - 4, 0x5A);
        let crc = mpeg2_crc32(&private);
        private.extend_from_slice(&crc.to_be_bytes());
        let good = packetize_sections(0x40, &[&pmt, &private], 0);
        assert_eq!(good.len(), 3);
        let later = packetize_sections(0x40, &[&pmt, &private], 3);
        let mut data = pat_packet(&[(1, 0x40)], 0, 0).to_vec();
        for p in [good[0], good[1], good[2], later[0], later[2]] {
            data.extend_from_slice(&p);
        }
        let mut fixer = TsContinuityFixer::new();
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 4, "PAT + the good three-packet unit");
        let mut asm = SectionAssembler::new();
        let mut tables = Vec::new();
        for p in &injected[1..] {
            tables.extend(asm.push_packet(&p.data).map(|s| s[0]));
        }
        assert_eq!(tables, vec![0x02, 0xC1]);
    }

    /// A PMT that ends in the next unit's PUSI packet, before its
    /// pointer_field target, belongs to the unit it started in: both
    /// packets are cached as one unit (the rule `PsiUnitStage` applies).
    /// Clearing the unit at every PUSI kept only the second packet — the
    /// tail of the program's PMT, which the switch could not inject.
    #[test]
    fn a_pmt_finishing_in_the_next_pointer_tail_is_cached_whole() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        let a = pmt_section(1, 0, 0x100, &[0u8; 170], &[(0x1B, 0x100, &[])]);
        let b = pmt_section(2, 0, 0x200, &[], &[(0x0F, 0x201, &[])]);
        let pkts = packetize_sections(0x31, &[&a, &b], 0);
        assert_eq!(pkts.len(), 2);
        assert!(ts_pusi(&pkts[1]) && pkts[1][4] > 0, "tail before the pointer");
        let mut data = pat_packet(&[(1, 0x31)], 0, 0).to_vec();
        data.extend_from_slice(&pkts[0]);
        data.extend_from_slice(&pkts[1]);
        let mut fixer = TsContinuityFixer::new();
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 3, "PAT + both packets of the unit");
        let mut asm = SectionAssembler::new();
        let mut programs = Vec::new();
        for p in &injected[1..] {
            for sec in asm.push_packet(&p.data) {
                assert_eq!(mpeg2_crc32(sec), 0);
                programs.push(u16::from_be_bytes([sec[3], sec[4]]));
            }
        }
        assert_eq!(programs, vec![1, 2]);
    }

    /// A PMT whose CRC does not verify must not replace the cached one
    /// either: the switch re-stamps the cached PMT's version, which
    /// recomputes its CRC, so a damaged PMT would go out looking valid.
    #[test]
    fn a_pmt_failing_its_crc_does_not_replace_the_cached_pmt() {
        let good = build_pmt(0x1000, &[(0x1B, 0x100), (0x0F, 0x101)], 0);
        let mut bad = build_pmt(0x1000, &[(0x1B, 0x100), (0x0F, 0x101)], 1);
        bad[22] = 0x03; // the audio stream_type, CRC left as it was
        let mut data = build_pat(&[(1, 0x1000)], 0).to_vec();
        data.extend_from_slice(&good);
        data.extend_from_slice(&bad);
        let mut fixer = TsContinuityFixer::new();
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 2);
        let pmt = &injected[1].data;
        assert_eq!(pmt[22], 0x0F, "the last good PMT is injected");
        assert!(verify_psi_crc(pmt, 5));
    }

    /// VH1.ts: the pointer target on the PMT PID is a short-form 0xC0
    /// section; the PMT sits at offset 29. The phantom PMT injected on a
    /// switch used to be "stamped" at the pointer target — a data byte and
    /// the last four bytes of the 0xC0 table overwritten, the real PMT's
    /// version never bumped, defeating the re-parse the fixer exists for.
    #[test]
    fn on_switch_stamps_the_pmt_behind_a_private_section() {
        use crate::engine::ts_test_fixtures::{vh1_pat_packet, vh1_pmt_packet, VH1_PMT_OFFSET};
        let mut fixer = TsContinuityFixer::new();
        let pmt = vh1_pmt_packet();
        let mut data = vh1_pat_packet().to_vec();
        data.extend_from_slice(&pmt);
        fixer.observe_passive("b", &make_rtp_packet(&data));
        let injected = fixer.on_switch("b");
        assert_eq!(injected.len(), 2);
        let out = &injected[1].data;
        assert_eq!(&out[..VH1_PMT_OFFSET], &pmt[..VH1_PMT_OFFSET], "0xC0 section untouched");
        let old_v = (pmt[VH1_PMT_OFFSET + 5] >> 1) & 0x1F;
        let new_v = (out[VH1_PMT_OFFSET + 5] >> 1) & 0x1F;
        assert_eq!(new_v, 1, "monotonic stamp (was {old_v})");
        assert!(verify_psi_crc(out, VH1_PMT_OFFSET), "PMT CRC valid");
    }

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

        // Switch to input B.
        let _ = fixer.on_switch("b");

        // Input B sends PID 0x100 with CC=0 (its own sequence). It passes
        // through with its original CC, creating a CC jump (7 → 0) that
        // the receiver detects as packet loss and handles by
        // flushing/resyncing.
        let pkt = make_rtp_packet(&build_ts_packet(0x100, 0));
        let result = fixer.process_packet("b", &pkt).forwarded(&pkt);
        assert_eq!(ts_cc(&result[0..TS_PACKET_SIZE]), 0);

        // Second packet maintains continuity within B's sequence.
        let pkt = make_rtp_packet(&build_ts_packet(0x100, 1));
        let result = fixer.process_packet("b", &pkt).forwarded(&pkt);
        assert_eq!(ts_cc(&result[0..TS_PACKET_SIZE]), 1);
    }

    #[test]
    fn cc_continuity_within_input_after_switch() {
        let mut fixer = TsContinuityFixer::new();

        let ts = build_ts_packet(0x100, 14);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        let _ = fixer.on_switch("b");

        // Input B: CC wraps around within its own sequence.
        let pkt1 = make_rtp_packet(&build_ts_packet(0x100, 14));
        let result1 = fixer.process_packet("b", &pkt1).forwarded(&pkt1);
        assert_eq!(ts_cc(&result1[..TS_PACKET_SIZE]), 14);

        let pkt2 = make_rtp_packet(&build_ts_packet(0x100, 15));
        let result2 = fixer.process_packet("b", &pkt2).forwarded(&pkt2);
        assert_eq!(ts_cc(&result2[..TS_PACKET_SIZE]), 15);

        let pkt3 = make_rtp_packet(&build_ts_packet(0x100, 0));
        let result3 = fixer.process_packet("b", &pkt3).forwarded(&pkt3);
        assert_eq!(ts_cc(&result3[..TS_PACKET_SIZE]), 0); // wraps
    }

    /// A switch shows as one CC jump on every PID the two inputs share: the
    /// new input's first packet on each goes out with its own CC.
    #[test]
    fn a_switch_shows_as_one_cc_jump_per_pid() {
        let mut fixer = TsContinuityFixer::new();

        // Input A: two PIDs.
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 5)));
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x200, 10)));

        let _ = fixer.on_switch("b");

        // Input B starts both PIDs at 0: 5 → 0 and 10 → 0.
        for pid in [0x100, 0x200] {
            let pkt = make_rtp_packet(&build_ts_packet(pid, 0));
            let out = fixer.process_packet("b", &pkt).forwarded(&pkt);
            assert_eq!(ts_cc(&out[..TS_PACKET_SIZE]), 0, "PID {pid:#x}");
        }
    }

    #[test]
    fn pat_pmt_cached_from_passive_input() {
        let mut fixer = TsContinuityFixer::new();

        // Active input sends some data first.
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
    fn multiple_switches_pass_each_inputs_cc_through() {
        let mut fixer = TsContinuityFixer::new();

        // Input A: PID 0x100, CC=0..4
        for cc in 0..=4u8 {
            fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, cc)));
        }

        // Switch to B — B's CC passes through.
        let _ = fixer.on_switch("b");
        let pkt = make_rtp_packet(&build_ts_packet(0x100, 10));
        let result = fixer.process_packet("b", &pkt).forwarded(&pkt);
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 10);

        let pkt = make_rtp_packet(&build_ts_packet(0x100, 11));
        let result = fixer.process_packet("b", &pkt).forwarded(&pkt);
        assert_eq!(ts_cc(&result[..TS_PACKET_SIZE]), 11);

        // Switch back to A — A's CC passes through.
        let _ = fixer.on_switch("a");
        let pkt = make_rtp_packet(&build_ts_packet(0x100, 0));
        let result = fixer.process_packet("a", &pkt).forwarded(&pkt);
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

        let _ = fixer.on_switch("b");

        // Input B: 3 packets in one datagram. CC passes through.
        let mut data_b = Vec::new();
        for cc in 0..=2u8 {
            data_b.extend_from_slice(&build_ts_packet(0x100, cc));
        }
        let pkt_b = make_rtp_packet(&data_b);
        let result = fixer.process_packet("b", &pkt_b).forwarded(&pkt_b);

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
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };

        assert!(matches!(fixer.process_packet("a", &pkt), ProcessResult::Unchanged));

        let _ = fixer.on_switch("b");

        // New packet from input B, RTP-wrapped, carrying the PCR that takes
        // the switch's DI: the bit is written at the RTP header's length
        // plus the TS packet's offset.
        let mut rtp_data2 = vec![0u8; RTP_HEADER_MIN_SIZE];
        rtp_data2[0] = 0x80;
        rtp_data2[1] = 33;
        rtp_data2[2..12].copy_from_slice(&[0x12, 0x34, 0, 0, 0x07, 0xD0, 0xDE, 0xAD, 0xBE, 0xEF]);
        let ts2 = build_pcr_packet(0x100, 0, 27_000_000);
        rtp_data2.extend_from_slice(&ts2);

        let pkt2 = RtpPacket {
            data: Bytes::from(rtp_data2),
            sequence_number: 100,
            rtp_timestamp: 2000,
            recv_time_us: 600,
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };

        let result = fixer.process_packet("b", &pkt2).unwrap_rewritten();
        // RTP header should be preserved byte for byte.
        assert_eq!(&result[..RTP_HEADER_MIN_SIZE], &pkt2.data[..RTP_HEADER_MIN_SIZE]);
        let ts_start = RTP_HEADER_MIN_SIZE;
        let ts_out = &result[ts_start..ts_start + TS_PACKET_SIZE];
        assert!(ts_discontinuity_indicator(ts_out), "DI on the PCR");
        // CC passes through from the new input.
        assert_eq!(ts_cc(ts_out), 0);
        // Nothing but the DI bit changed.
        let mut expected = pkt2.data.to_vec();
        expected[ts_start + 5] |= 0x80;
        assert_eq!(&result[..], &expected[..]);
    }

    #[test]
    fn new_pid_after_switch_gets_natural_cc() {
        let mut fixer = TsContinuityFixer::new();

        let ts = build_ts_packet(0x100, 3);
        fixer.process_packet("a", &make_rtp_packet(&ts));

        let _ = fixer.on_switch("b");

        // Input B has PID 0x200. CC passes through.
        let pkt = make_rtp_packet(&build_ts_packet(0x200, 7));
        let result = fixer.process_packet("b", &pkt).forwarded(&pkt);
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
        let v = make_rtp_packet(&build_ts_packet(0x100, 0));
        let result = fixer.process_packet("b", &v).forwarded(&v);
        assert_eq!(ts_pid(&result[..TS_PACKET_SIZE]), 0x100);

        // Audio packet also passes through immediately.
        let a = make_rtp_packet(&build_ts_packet(0x101, 0));
        let result = fixer.process_packet("b", &a).forwarded(&a);
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

        // Drive a packet through the active input first.
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

    // ── DI-on-PCR after switch ────────────────────────────────────────────

    /// Build a payload-bearing PUSI=1 packet on `pid` carrying a PCR field
    /// in the adaptation field. AF layout: length(1) + flags(1, PCR_flag=1)
    /// + PCR(6) = 8 bytes. Remaining 176 bytes are payload (0xFF stuffing).
    fn build_pcr_packet(pid: u16, cc: u8, pcr_27mhz: u64) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pid >> 8) as u8) & 0x1F); // PUSI=1
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x30 | (cc & 0x0F); // AFC=11 (AF + payload)
        pkt[4] = 7; // af_length (flags + PCR)
        pkt[5] = 0x10; // PCR_flag=1, DI=0
        let base = (pcr_27mhz / 300) & 0x1_FFFF_FFFF;
        let ext = (pcr_27mhz % 300) as u32;
        pkt[6] = ((base >> 25) & 0xFF) as u8;
        pkt[7] = ((base >> 17) & 0xFF) as u8;
        pkt[8] = ((base >> 9) & 0xFF) as u8;
        pkt[9] = ((base >> 1) & 0xFF) as u8;
        pkt[10] = (((base & 1) << 7) as u8) | 0x7E | (((ext >> 8) & 0x01) as u8);
        pkt[11] = (ext & 0xFF) as u8;
        pkt
    }

    /// AF-only PCR packet (no payload) — encoders often emit these to keep
    /// PCR cadence independent of ES/PES boundaries.
    fn build_pcr_only_packet(pid: u16, cc: u8, pcr_27mhz: u64) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = build_pcr_packet(pid, cc, pcr_27mhz);
        pkt[3] = 0x20 | (cc & 0x0F); // AFC=10 (AF only, no payload)
        // af_length must cover the rest of the packet (188 - 4 - 1 = 183).
        pkt[4] = 183;
        pkt
    }

    /// On the very first PCR-bearing TS packet after `on_switch`, the
    /// continuity fixer must light DI=1 in the adaptation field. Without
    /// this, professional / prosumer receivers see the post-switch PCR
    /// epoch jump as a clock fault and freeze.
    #[test]
    fn first_pcr_after_switch_carries_discontinuity_indicator() {
        let mut fixer = TsContinuityFixer::new();

        // Pre-switch: input "a" emits a PCR packet. No DI is pending, so
        // it passes unchanged.
        let pre = build_pcr_packet(0x100, 0, 27_000_000);
        let result = fixer.process_packet("a", &make_rtp_packet(&pre));
        assert!(matches!(result, ProcessResult::Unchanged));

        // Switch to "b". Arms pending_di_on_pcr.
        let _ = fixer.on_switch("b");

        // Input "b" sends its first PCR packet. DI must be set.
        let post = build_pcr_packet(0x100, 0, 99_999_999);
        let bytes = fixer
            .process_packet("b", &make_rtp_packet(&post))
            .unwrap_rewritten();
        assert!(
            ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]),
            "first PCR-bearing packet from new input must carry DI=1"
        );

        // Subsequent PCR packets from the same input do NOT keep setting DI
        // — the flag is one-shot per switch.
        let next = make_rtp_packet(&build_pcr_packet(0x100, 1, 100_009_000));
        let bytes = fixer.process_packet("b", &next).forwarded(&next);
        assert!(
            !ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]),
            "subsequent PCR packets within the same input must not carry DI"
        );
    }

    /// PCR riding an AF-only packet (no payload bit set) is the common case
    /// for software encoders that decouple PCR cadence from ES boundaries.
    /// The fixer must still set DI on these.
    #[test]
    fn first_pcr_after_switch_handles_af_only_carrier() {
        let mut fixer = TsContinuityFixer::new();

        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));
        let _ = fixer.on_switch("b");

        let af_only = build_pcr_only_packet(0x100, 0, 27_000_000);
        let bytes = fixer
            .process_packet("b", &make_rtp_packet(&af_only))
            .unwrap_rewritten();
        assert!(
            ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]),
            "DI must be set on AF-only PCR packets too"
        );
    }

    /// Non-PCR packets that arrive between the switch and the first PCR
    /// must NOT consume the one-shot — DI must still land on the next real
    /// PCR packet. Otherwise an audio PES PUSI arriving before the next
    /// video PCR would silently absorb the flag (audio PES has no AF, so
    /// DI can't be set on it anyway, and the first PCR would land without
    /// DI — exactly the bug we're fixing).
    #[test]
    fn non_pcr_packets_after_switch_do_not_consume_flag() {
        let mut fixer = TsContinuityFixer::new();

        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));
        let _ = fixer.on_switch("b");

        // Several payload-only packets without PCR.
        for cc in 0..3u8 {
            let plain = build_ts_packet(0x101, cc);
            let _ = fixer.process_packet("b", &make_rtp_packet(&plain));
        }

        // Then a PCR packet — must still receive DI=1.
        let pcr = build_pcr_packet(0x100, 0, 27_000_000);
        let bytes = fixer
            .process_packet("b", &make_rtp_packet(&pcr))
            .unwrap_rewritten();
        assert!(
            ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]),
            "DI flag survives across non-PCR packets after a switch"
        );
    }

    /// A source discontinuity the watcher saw on one PCR PID puts DI on that
    /// PID's next PCR — not on another program's PCR that happens to come
    /// first (an MPTS carries one clock per program).
    #[test]
    fn a_pcr_jump_signal_lands_on_its_own_pid() {
        let mut fixer = TsContinuityFixer::new();
        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));
        fixer.signal_source_discontinuity(0x200);
        let other = make_rtp_packet(&build_pcr_packet(0x100, 1, 27_000_000));
        let bytes = fixer.process_packet("a", &other).forwarded(&other);
        assert!(!ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]), "not another program's PCR");
        let own = build_pcr_packet(0x200, 0, 99_000_000);
        let bytes = fixer.process_packet("a", &make_rtp_packet(&own)).unwrap_rewritten();
        assert!(ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]), "the jumped PID's next PCR");
        let again = make_rtp_packet(&build_pcr_packet(0x200, 1, 99_810_000));
        let bytes = fixer.process_packet("a", &again).forwarded(&again);
        assert!(!ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]), "one-shot");
    }

    /// Each new switch re-arms the flag, so back-and-forth A↔B always
    /// flags the first PCR after every switch.
    #[test]
    fn each_switch_rearms_di_one_shot() {
        let mut fixer = TsContinuityFixer::new();

        fixer.process_packet("a", &make_rtp_packet(&build_ts_packet(0x100, 0)));

        for (target, pcr) in [
            ("b", 1_000_000u64),
            ("a", 50_000_000u64),
            ("b", 80_000_000u64),
            ("a", 120_000_000u64),
        ] {
            let _ = fixer.on_switch(target);
            let pkt = build_pcr_packet(0x100, 0, pcr);
            let bytes = fixer
                .process_packet(target, &make_rtp_packet(&pkt))
                .unwrap_rewritten();
            assert!(
                ts_discontinuity_indicator(&bytes[..TS_PACKET_SIZE]),
                "switch to '{target}' must carry DI on its first PCR"
            );

            // The very next PCR within the same input must NOT carry DI.
            let pkt2 = make_rtp_packet(&build_pcr_packet(0x100, 1, pcr.wrapping_add(40_000)));
            let bytes2 = fixer.process_packet(target, &pkt2).forwarded(&pkt2);
            assert!(
                !ts_discontinuity_indicator(&bytes2[..TS_PACKET_SIZE]),
                "subsequent PCR after switch to '{target}' must not carry DI"
            );
        }
    }

    // ── edge#214: CC passes through ─────────────────────────────────────

    /// The CCs the fixer forwards for `pkts` (one TS packet per datagram)
    /// from `input`.
    fn forwarded_ccs(fixer: &mut TsContinuityFixer, input: &str, pkts: &[[u8; TS_PACKET_SIZE]]) -> Vec<u8> {
        pkts.iter()
            .map(|ts| {
                let pkt = make_rtp_packet(ts);
                ts_cc(&fixer.process_packet(input, &pkt).forwarded(&pkt)[..TS_PACKET_SIZE])
            })
            .collect()
    }

    /// Loss on the new input after a switch still shows downstream as a CC
    /// gap. The fixer used to renumber every payload CC after the first
    /// switch, so B's lost CC 3 and 4 went out as a continuous 0..4.
    #[test]
    fn upstream_loss_after_a_switch_still_shows_as_a_cc_gap() {
        let mut fixer = TsContinuityFixer::new();
        let a: Vec<_> = (5..=7u8).map(|cc| build_ts_packet(0x100, cc)).collect();
        forwarded_ccs(&mut fixer, "a", &a);
        let _ = fixer.on_switch("b");
        let b: Vec<_> = [0u8, 1, 2, 5, 6].iter().map(|&cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "b", &b), vec![0, 1, 2, 5, 6]);
    }

    /// A source PCR jump arms a DI on that PID's next PCR and touches no
    /// CC: the PCR goes out with DI and its own CC, and the packets after
    /// it go out unchanged. The signal used to switch on CC renumbering
    /// for the rest of the flow (CC 2 then 3, where the source sent 6).
    #[test]
    fn a_source_pcr_jump_arms_di_without_renumbering_cc() {
        let mut fixer = TsContinuityFixer::new();
        let pcr = make_rtp_packet(&build_pcr_packet(0x100, 0, 27_000_000));
        assert!(matches!(fixer.process_packet("a", &pcr), ProcessResult::Unchanged));
        let payload = make_rtp_packet(&build_ts_packet(0x100, 1));
        assert!(matches!(fixer.process_packet("a", &payload), ProcessResult::Unchanged));
        fixer.signal_source_discontinuity(0x100);

        let jumped = make_rtp_packet(&build_pcr_packet(0x100, 2, 990_000_000));
        let out = fixer.process_packet("a", &jumped).unwrap_rewritten();
        assert!(ts_discontinuity_indicator(&out[..TS_PACKET_SIZE]), "DI on the jumped PCR");
        assert_eq!(ts_cc(&out[..TS_PACKET_SIZE]), 2, "its own CC");

        let after_loss = make_rtp_packet(&build_ts_packet(0x100, 6));
        assert!(
            matches!(fixer.process_packet("a", &after_loss), ProcessResult::Unchanged),
            "a later packet goes out as the source sent it, gap and all",
        );
    }

    /// A legal duplicate (the same CC twice) after a switch goes out as a
    /// duplicate. Renumbered, it became CC + 1 and pushed every later
    /// packet one on.
    #[test]
    fn a_duplicate_packet_after_a_switch_keeps_its_cc() {
        let mut fixer = TsContinuityFixer::new();
        forwarded_ccs(&mut fixer, "a", &[build_ts_packet(0x100, 9)]);
        let _ = fixer.on_switch("b");
        let b: Vec<_> = [3u8, 3, 4].iter().map(|&cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "b", &b), vec![3, 3, 4]);
    }

    /// An AF-only PCR packet after a loss carries the source's CC, which is
    /// also the CC of the last payload packet emitted on its PID. The fixer
    /// used to renumber the payload (4 → 2) and pin the AF-only packet to
    /// the renumbered value.
    #[test]
    fn a_pcr_only_packet_after_a_loss_carries_the_source_cc() {
        let mut fixer = TsContinuityFixer::new();
        forwarded_ccs(&mut fixer, "a", &[build_ts_packet(0x100, 12)]);
        let _ = fixer.on_switch("b");
        let b: Vec<_> = [0u8, 1, 4].iter().map(|&cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "b", &b), vec![0, 1, 4]);
        let af_only = make_rtp_packet(&build_pcr_only_packet(0x100, 4, 27_000_000));
        let out = fixer.process_packet("b", &af_only).unwrap_rewritten();
        assert_eq!(ts_cc(&out[..TS_PACKET_SIZE]), 4, "the source's CC");
        assert!(ts_discontinuity_indicator(&out[..TS_PACKET_SIZE]), "and the switch's DI");
    }

    /// Once the switch's DI has landed, the fixer is back to zero
    /// overhead: every later datagram, PCR-bearing or not, comes back
    /// `Unchanged`. It used to rewrite every datagram after the first
    /// switch, for the life of the flow.
    #[test]
    fn packets_after_the_switch_di_go_out_unchanged() {
        let mut fixer = TsContinuityFixer::new();
        forwarded_ccs(&mut fixer, "a", &[build_ts_packet(0x100, 3)]);
        let _ = fixer.on_switch("b");
        let first = make_rtp_packet(&build_pcr_packet(0x100, 0, 27_000_000));
        let out = fixer.process_packet("b", &first).unwrap_rewritten();
        assert!(ts_discontinuity_indicator(&out[..TS_PACKET_SIZE]));
        let payload = make_rtp_packet(&build_ts_packet(0x100, 1));
        assert!(matches!(fixer.process_packet("b", &payload), ProcessResult::Unchanged));
        let pcr = make_rtp_packet(&build_pcr_packet(0x100, 2, 27_540_000));
        assert!(matches!(fixer.process_packet("b", &pcr), ProcessResult::Unchanged));
    }

    /// A → B → A: each input's CCs pass through, and a gap on A after the
    /// return passes through with them.
    #[test]
    fn a_b_a_round_trip_passes_each_inputs_own_cc() {
        let mut fixer = TsContinuityFixer::new();
        let a1: Vec<_> = (0..=3u8).map(|cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "a", &a1), vec![0, 1, 2, 3]);
        let _ = fixer.on_switch("b");
        let b: Vec<_> = (10..=12u8).map(|cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "b", &b), vec![10, 11, 12]);
        let _ = fixer.on_switch("a");
        let a2: Vec<_> = [7u8, 8, 11, 12].iter().map(|&cc| build_ts_packet(0x100, cc)).collect();
        assert_eq!(forwarded_ccs(&mut fixer, "a", &a2), vec![7, 8, 11, 12]);
    }

    /// A pending DI for a PID the input never carries keeps the PCR check
    /// engaged but rewrites nothing: no PCR takes it, so every datagram
    /// comes back `Unchanged`. The signal used to switch on renumbering,
    /// so every datagram came back rewritten.
    #[test]
    fn a_stale_pending_di_does_not_rewrite_anything() {
        let mut fixer = TsContinuityFixer::new();
        fixer.signal_source_discontinuity(0x300);
        for cc in 0..4u8 {
            let plain = make_rtp_packet(&build_ts_packet(0x100, cc));
            assert!(matches!(fixer.process_packet("a", &plain), ProcessResult::Unchanged));
            let pcr = make_rtp_packet(&build_pcr_packet(0x100, cc, 27_000_000 + cc as u64 * 1_080_000));
            assert!(matches!(fixer.process_packet("a", &pcr), ProcessResult::Unchanged));
        }
        assert_eq!(fixer.pending_di_pids, vec![0x300], "still pending for its own PID");
    }

    /// A switch to an MPTS gives every program's first PCR after the cut a
    /// DI, not only the first PCR of any program: each program's clock
    /// moves. Before, the second program's first PCR went out without one,
    /// and the source-discontinuity watch stamped its DI a PCR late.
    #[test]
    fn a_switch_to_an_mpts_marks_every_programs_first_pcr() {
        let mut fixer = TsContinuityFixer::new();
        let psi = [
            build_pat(&[(1, 0x1000), (2, 0x1100)], 0),
            build_pmt(0x1000, &[(0x1B, 0x100)], 0),
            build_pmt(0x1100, &[(0x1B, 0x200)], 0),
        ];
        for pkt in &psi {
            fixer.observe_passive("b", &make_rtp_packet(pkt));
        }
        let _ = fixer.on_switch("b");
        let di = |fixer: &mut TsContinuityFixer, pid: u16, cc: u8| {
            let pkt = make_rtp_packet(&build_pcr_packet(pid, cc, 27_000_000 + cc as u64 * 1_080_000));
            ts_discontinuity_indicator(&fixer.process_packet("b", &pkt).forwarded(&pkt)[..TS_PACKET_SIZE])
        };
        assert!(di(&mut fixer, 0x100, 0), "program 1's first PCR");
        assert!(di(&mut fixer, 0x200, 0), "program 2's first PCR");
        assert!(!di(&mut fixer, 0x100, 1), "one DI per program");
        assert!(!di(&mut fixer, 0x200, 1));
        assert!(fixer.pending_di_pids.is_empty() && !fixer.pending_di_on_pcr, "nothing left pending");
    }

    /// An RTP datagram under `ssrc` numbered `seq` carrying `ts`, with a CSRC
    /// word in its header so a 12-byte strip would be wrong.
    fn rtp_with_csrc(ssrc: u32, seq: u16, ts: &[u8]) -> RtpPacket {
        let mut data = vec![0x81, 0x80 | 33];
        data.extend_from_slice(&seq.to_be_bytes());
        data.extend_from_slice(&3_000u32.to_be_bytes());
        data.extend_from_slice(&ssrc.to_be_bytes());
        data.extend_from_slice(&[0xC5; 4]);
        data.extend_from_slice(ts);
        RtpPacket { data: Bytes::from(data), is_raw_ts: false, sequence_number: seq, ..make_rtp_packet(&[]) }
    }

    fn header_fields(p: &RtpPacket) -> (u8, u8, u16, u32, u32) {
        let d = &p.data;
        (
            d[0],
            d[1],
            u16::from_be_bytes([d[2], d[3]]),
            u32::from_be_bytes([d[4], d[5], d[6], d[7]]),
            u32::from_be_bytes([d[8], d[9], d[10], d[11]]),
        )
    }

    /// A switch's PSI ahead of an RTP datagram goes out as RTP datagrams of
    /// up to six packets in that datagram's stream — its SSRC, payload type
    /// and timestamp, numbered up to the one before it — with no marker,
    /// CSRC or extension of its own. Ahead of raw TS it is left as it was,
    /// and ahead of an RTP datagram too short to hold a header nothing is
    /// injected rather than raw TS.
    #[test]
    fn switch_psi_takes_the_framing_of_the_datagram_it_precedes() {
        let raw_psi: Vec<RtpPacket> =
            (0..8u8).map(|cc| make_rtp_packet(&build_pat(&[(1, 0x1000)], cc))).collect();
        let first = rtp_with_csrc(0xFEED, 2, &build_ts_packet(0x100, 0));
        let framed = frame_switch_psi(raw_psi.clone(), &first);
        assert_eq!(framed.len(), 2, "eight packets: six, then two");
        for (p, (seq, len)) in framed.iter().zip([(0u16, 6usize), (1, 2)]) {
            assert!(!p.is_raw_ts);
            assert_eq!(header_fields(p), (0x80, 33, seq, 3_000, 0xFEED));
            assert_eq!(p.sequence_number, seq);
            assert_eq!(p.data.len(), RTP_HEADER_MIN_SIZE + len * TS_PACKET_SIZE);
        }
        let ts: Vec<u8> = framed.iter().flat_map(|p| p.data[RTP_HEADER_MIN_SIZE..].to_vec()).collect();
        let want: Vec<u8> = raw_psi.iter().flat_map(|p| p.data.to_vec()).collect();
        assert_eq!(ts, want, "the PSI bytes in order");

        // Numbered back across the wrap.
        let framed = frame_switch_psi(raw_psi.clone(), &rtp_with_csrc(1, 0, &[]));
        assert_eq!(framed.iter().map(|p| header_fields(p).2).collect::<Vec<_>>(), [0xFFFE, 0xFFFF]);

        let raw_first = make_rtp_packet(&build_ts_packet(0x100, 0));
        assert_eq!(frame_switch_psi(raw_psi.clone(), &raw_first).len(), 8, "raw ahead of raw");
        let short = RtpPacket { data: Bytes::from_static(&[0x80, 33, 0, 1]), is_raw_ts: false, ..raw_first };
        assert!(frame_switch_psi(raw_psi, &short).is_empty(), "nothing rather than raw TS");
    }

    fn conformed(framing: &mut FlowFraming, pkt: RtpPacket) -> Vec<RtpPacket> {
        let mut out = Vec::new();
        framing.conform(pkt, |p| out.push(p));
        out
    }

    /// The flow's framing is its first media datagram's; NULL packets alone
    /// settle nothing. On an RTP-framed flow raw TS is wrapped in the flow's
    /// own header — one SSRC, consecutive numbers, the item's timestamp and
    /// receive time kept, its sender timestamp on the first datagram only —
    /// six packets to a datagram; RTP passes as it came. On a raw-TS flow
    /// RTP loses its header, CSRCs included.
    #[test]
    fn a_flow_keeps_its_first_datagrams_framing() {
        let seven: Vec<u8> = (0..7u8).flat_map(|cc| build_ts_packet(0x100, cc)).collect();
        let null7: Vec<u8> = (0..7).flat_map(|_| build_ts_packet(NULL_PID, 0)).collect();

        let mut rtp_flow = FlowFraming::new();
        rtp_flow.observe(&make_rtp_packet(&null7));
        assert!(rtp_flow.passes(&make_rtp_packet(&seven)), "NULL packets settled nothing");
        let first = rtp_with_csrc(0xA, 9, &build_ts_packet(0x100, 0));
        rtp_flow.observe(&first);
        assert_eq!(conformed(&mut rtp_flow, first.clone())[0].data, first.data, "as it came");
        let mut raw_item = make_rtp_packet(&seven);
        raw_item.rtp_timestamp = 777;
        raw_item.recv_time_us = 42;
        raw_item.sender_timestamp_us = Some(5);
        assert!(!rtp_flow.passes(&raw_item));
        let wrapped = conformed(&mut rtp_flow, raw_item.clone());
        assert_eq!(wrapped.len(), 2);
        let (_, pt, seq0, ts, ssrc) = header_fields(&wrapped[0]);
        assert_eq!((pt, ts), (33, 777));
        assert_ne!(ssrc, 0xA);
        for (i, p) in wrapped.iter().enumerate() {
            assert!(!p.is_raw_ts);
            assert_eq!(header_fields(p).4, ssrc, "one SSRC");
            assert_eq!(header_fields(p).2, seq0.wrapping_add(i as u16), "numbered on");
            let sender_timestamp = if i == 0 { Some(5) } else { None };
            assert_eq!((p.recv_time_us, p.sender_timestamp_us), (42, sender_timestamp), "one sample");
        }
        assert_eq!(&wrapped[0].data[RTP_HEADER_MIN_SIZE..], &seven[..6 * TS_PACKET_SIZE]);
        assert_eq!(&wrapped[1].data[RTP_HEADER_MIN_SIZE..], &seven[6 * TS_PACKET_SIZE..]);
        let next = conformed(&mut rtp_flow, raw_item);
        assert_eq!(header_fields(&next[0]).2, seq0.wrapping_add(2), "the numbers carry on");

        let mut raw_flow = FlowFraming::new();
        raw_flow.observe(&make_rtp_packet(&seven));
        let stripped = conformed(&mut raw_flow, rtp_with_csrc(0xA, 9, &build_ts_packet(0x100, 3)));
        assert_eq!(stripped.len(), 1);
        assert!(stripped[0].is_raw_ts);
        assert_eq!(&stripped[0].data[..], &build_ts_packet(0x100, 3)[..]);
        assert!(conformed(&mut raw_flow, rtp_with_csrc(0xA, 10, &[])).is_empty(), "a header alone");
    }
}
