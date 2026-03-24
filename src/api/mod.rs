// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! REST API, WebSocket, and Prometheus metrics server.
//!
//! Built on [axum](https://docs.rs/axum). Provides CRUD operations for flow
//! management, real-time statistics via WebSocket, Prometheus-format metrics
//! export, and health checks.
//!
//! All REST endpoints are prefixed with `/api/v1/` except `/health` and `/metrics`.
//! CORS is permissive (all origins). HTTP request tracing is enabled via tower-http.

pub mod auth;
pub mod errors;
pub mod flows;
pub mod models;
pub mod nmos;
pub mod nmos_is05;
pub mod server;
pub mod stats;
pub mod tunnels;
#[cfg(feature = "webrtc")]
pub mod webrtc;
pub mod ws;
