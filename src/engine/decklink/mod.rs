//! SDI capture/playout via Blackmagic DeckLink (the `sdi-decklink` feature,
//! off by default).
//!
//! Wraps `bilbycast-decklink-rs`, which talks to the Blackmagic DeckLink SDK
//! directly — not FFmpeg's `decklink` avdevice, which hides
//! `bmdFrameHasNoInputSource` and so makes a pulled cable indistinguishable
//! from a live feed. Boot probe + device manager live in [`domain`]; the
//! per-port hardware status cache lives in [`status`]; the flow spawn arms
//! dispatch through `engine::input_sdi` → `engine::sdi_io`. Mirrors the
//! `engine::mxl` layout.

pub mod domain;
pub mod status;
