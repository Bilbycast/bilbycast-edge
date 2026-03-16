//! REST API, WebSocket, and Prometheus metrics server.
//!
//! Built on [axum](https://docs.rs/axum). Provides CRUD operations for flow
//! management, real-time statistics via WebSocket, Prometheus-format metrics
//! export, and health checks.
//!
//! All REST endpoints are prefixed with `/api/v1/` except `/health` and `/metrics`.
//! CORS is permissive (all origins). HTTP request tracing is enabled via tower-http.

pub mod errors;
pub mod flows;
pub mod models;
pub mod server;
pub mod stats;
pub mod ws;
