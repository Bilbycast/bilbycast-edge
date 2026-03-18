//! OAuth 2.0 Client Credentials + JWT (HS256) + RBAC authentication for bilbycast-edge.
//!
//! Provides:
//! - JWT encode/decode using HMAC-SHA256
//! - Auth middleware for axum that validates Bearer tokens
//! - Role-based access control via the [`RequireAdmin`] extractor
//! - OAuth 2.0 `/oauth/token` endpoint (client_credentials grant)
//!
//! When [`AuthConfig::enabled`] is `false` (or no `AuthState` is provided), all
//! requests pass through without authentication.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::errors::ApiError;

// ---------------------------------------------------------------------------
// Base64 URL-safe engine (no padding)
// ---------------------------------------------------------------------------

fn b64() -> &'static base64::engine::GeneralPurpose {
    &base64::engine::general_purpose::URL_SAFE_NO_PAD
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

fn default_token_lifetime() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

/// Top-level authentication configuration block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Master switch — when `false`, auth middleware is a no-op.
    pub enabled: bool,
    /// HMAC-SHA256 secret used to sign and verify JWTs.
    pub jwt_secret: String,
    /// Token lifetime in seconds (default 3600 = 1 hour).
    #[serde(default = "default_token_lifetime")]
    pub token_lifetime_secs: u64,
    /// Registered OAuth clients.
    pub clients: Vec<AuthClient>,
    /// When `true`, unauthenticated requests to `/metrics` and `/health` are allowed.
    #[serde(default = "default_true")]
    pub public_metrics: bool,
}

/// A single registered OAuth client (client_credentials grant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthClient {
    pub client_id: String,
    pub client_secret: String,
    /// `"admin"` or `"monitor"`.
    pub role: String,
}

// ---------------------------------------------------------------------------
// JWT Claims
// ---------------------------------------------------------------------------

/// JWT payload claims. Inserted into axum request extensions by the auth middleware
/// so downstream handlers and extractors can access the authenticated identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: String,
    pub iat: u64,
    pub exp: u64,
    pub iss: String,
}

// ---------------------------------------------------------------------------
// AuthState — runtime auth state
// ---------------------------------------------------------------------------

/// Runtime authentication state, built once at startup from [`AuthConfig`].
pub struct AuthState {
    pub config: AuthConfig,
    pub hmac_key: Vec<u8>,
}

impl AuthState {
    /// Build an [`AuthState`] from the supplied config.
    pub fn new(config: AuthConfig) -> Self {
        let hmac_key = config.jwt_secret.as_bytes().to_vec();
        Self { config, hmac_key }
    }
}

// ---------------------------------------------------------------------------
// JWT encode / decode
// ---------------------------------------------------------------------------

/// Static base64url-encoded JWT header for HS256.
const JWT_HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

/// Encode a [`Claims`] struct into a signed JWT string.
pub fn jwt_encode(claims: &Claims, secret: &[u8]) -> Result<String, String> {
    let payload_json =
        serde_json::to_string(claims).map_err(|e| format!("claims serialization failed: {e}"))?;
    let payload_b64 = b64().encode(payload_json.as_bytes());

    let signing_input = format!("{JWT_HEADER_B64}.{payload_b64}");

    let mut mac = <Hmac<Sha256>>::new_from_slice(secret)
        .map_err(|e| format!("HMAC key error: {e}"))?;
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    let sig_b64 = b64().encode(&signature);

    Ok(format!("{signing_input}.{sig_b64}"))
}

