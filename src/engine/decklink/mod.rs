//! SDI capture/playout via Blackmagic DeckLink (the `sdi-decklink` feature,
//! off by default).
//!
//! Wraps `bilbycast-decklink-rs`, which talks to the Blackmagic DeckLink SDK
//! directly — not FFmpeg's `decklink` avdevice, which hides
//! `bmdFrameHasNoInputSource` and so makes a pulled cable indistinguishable
//! from a live feed. Boot probe + device manager live in [`domain`]; the
//! boot-time diagnosis of `/dev/blackmagic` it logs when cards are missing
//! lives in [`nodes`]; the per-port hardware status cache lives in [`status`];
//! the flow spawn arms dispatch through `engine::input_sdi` →
//! `engine::sdi_io`. Mirrors the `engine::mxl` layout.
//!
//! This module is also compiled under `test` so that [`nodes`] — plain std +
//! libc — is tested and linted by the default CI run; `domain` and `status`
//! use the optional `decklink_rs` crate and stay behind the feature.

#[cfg(feature = "sdi-decklink")]
pub mod domain;
pub mod nodes;
#[cfg(feature = "sdi-decklink")]
pub mod status;
