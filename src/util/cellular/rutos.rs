// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Teltonika RutOS cellular source — opt-in HTTP telemetry.
//!
//! Reads mobile radio state from a RutOS router (RUT / RUTX / OTD) over its
//! HTTP API using a **read-only** credential. Two transports:
//!   - **ubus JSON-RPC** (`POST /ubus`) — broad RutOS-version compatibility.
//!     Logs in via `session login`, then calls `gsm.modem0 get_signal_query`
//!     (+ `info`) for signal / operator / access-tech.
//!   - **REST** (RutOS 7.x: `POST /api/login` → bearer → `GET /api/modems/status`).
//!
//! Field names vary by model + firmware, so [`json_to_metrics`] is deliberately
//! **tolerant** — it tries multiple key spellings and coerces string-or-number
//! values. The exact RutOS field set is an item to confirm against the live
//! device (see `docs/cellular.md`); the mapping is unit-tested against captured
//! shapes and degrades to fewer fields rather than failing.
//!
//! TLS: RutOS ships a self-signed cert, so `verify_tls` defaults to `false`
//! (accept self-signed) with an optional SHA-256 `cert_fingerprint` pin that is
//! *stronger* than CA validation. The policy is per-uplink — never global.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{
    CellularMetrics, CellularRegState, CellularSignal, CellularSourceKind, derive_bars,
    normalize_access_tech,
};
use crate::config::models::CellularUplinkConfig;

/// A built, ready-to-poll RutOS uplink.
pub struct RutosSource {
    iface: String,
    base_url: String,
    client: reqwest::Client,
    api: RutosApi,
    username: String,
    password: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RutosApi {
    Ubus,
    Rest,
}

/// Build the RutOS sources from config (skipping non-RutOS entries and any that
/// can't produce a TLS client). Keyed by the interface they annotate.
pub fn build_sources(uplinks: &[CellularUplinkConfig]) -> HashMap<String, RutosSource> {
    let mut out = HashMap::new();
    for u in uplinks {
        // Only RutOS entries need a source; modems auto-detect.
        if !u.kind.eq_ignore_ascii_case("rutos") {
            continue;
        }
        if u.interface.trim().is_empty() || u.address.trim().is_empty() {
            continue;
        }
        let api = match u.api.to_ascii_lowercase().as_str() {
            "rest" => RutosApi::Rest,
            _ => RutosApi::Ubus,
        };
        let scheme = if u.scheme.eq_ignore_ascii_case("http") {
            "http"
        } else {
            "https"
        };
        let client = match build_client(u.verify_tls, u.cert_fingerprint.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "cellular uplink '{}': failed to build HTTP client: {e}",
                    u.interface
                );
                continue;
            }
        };
        out.insert(
            u.interface.clone(),
            RutosSource {
                iface: u.interface.clone(),
                base_url: format!("{scheme}://{}", u.address.trim_end_matches('/')),
                client,
                api,
                username: u.username.clone().unwrap_or_default(),
                password: u.password.clone().unwrap_or_default(),
            },
        );
    }
    out
}

/// Build a per-uplink reqwest client honouring its TLS policy.
fn build_client(verify_tls: bool, fingerprint: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(4))
        .user_agent(concat!("bilbycast-edge/", env!("CARGO_PKG_VERSION")));

    if let Some(fp) = fingerprint.filter(|s| !s.trim().is_empty()) {
        // Pin on SHA-256 — accept any cert that matches (RutOS is self-signed,
        // so CA-chain validation is meaningless; the pin is the trust anchor).
        let provider = rustls::crypto::ring::default_provider();
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(FingerprintVerifier::new(fp)))
            .with_no_client_auth();
        builder = builder.use_preconfigured_tls(tls);
    } else if !verify_tls {
        // Accept the self-signed cert without a pin (default for RutOS).
        builder = builder.danger_accept_invalid_certs(true);
    }
    Ok(builder.build()?)
}

impl RutosSource {
    /// Best-effort single sample. Returns `None` on any failure (the poller
    /// loop is the retry; a `None` ages the cache entry out → UI "no data").
    pub async fn sample(&self) -> Option<CellularMetrics> {
        let record = match self.api {
            RutosApi::Ubus => self.sample_ubus().await,
            RutosApi::Rest => self.sample_rest().await,
        }?;
        Some(json_to_metrics(&record))
    }