/// Decode and verify a JWT string, returning the contained [`Claims`].
///
/// Checks:
/// 1. Structural validity (three dot-separated parts)
/// 2. HMAC-SHA256 signature
/// 3. Token expiry (`exp` claim vs current time)
pub fn jwt_decode(token: &str, secret: &[u8]) -> Result<Claims, String> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        return Err("malformed JWT: expected 3 parts".into());
    }

    let header_b64 = parts[0];
    let payload_b64 = parts[1];
    let sig_b64 = parts[2];

    // Verify header matches expected HS256 header.
    if header_b64 != JWT_HEADER_B64 {
        return Err("unsupported JWT header (only HS256 is supported)".into());
    }

    // Verify signature.
    let signing_input = format!("{header_b64}.{payload_b64}");
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret)
        .map_err(|e| format!("HMAC key error: {e}"))?;
    mac.update(signing_input.as_bytes());

    let sig_bytes = b64()
        .decode(sig_b64)
        .map_err(|e| format!("bad signature encoding: {e}"))?;
    mac.verify_slice(&sig_bytes)
        .map_err(|_| "invalid JWT signature".to_string())?;

    // Decode payload.
    let payload_bytes = b64()
        .decode(payload_b64)
        .map_err(|e| format!("bad payload encoding: {e}"))?;
    let claims: Claims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| format!("invalid JWT payload: {e}"))?;

    // Check expiry.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now > claims.exp {
        return Err("token expired".into());
    }

    Ok(claims)
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

/// JSON error body returned by auth middleware and extractors.
fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "success": false, "error": message })),
    )
        .into_response()
}

/// Axum middleware that validates JWT Bearer tokens.
///
/// When auth is disabled (`State` is `None`), all requests pass through unchanged.
pub async fn auth_middleware(
    State(auth): State<Option<Arc<AuthState>>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(auth) = auth else {
        // Auth disabled — pass through.
        return next.run(req).await;
    };

    // Extract the Authorization header.
    let token = match req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(val) if val.starts_with("Bearer ") => &val[7..],
        Some(_) => return json_error(StatusCode::UNAUTHORIZED, "invalid authorization scheme, expected Bearer"),
        None => return json_error(StatusCode::UNAUTHORIZED, "missing Authorization header"),
    };

    // Validate the JWT.
    match jwt_decode(token, &auth.hmac_key) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(e) => json_error(StatusCode::UNAUTHORIZED, &e),
    }
}

// ---------------------------------------------------------------------------
// RequireAdmin extractor
// ---------------------------------------------------------------------------

/// Axum extractor that requires the authenticated user to have the `"admin"` role.
///
/// Extracts [`Claims`] from request extensions (inserted by [`auth_middleware`])
/// and returns 403 if the role is not `"admin"`.
pub struct RequireAdmin(pub Claims);

impl<S: Send + Sync> FromRequestParts<S> for RequireAdmin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // If no Claims in extensions, auth middleware didn't run (auth disabled) — allow through
        let Some(claims) = parts.extensions.get::<Claims>().cloned() else {
            return Ok(RequireAdmin(Claims {
                sub: "anonymous".to_string(),
                role: "admin".to_string(),
                iat: 0,
                exp: 0,
                iss: "none".to_string(),
            }));
        };

        if claims.role != "admin" {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "admin role required",
            ));
        }

        Ok(RequireAdmin(claims))
    }
}

// ---------------------------------------------------------------------------
// OAuth 2.0 token endpoint
// ---------------------------------------------------------------------------

/// Request parameters for the `/oauth/token` endpoint.
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub client_id: String,
    pub client_secret: String,
}

/// Successful token response.
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub role: String,
}

