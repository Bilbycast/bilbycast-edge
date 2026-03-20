//! SRT connection management.
//!
//! Uses the pure Rust [`srt_transport`] crate for async SRT I/O (no C library
//! dependency).
//!
//! - [`connection`] -- Connect helpers for caller and listener modes with
//!   exponential backoff retry (500ms to 30s cap). Includes convenience wrappers
//!   for input, output, and redundancy leg connections, plus optional AES
//!   encryption setup.

pub mod connection;
