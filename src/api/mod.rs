// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
pub mod inputs;
pub mod models;
pub mod outputs;
pub mod nmos;
pub mod nmos_is05;
pub mod nmos_is08;
pub mod nmos_mdns;
pub mod nmos_registration;
pub mod server;
pub mod stats;
pub mod tunnels;
#[cfg(feature = "webrtc")]
pub mod webrtc;
pub mod ws;
