// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Configuration models, JSON file persistence, and validation.
//!
//! The config file (`config.json`) defines server settings and flow definitions.
//! Flows can also be created and modified at runtime via the REST API, which
//! persists changes back to disk atomically.
//!
//! - [`models`] -- All config structs (`AppConfig`, `FlowConfig`, `InputConfig`, etc.)
//! - [`persistence`] -- Load/save config as JSON with atomic file writes
//! - [`validation`] -- Validates config fields (addresses, SRT modes, FEC params, etc.)

pub mod crypto;
pub mod models;
pub mod persistence;
pub mod secrets;
pub mod validation;
