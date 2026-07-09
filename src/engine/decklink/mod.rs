//! SDI capture/playout via Blackmagic DeckLink (the `sdi-decklink` feature,
//! off by default).
//!
//! Wraps `bilbycast-decklink-rs` (FFmpeg's `decklink` avdevice). Boot probe +
//! device manager live in [`domain`]; the flow spawn arms dispatch through
//! `engine::input_sdi` → `engine::sdi_io`. Mirrors the `engine::mxl` layout.

pub mod domain;
