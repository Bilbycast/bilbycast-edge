// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `/api/v1/ptp` — operator PTP mode endpoints.
//!
//! Thin REST mirror of the `set_ptp_mode` manager-WS command. Reads
//! and writes `/var/lib/bilbycast/ptp.conf` (the file
//! `bilbycast-ptp-helper` watches at 1 Hz). No daemon control —
//! the helper's mtime poll picks up the new mode within ~1 s.

use axum::extract::Json;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::api::errors::ApiError;
use crate::api::models::ApiResponse;
use crate::util::ptp_config::{self, PtpMode, PtpSettings};

#[derive(Debug, Clone, Deserialize)]
pub struct SetPtpRequest {
    pub mode: String,
    #[serde(default)]
    pub iface: String,
    #[serde(default)]
    pub domain: Option<u8>,
    #[serde(default)]
    pub priority1: Option<u8>,
    #[serde(default)]
    pub scan_timeout: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PtpStatusPayload {
    pub mode: &'static str,
    pub iface: String,
    pub domain: Option<u8>,
    pub priority1: Option<u8>,
    pub scan_timeout: Option<u8>,
    pub config_path: String,
}

impl From<PtpSettings> for PtpStatusPayload {
    fn from(s: PtpSettings) -> Self {
        Self {
            mode: s.mode.as_str(),
            iface: s.iface,
            domain: s.domain,
            priority1: s.priority1,
            scan_timeout: s.scan_timeout,
            config_path: ptp_config::config_path().display().to_string(),
        }
    }
}

pub async fn get_ptp() -> impl IntoResponse {
    Json(ApiResponse::ok(PtpStatusPayload::from(ptp_config::load())))
}

pub async fn put_ptp(
    Json(req): Json<SetPtpRequest>,
) -> Result<Json<ApiResponse<PtpStatusPayload>>, ApiError> {
    let mode = match req.mode.to_ascii_lowercase().as_str() {
        "auto" => PtpMode::Auto,
        "grandmaster" | "gm" | "master" => PtpMode::Grandmaster,
        "slave-only" | "slave" => PtpMode::SlaveOnly,
        "off" | "disabled" | "none" => PtpMode::Off,
        other => {
            return Err(ApiError::BadRequest(format!(
                "Unknown mode '{other}' (expected auto, grandmaster, slave-only, off)"
            )));
        }
    };
    let settings = PtpSettings {
        mode,
        iface: req.iface,
        domain: req.domain,
        priority1: req.priority1,
        scan_timeout: req.scan_timeout,
    }
    .normalised();
    settings
        .validate()
        .map_err(ApiError::BadRequest)?;
    ptp_config::save(&settings)
        .map_err(|e| ApiError::Internal(format!("Cannot persist PTP config: {e}")))?;
    Ok(Json(ApiResponse::ok(PtpStatusPayload::from(settings))))
}
