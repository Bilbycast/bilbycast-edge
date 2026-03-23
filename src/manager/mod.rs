// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Manager client module.
//!
//! When enabled, bilbycast-edge maintains a persistent WebSocket connection
//! to a bilbycast-manager instance. This allows centralized monitoring and
//! configuration of edge nodes even when they are behind NAT/firewalls.
//!
//! The edge node initiates the outbound connection, so no inbound ports are needed.

pub mod client;
pub mod config;

pub use config::ManagerConfig;
