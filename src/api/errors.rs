// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Error type for all API handlers, convertible into an HTTP response.
///
/// Each variant maps to a specific HTTP status code and carries a human-readable
/// error message that is included in the JSON response body as `{ "success": false, "error": "..." }`.
pub enum ApiError {
    /// The requested resource was not found. Maps to HTTP `404 Not Found`.
    /// Used when a flow ID or output ID does not exist in the configuration.
    NotFound(String),
    /// The request was malformed or failed validation. Maps to HTTP `400 Bad Request`.
    /// Used for invalid config payloads, empty IDs, bad addresses, etc.
    BadRequest(String),
    /// The request conflicts with the current state of the resource. Maps to HTTP `409 Conflict`.
    /// Used when attempting to create a flow/output that already exists, start an
    /// already-running flow, or stop an already-stopped flow.
    Conflict(String),
    /// An unexpected server-side error occurred. Maps to HTTP `500 Internal Server Error`.
    /// Used for disk I/O failures, engine errors, and other unrecoverable conditions.
    Internal(String),
}

/// Converts an [`ApiError`] into an Axum HTTP [`Response`].
///
/// The response body is a JSON object with `"success": false` and an `"error"` field
/// containing the error message. The HTTP status code is determined by the variant:
/// `NotFound` -> 404, `BadRequest` -> 400, `Conflict` -> 409, `Internal` -> 500.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "success": false, "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err.to_string())
    }
}