/// OAuth 2.0 token endpoint handler.
///
/// Accepts `application/x-www-form-urlencoded` or `application/json` bodies with
/// `grant_type=client_credentials`, `client_id`, and `client_secret`.
///
/// On success, returns a signed JWT with the client's role.
pub async fn oauth_token_handler(
    State(state): State<super::server::AppState>,
    body: axum::body::Bytes,
) -> Result<Json<TokenResponse>, ApiError> {
    let auth = state.auth_state.as_ref()
        .ok_or_else(|| ApiError::BadRequest("authentication is not enabled".into()))?;

    // Try to parse as form-urlencoded first, then as JSON.
    let params: TokenRequest = serde_urlencoded::from_bytes(&body)
        .or_else(|_| serde_json::from_slice(&body))
        .map_err(|e| {
            ApiError::BadRequest(format!(
                "failed to parse token request (expected form or JSON body): {e}"
            ))
        })?;

    if params.grant_type != "client_credentials" {
        return Err(ApiError::BadRequest(
            "unsupported grant_type, expected client_credentials".into(),
        ));
    }

    // Look up client.
    let client = auth
        .config
        .clients
        .iter()
        .find(|c| c.client_id == params.client_id && c.client_secret == params.client_secret)
        .ok_or_else(|| ApiError::BadRequest("invalid client credentials".into()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = Claims {
        sub: client.client_id.clone(),
        role: client.role.clone(),
        iat: now,
        exp: now + auth.config.token_lifetime_secs,
        iss: "bilbycast-edge".to_string(),
    };

    let token = jwt_encode(&claims, &auth.hmac_key)
        .map_err(|e| ApiError::Internal(format!("JWT signing failed: {e}")))?;

    Ok(Json(TokenResponse {
        access_token: token,
        token_type: "bearer".to_string(),
        expires_in: auth.config.token_lifetime_secs,
        role: client.role.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &[u8] = b"super-secret-key-for-tests";

    fn make_claims(role: &str, lifetime_secs: u64) -> Claims {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Claims {
            sub: "test-client".to_string(),
            role: role.to_string(),
            iat: now,
            exp: now + lifetime_secs,
            iss: "bilbycast-edge".to_string(),
        }
    }

    #[test]
    fn jwt_round_trip_admin() {
        let claims = make_claims("admin", 3600);
        let token = jwt_encode(&claims, TEST_SECRET).expect("encode should succeed");

        // Token should have three dot-separated parts.
        assert_eq!(token.matches('.').count(), 2);

        let decoded = jwt_decode(&token, TEST_SECRET).expect("decode should succeed");
        assert_eq!(decoded.sub, "test-client");
        assert_eq!(decoded.role, "admin");
        assert_eq!(decoded.iss, "bilbycast-edge");
        assert_eq!(decoded.iat, claims.iat);
        assert_eq!(decoded.exp, claims.exp);
    }

    #[test]
    fn jwt_round_trip_monitor() {
        let claims = make_claims("monitor", 7200);
        let token = jwt_encode(&claims, TEST_SECRET).expect("encode should succeed");
        let decoded = jwt_decode(&token, TEST_SECRET).expect("decode should succeed");
        assert_eq!(decoded.role, "monitor");
    }

    #[test]
    fn jwt_decode_wrong_secret_fails() {
        let claims = make_claims("admin", 3600);
        let token = jwt_encode(&claims, TEST_SECRET).expect("encode should succeed");
        let result = jwt_decode(&token, b"wrong-secret");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid JWT signature"));
    }

    #[test]
    fn jwt_decode_expired_token_fails() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = Claims {
            sub: "test-client".to_string(),
            role: "admin".to_string(),
            iat: now - 7200,
            exp: now - 3600, // expired an hour ago
            iss: "bilbycast-edge".to_string(),
        };
        let token = jwt_encode(&claims, TEST_SECRET).expect("encode should succeed");
        let result = jwt_decode(&token, TEST_SECRET);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }

    #[test]
    fn jwt_decode_malformed_token_fails() {
        let result = jwt_decode("not.a.valid.token", TEST_SECRET);
        assert!(result.is_err());
    }

    #[test]
    fn jwt_decode_tampered_payload_fails() {
        let claims = make_claims("admin", 3600);
        let token = jwt_encode(&claims, TEST_SECRET).expect("encode should succeed");

        // Tamper with the payload by replacing it.
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        let tampered_payload = b64().encode(b"{\"sub\":\"hacker\",\"role\":\"admin\",\"iat\":0,\"exp\":9999999999,\"iss\":\"bilbycast-edge\"}");
        let tampered_token = format!("{}.{}.{}", parts[0], tampered_payload, parts[2]);

        let result = jwt_decode(&tampered_token, TEST_SECRET);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid JWT signature"));
    }

    #[test]
    fn jwt_header_is_correct() {
        // Verify the hardcoded header constant matches the expected JSON.
        let decoded = b64().decode(JWT_HEADER_B64).expect("header should decode");
        let header: serde_json::Value =
            serde_json::from_slice(&decoded).expect("header should be valid JSON");
        assert_eq!(header["alg"], "HS256");
        assert_eq!(header["typ"], "JWT");
    }
}
