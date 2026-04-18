// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Statistics collection using lock-free atomic counters.
//!
//! - [`collector::StatsCollector`] -- Global registry of per-flow accumulators,
//!   backed by a [`DashMap`](dashmap::DashMap) for thread-safe concurrent access.
//! - [`collector::FlowStatsAccumulator`] / [`collector::OutputStatsAccumulator`] --
//!   `AtomicU64` counters for packets, bytes, loss, FEC recovery, and redundancy
//!   events. Updated in the hot path without locking.
//! - [`models`] -- Serializable snapshot structs (`FlowStats`, `InputStats`,
//!   `OutputStats`, `SrtLegStats`) for API, WebSocket, and Prometheus export.
//! - [`throughput::ThroughputEstimator`] -- Computes bits-per-second from periodic
//!   byte counter samples using delta-over-time calculation.

pub mod collector;
pub mod models;
pub mod throughput;
