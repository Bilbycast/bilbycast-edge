// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Composite ingress post-processor: program filter → role-keyed PID
//! overrides → mechanical PID remap.
//!
//! Wraps the three input-side TS rewriting stages so each TS-carrying
//! input task can pull in symmetric `program_number` / `pid_overrides` /
//! `pid_map` semantics with one helper. The chain ordering mirrors the
//! output-side equivalent in the per-output forward loops:
//!
//! 1. **`TsProgramFilter`** narrows MPTS → SPTS at ingress when
//!    `program_number` is set. Drops every program except the named one
//!    before any other stage runs (saves work for the rest of the chain).
//! 2. **`TsPidOverridesRewriter`** applies role-keyed per-program PID
//!    rewrites for passthrough flows (no `audio_encode` / `video_encode`).
//!    Skipped when transcode is active — the transcoded path handles
//!    PID overrides inside the replacers themselves to avoid double-rewriting.
//! 3. **`TsPidRemapper`** mechanical `source_pid → target_pid` lookup.
//!    Runs last so the operator can layer "rewrite roles, then bulk-remap
//!    leftover PIDs".
//!
//! The processor is byte-stream oriented: callers feed 188-byte-aligned
//! TS bytes, get rewritten 188-byte-aligned TS bytes back. Two scratch
//! buffers are reused across calls (no steady-state allocations).

use std::collections::BTreeMap;

use crate::config::models::TsPidOverridesMap;

use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

/// Construction options assembled from one input config + a flag telling
/// the post-processor whether to skip the role-keyed override stage
/// (because the input has `audio_encode` / `video_encode` and the
/// transcoder already handles overrides internally).
pub struct InputPostProcessConfig<'a> {
    pub program_number: Option<u16>,
    pub pid_overrides: Option<&'a TsPidOverridesMap>,
    pub pid_map: Option<&'a BTreeMap<u16, u16>>,
    /// True when `audio_encode` or `video_encode` is configured on the
    /// input. The standalone `pid_overrides` rewriter is skipped on this
    /// path because the transcoder's replacers handle PID overrides
    /// inside the transcoded ES — chaining the standalone rewriter on
    /// top would double-rewrite.
    pub has_transcode: bool,
}

/// Composite TS post-processor for inputs.
///
/// Returns `None` from [`from_config`] when none of the stages have any
/// work to do — the caller can then skip the whole stage entirely
/// (zero cost, no scratch buffers allocated).
///
/// One scratch buffer per stage so the borrow checker is happy with
/// simultaneous immutable / mutable references to different stages'
/// outputs. ~96 KB total when all three stages are active.
pub struct InputPostProcess {
    program_filter: Option<TsProgramFilter>,
    pid_overrides_rewriter: Option<TsPidOverridesRewriter>,
    pid_remapper: Option<TsPidRemapper>,
    scratch_filter: Vec<u8>,
    scratch_overrides: Vec<u8>,
    scratch_remap: Vec<u8>,
}

impl InputPostProcess {
    /// Construct from input config. Returns `None` when no stage has any
    /// work to do — callers should then skip the post-process entirely.
    pub fn from_config(cfg: &InputPostProcessConfig<'_>) -> Option<Self> {
        let program_filter = cfg.program_number.map(TsProgramFilter::new);

        let pid_overrides_rewriter = if cfg.has_transcode {
            None
        } else {
            cfg.pid_overrides.and_then(|m| {
                let r = TsPidOverridesRewriter::new(m);
                if r.is_active() { Some(r) } else { None }
            })
        };

        let pid_remapper = cfg.pid_map.and_then(|m| {
            let r = TsPidRemapper::new(m);
            if r.is_active() { Some(r) } else { None }
        });

        if program_filter.is_none()
            && pid_overrides_rewriter.is_none()
            && pid_remapper.is_none()
        {
            return None;
        }

        Some(Self {
            program_filter,
            pid_overrides_rewriter,
            pid_remapper,
            scratch_filter: Vec::with_capacity(32 * 1024),
            scratch_overrides: Vec::with_capacity(32 * 1024),
            scratch_remap: Vec::with_capacity(32 * 1024),
        })
    }

    /// Run the chain on one chunk of 188-byte-aligned TS bytes. Returns
    /// a borrowed slice valid until the next call.
    ///
    /// Each stage writes into its own dedicated scratch buffer so the
    /// borrow checker stays happy with the chained references.
    pub fn process<'a>(&'a mut self, ts_in: &'a [u8]) -> &'a [u8] {
        // Split the borrow so each stage can hold a &mut to its scratch
        // while the previous stage's scratch is held immutably.
        let Self {
            program_filter,
            pid_overrides_rewriter,
            pid_remapper,
            scratch_filter,
            scratch_overrides,
            scratch_remap,
        } = self;
        scratch_filter.clear();
        scratch_overrides.clear();
        scratch_remap.clear();

        let after_filter: &[u8] = if let Some(f) = program_filter.as_mut() {
            f.filter_into(ts_in, scratch_filter);
            scratch_filter
        } else {
            ts_in
        };

        let after_overrides: &[u8] = if let Some(rw) = pid_overrides_rewriter.as_mut() {
            rw.process(after_filter, scratch_overrides);
            scratch_overrides
        } else {
            after_filter
        };

        if let Some(rm) = pid_remapper.as_mut() {
            rm.process(after_overrides, scratch_remap);
            scratch_remap
        } else {
            after_overrides
        }
    }
}