    /// ubus JSON-RPC: `session login` → token, then merge `gsm.modem0 info` +
    /// `get_signal_query` results into one record.
    async fn sample_ubus(&self) -> Option<Value> {
        let url = format!("{}/ubus", self.base_url);
        // 1. Login.
        let login = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "call",
            "params": ["00000000000000000000000000000000", "session", "login",
                       { "username": self.username, "password": self.password }]
        });
        let resp: Value = self.post_json(&url, &login).await?;
        // result = [status, { ubus_rpc_session, ... }]; status 0 = OK.
        let token = resp
            .get("result")
            .and_then(|r| r.as_array())
            .filter(|a| a.first().and_then(|s| s.as_i64()) == Some(0))
            .and_then(|a| a.get(1))
            .and_then(|o| o.get("ubus_rpc_session"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())?;

        let mut merged = serde_json::Map::new();
        // 2. Pull info + signal; merge (signal wins on overlap).
        for method in ["info", "get_signal_query"] {
            let call = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "call",
                "params": [token, "gsm.modem0", method, {}]
            });
            if let Some(resp) = self.post_json::<Value>(&url, &call).await {
                if let Some(obj) = resp
                    .get("result")
                    .and_then(|r| r.as_array())
                    .filter(|a| a.first().and_then(|s| s.as_i64()) == Some(0))
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.as_object())
                {
                    for (k, v) in obj {
                        merged.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if merged.is_empty() {
            return None;
        }
        Some(Value::Object(merged))
    }

    /// RutOS 7.x REST: `POST /api/login` → bearer → `GET /api/modems/status`.
    async fn sample_rest(&self) -> Option<Value> {
        let login_url = format!("{}/api/login", self.base_url);
        let body = serde_json::json!({ "username": self.username, "password": self.password });
        let resp: Value = self.post_json(&login_url, &body).await?;
        // token under data.token | data.access_token | token.
        let token = resp
            .pointer("/data/token")
            .or_else(|| resp.pointer("/data/access_token"))
            .or_else(|| resp.get("token"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())?;

        let status_url = format!("{}/api/modems/status", self.base_url);
        let resp = self
            .client
            .get(&status_url)
            .bearer_auth(&token)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        // data is usually an array of modems (take the first) or an object.
        let record = match v.get("data") {
            Some(Value::Array(a)) => a.first().cloned(),
            Some(Value::Object(o)) => Some(Value::Object(o.clone())),
            _ => Some(v.clone()),
        };
        record.filter(|r| r.is_object())
    }

    async fn post_json<T: serde::de::DeserializeOwned>(&self, url: &str, body: &Value) -> Option<T> {
        let resp = self.client.post(url).json(body).send().await.ok()?;
        if !resp.status().is_success() {
            tracing::debug!(
                "cellular uplink '{}': {} returned HTTP {}",
                self.iface,
                url,
                resp.status()
            );
            return None;
        }
        resp.json::<T>().await.ok()
    }
}

// ── Tolerant JSON → CellularMetrics mapping (pure, unit-tested) ──

/// Map a RutOS record (ubus merged object or REST modem-status object) to the
/// unified shape. Tolerant of key spelling + string-vs-number values.
pub fn json_to_metrics(record: &Value) -> CellularMetrics {
    // Case-insensitive flattened view: top-level keys plus one level of common
    // nesting (`data` / `modem` / `value`), child keys filling gaps.
    let map = flatten(record);

    let signal = {
        let sig = CellularSignal {
            rsrp_dbm: num(&map, &["rsrp"]),
            rsrq_db: num(&map, &["rsrq"]),
            sinr_db: num(&map, &["sinr", "snr"]),
            rssi_dbm: num(&map, &["rssi", "signal"]),
            bars: None,
        };
        let mut sig = sig;
        sig.bars = derive_bars(&sig);
        let empty = sig.rsrp_dbm.is_none()
            && sig.rsrq_db.is_none()
            && sig.sinr_db.is_none()
            && sig.rssi_dbm.is_none();
        (!empty).then_some(sig)
    };

    let access_tech = text(
        &map,
        &[
            "conn_type",
            "conntype",
            "nettype",
            "net_type",
            "service_mode",
            "mode",
            "network_type",
        ],
    )
    .and_then(|s| normalize_access_tech(&s));

    let roaming = boolean(&map, &["roaming", "roam"]).or_else(|| {
        text(&map, &["service", "netstate", "registration", "reg_status"])
            .map(|s| s.to_ascii_lowercase().contains("roam"))
    });

    let state = derive_state(&map);

    CellularMetrics {
        source: CellularSourceKind::Rutos,
        state,
        access_tech,
        operator: text(&map, &["operator", "operator_name", "opname", "cops"]),
        plmn: text(&map, &["plmn", "operator_number", "cops_numeric", "mccmnc"]),
        band: text(&map, &["band", "current_band", "active_band"]),
        cell_id: text(&map, &["cell_id", "cellid", "ci", "global_cell_id"]),
        signal,
        roaming,
        sim_slot: num(&map, &["sim_slot", "simslot", "sim"]).map(|f| f as u8),
        temperature_c: num(&map, &["temperature", "temp", "modem_temp"]),
        data_used_bytes: num(&map, &["data_used", "data_used_bytes", "usage_bytes"])
            .map(|f| f as u64),
        data_limit_bytes: num(&map, &["data_limit", "data_limit_bytes", "limit_bytes"])
            .map(|f| f as u64),
        sampled_at_unix_ms: None, // stamped by the poller
    }
}

fn derive_state(map: &HashMap<String, Value>) -> CellularRegState {
    // SIM-state hints first.
    if let Some(sim) = text(map, &["sim_state", "simstate"]) {
        let s = sim.to_ascii_lowercase();
        if s.contains("absent") || s.contains("not inserted") || s.contains("missing") {
            return CellularRegState::SimMissing;
        }
        if s.contains("pin") || s.contains("puk") || s.contains("locked") {
            return CellularRegState::SimPinRequired;
        }
    }
    // Registration / service string.
    if let Some(reg) = text(
        map,
        &["service", "netstate", "registration", "reg_status", "state"],
    ) {
        let s = reg.to_ascii_lowercase();
        if s.contains("roam") {
            return CellularRegState::RegisteredRoaming;
        }
        if s.contains("denied") {
            return CellularRegState::Denied;
        }
        if s.contains("search") || s.contains("not registered") {
            return CellularRegState::Searching;
        }
        if s.contains("registered") || s.contains("home") || s.contains("connected") {
            return CellularRegState::RegisteredHome;
        }
    }
    // We got a record with signal but no explicit reg → assume registered.
    if num(map, &["rsrp", "rssi", "sinr"]).is_some() {
        return CellularRegState::RegisteredHome;
    }
    CellularRegState::Unknown
}

/// Build a case-insensitive flattened lookup map: top-level keys plus one level
/// of common nesting (`data` / `modem` / `value`). Top-level keys win.
fn flatten(v: &Value) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let mut absorb = |obj: &serde_json::Map<String, Value>, overwrite: bool| {
        for (k, val) in obj {
            let key = k.to_ascii_lowercase();
            if overwrite || !out.contains_key(&key) {
                out.insert(key, val.clone());
            }
        }
    };
    if let Some(obj) = v.as_object() {
        // Nested containers first (lower priority), then top-level overrides.
        for nest in ["data", "modem", "value"] {
            if let Some(child) = obj.get(nest).and_then(|c| c.as_object()) {
                absorb(child, false);
            }
        }
        absorb(obj, true);
    }
    out
}

