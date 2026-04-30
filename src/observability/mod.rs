// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Edge-side observability outputs that complement the manager WS event
//! stream and the `/metrics` Prometheus surface — currently the structured
//! JSON log shipper for SIEM / NMS pickup (Splunk, Skyline DataMiner, etc).

pub mod log_shipper;

pub use log_shipper::JsonLogShipper;