/// First present numeric value across the candidate keys. Accepts JSON numbers
/// and numeric strings (`"-95"`, `"-95 dBm"`).
fn num(map: &HashMap<String, Value>, keys: &[&str]) -> Option<f32> {
    for k in keys {
        if let Some(v) = map.get(*k) {
            match v {
                Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        return Some(f as f32);
                    }
                }
                Value::String(s) => {
                    if let Some(f) = parse_leading_number(s) {
                        return Some(f);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn parse_leading_number(s: &str) -> Option<f32> {
    let t = s.trim();
    let mut end = 0;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    t.get(..end).and_then(|p| p.parse::<f32>().ok())
}

/// First present non-empty string across the candidate keys.
fn text(map: &HashMap<String, Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(Value::String(s)) = map.get(*k) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// First present boolean across the candidate keys (bool, 0/1, "true"/"yes"/"1").
fn boolean(map: &HashMap<String, Value>, keys: &[&str]) -> Option<bool> {
    for k in keys {
        match map.get(*k) {
            Some(Value::Bool(b)) => return Some(*b),
            Some(Value::Number(n)) => return Some(n.as_i64().unwrap_or(0) != 0),
            Some(Value::String(s)) => {
                let s = s.trim().to_ascii_lowercase();
                if !s.is_empty() {
                    return Some(matches!(s.as_str(), "1" | "true" | "yes" | "on"));
                }
            }
            _ => {}
        }
    }
    None
}

// ── Fingerprint-pinning TLS verifier (self-signed RutOS, SHA-256 pin) ──

#[derive(Debug)]
struct FingerprintVerifier {
    /// Lower-case hex, no separators.
    expected: String,
}

impl FingerprintVerifier {
    fn new(fp: &str) -> Self {
        Self {
            expected: normalize_fingerprint(fp),
        }
    }
}

/// Strip `:`/whitespace and lower-case so `"AB:CD"` and `"abcd"` compare equal.
fn normalize_fingerprint(fp: &str) -> String {
    fp.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn sha256_hex(der: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = sha256_hex(end_entity.as_ref());
        if actual == self.expected {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "cellular uplink TLS fingerprint mismatch: expected {}, got {actual}",
                self.expected
            )))
        }
    }

    // We pin the leaf cert by SHA-256, but a fingerprint is *public* — it does
    // not prove the peer holds the matching private key. The handshake
    // signature is what proves possession, so we MUST still verify it (against
    // the pinned cert's public key) or a MITM could replay the real cert and
    // sign with their own key. Delegate to the crypto provider.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ubus_signal_query_5g_nsa() {
        // Shape modelled on RutOS `gsm.modem0 get_signal_query` + `info` merged.
        let record = serde_json::json!({
            "rssi": -67, "rsrp": -95, "rsrq": -11, "sinr": 12,
            "conn_type": "5G-NSA", "operator": "Telstra", "band": "n78",
            "cell_id": "0x1A2B3C", "temperature": 41, "service": "registered (home)"
        });
        let m = json_to_metrics(&record);
        assert_eq!(m.source, CellularSourceKind::Rutos);
        assert_eq!(m.state, CellularRegState::RegisteredHome);
        assert_eq!(m.access_tech.as_deref(), Some("5gnr_nsa"));
        assert_eq!(m.operator.as_deref(), Some("Telstra"));
        assert_eq!(m.band.as_deref(), Some("n78"));
        assert_eq!(m.cell_id.as_deref(), Some("0x1A2B3C"));
        assert_eq!(m.temperature_c, Some(41.0));
        let sig = m.signal.unwrap();
        assert_eq!(sig.rsrp_dbm, Some(-95.0));
        assert_eq!(sig.sinr_db, Some(12.0));
        assert!(sig.bars.is_some());
    }

    #[test]
    fn string_numbers_and_units() {
        // Some firmwares return strings, sometimes with a unit suffix.
        let record = serde_json::json!({
            "RSRP": "-105 dBm", "SINR": "3", "conntype": "LTE",
            "service": "Registered, roaming"
        });
        let m = json_to_metrics(&record);
        assert_eq!(m.access_tech.as_deref(), Some("lte"));
        assert_eq!(m.state, CellularRegState::RegisteredRoaming);
        assert_eq!(m.roaming, Some(true));
        let sig = m.signal.unwrap();
        assert_eq!(sig.rsrp_dbm, Some(-105.0));
        assert_eq!(sig.sinr_db, Some(3.0));
    }

    #[test]
    fn rest_data_wrapper_and_sim_missing() {
        // REST `/api/modems/status` style: nested under `data`, SIM absent.
        let record = serde_json::json!({
            "data": { "sim_state": "absent", "net_type": "LTE" }
        });
        let m = json_to_metrics(&record);
        assert_eq!(m.state, CellularRegState::SimMissing);
        assert_eq!(m.access_tech.as_deref(), Some("lte"));
        assert!(m.signal.is_none());
    }

    #[test]
    fn fingerprint_normalization() {
        assert_eq!(normalize_fingerprint("AB:CD:01"), "abcd01");
        assert_eq!(normalize_fingerprint("ab cd 01"), "abcd01");
    }

    // ── Integration: fake RutOS ubus server (axum) ──
    //
    // Exercises the real HTTP path: session login → token, gsm.modem0
    // `info` + `get_signal_query` calls, merge, and mapping. Plus the
    // unreachable case (server 500s) → None.

    use axum::{Json, Router, routing::post};
    use serde_json::Value;

    async fn ubus_ok(Json(req): Json<Value>) -> Json<Value> {
        let params = req.get("params").and_then(|p| p.as_array()).cloned().unwrap_or_default();
        let obj = params.get(1).and_then(|s| s.as_str()).unwrap_or("");
        let method = params.get(2).and_then(|s| s.as_str()).unwrap_or("");
        if obj == "session" && method == "login" {
            return Json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": [0, { "ubus_rpc_session": "TESTSESSIONTOKEN", "timeout": 300 }]
            }));
        }
        if obj == "gsm.modem0" && method == "info" {
            return Json(serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "result": [0, { "operator": "Telstra", "conn_type": "5G-NSA", "service": "registered (home)" }]
            }));
        }
        if obj == "gsm.modem0" && method == "get_signal_query" {
            return Json(serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "result": [0, { "rsrp": -95, "rsrq": -11, "sinr": 12, "rssi": -67, "band": "n78" }]
            }));
        }
        Json(serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": [4] }))
    }

    async fn ubus_500() -> (axum::http::StatusCode, &'static str) {
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
    }

    async fn spawn_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("{}:{}", addr.ip(), addr.port())
    }

    fn http_uplink(address: String) -> CellularUplinkConfig {
        CellularUplinkConfig {
            interface: "eno4".into(),
            kind: "rutos".into(),
            scheme: "http".into(), // plaintext for the test server
            address,
            api: "ubus".into(),
            username: Some("monitor".into()),
            password: Some("pw".into()),
            verify_tls: false,
            cert_fingerprint: None,
        }
    }

    #[tokio::test]
    async fn ubus_end_to_end_maps_signal() {
        let app = Router::new().route("/ubus", post(ubus_ok));
        let addr = spawn_server(app).await;
        let sources = build_sources(&[http_uplink(addr)]);
        let src = sources.get("eno4").expect("source built");
        let m = src.sample().await.expect("sample produced metrics");
        assert_eq!(m.source, CellularSourceKind::Rutos);
        assert_eq!(m.state, CellularRegState::RegisteredHome);
        assert_eq!(m.access_tech.as_deref(), Some("5gnr_nsa"));
        assert_eq!(m.operator.as_deref(), Some("Telstra"));
        assert_eq!(m.band.as_deref(), Some("n78"));
        let sig = m.signal.expect("signal");
        assert_eq!(sig.rsrp_dbm, Some(-95.0));
        assert_eq!(sig.sinr_db, Some(12.0));
    }

    #[tokio::test]
    async fn ubus_server_error_yields_none() {
        // Login endpoint 500s → no token → sample None (ages out of the cache).
        let app = Router::new().route("/ubus", post(ubus_500));
        let addr = spawn_server(app).await;
        let sources = build_sources(&[http_uplink(addr)]);
        let src = sources.get("eno4").unwrap();
        assert!(src.sample().await.is_none());
    }

    #[tokio::test]
    async fn unreachable_host_yields_none() {
        // Nothing listening on this port → connect fails → None.
        let sources = build_sources(&[http_uplink("127.0.0.1:1".into())]);
        let src = sources.get("eno4").unwrap();
        assert!(src.sample().await.is_none());
    }

    #[test]
    fn build_skips_non_rutos_and_blank() {
        let cfgs = vec![
            CellularUplinkConfig {
                interface: "eno4".into(),
                kind: "rutos".into(),
                scheme: "https".into(),
                address: "192.168.1.1".into(),
                api: "ubus".into(),
                username: Some("monitor".into()),
                password: Some("secret".into()),
                verify_tls: false,
                cert_fingerprint: None,
            },
            CellularUplinkConfig {
                interface: "wwan9".into(),
                kind: "modem".into(), // not rutos → skipped
                scheme: "https".into(),
                address: "x".into(),
                api: "ubus".into(),
                username: None,
                password: None,
                verify_tls: false,
                cert_fingerprint: None,
            },
        ];
        let sources = build_sources(&cfgs);
        assert!(sources.contains_key("eno4"));
        assert!(!sources.contains_key("wwan9"));
    }
}
