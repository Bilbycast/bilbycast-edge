// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Starlink dish gRPC client — hand-rolled, no tonic/prost.
//!
//! The dish exposes an **unauthenticated** gRPC service over **cleartext
//! HTTP/2** (h2c) at `192.168.100.1:9200`:
//!
//! ```text
//! service SpaceX.API.Device.Device { rpc Handle(Request) returns (Response); }
//! ```
//!
//! We only need one read-only call — `get_status` — so rather than pull in the
//! whole tonic/prost stack (and a build-time `.proto` compile) for one request,
//! the request is a fixed 8-byte gRPC frame and the response is parsed by a
//! tolerant protobuf field-walker ([`PbReader`]) that skips anything it does not
//! recognise. This keeps the dependency surface to `reqwest` (already vendored;
//! the `http2` feature carries the h2c support) and stays trivially pure-Rust
//! for `check-binary-purity.sh`.
//!
//! ## Wire format (verified live + against ewilken/starlink-rs + danopstech)
//!
//! - **Request:** `Request{ get_status = GetStatusRequest{} }` — field **1004**,
//!   wire type 2 (length-delimited), empty embedded message. As a gRPC
//!   length-prefixed message: `00 00 00 00 03 E2 3E 00`
//!   (flag=0, len=3, tag `E2 3E` = (1004<<3)|2, embedded length `00`).
//! - **Response:** `Response{ dish_get_status = DishGetStatusResponse{} }` —
//!   field **2004**. Inside `DishGetStatusResponse`:
//!   `device_info`=1, `device_state`=2, `snr`=1001 (legacy),
//!   `seconds_to_first_nonempty_slot`=1002, `pop_ping_drop_rate`=1003,
//!   `obstruction_stats`=1004, `alerts`=1005, `state`=1006 (enum),
//!   `downlink_throughput_bps`=1007, `uplink_throughput_bps`=1008,
//!   `pop_ping_latency_ms`=1009.
//!   `DeviceInfo{ id=1, hardware_version=2, software_version=3, country_code=4 }`,
//!   `DeviceState{ uptime_s=1 }`,
//!   `DishObstructionStats{ fraction_obstructed=1, currently_obstructed=5 }`,
//!   `DishAlerts{ motors_stuck=1, thermal_shutdown=2, thermal_throttle=3,
//!   unexpected_location=4, mast_not_near_vertical=5, slow_ethernet_speeds=6 }`.
//!
//! These are reverse-engineered (not SpaceX-published); newer firmware adds
//! fields, so the decoder is deliberately additive — unknown fields are skipped.

use std::collections::HashMap;
use std::time::Duration;

use super::{StarlinkMetrics, StarlinkState};
use crate::config::models::{DEFAULT_DISH_ADDR, StarlinkUplinkConfig};

// ── Wire constants ──

/// `Response.dish_get_status` field number (oneof `response`).
const RESP_DISH_GET_STATUS_FIELD: u32 = 2004;
/// Complete gRPC length-prefixed `Request{ get_status: {} }` (field 1004).
const GET_STATUS_FRAME: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x03, 0xE2, 0x3E, 0x00];
/// gRPC unary method path.
const HANDLE_PATH: &str = "/SpaceX.API.Device.Device/Handle";

// `DishAlerts` bool field → human name. Unknown true bits surface as `alert_<n>`
// (forward-compat — every field in the alerts message is a bool fault flag).
const ALERT_NAMES: &[(u32, &str)] = &[
    (1, "motors_stuck"),
    (2, "thermal_shutdown"),
    (3, "thermal_throttle"),
    (4, "unexpected_location"),
    (5, "mast_not_near_vertical"),
    (6, "slow_ethernet_speeds"),
];

// ── Minimal protobuf reader (tolerant field walker) ──

/// One decoded protobuf field value. Only the four wire types the dish uses are
/// represented; group wire types (3/4) stop iteration (they never appear here).
enum PbValue<'a> {
    Varint(u64),
    #[allow(dead_code)]
    Fixed64([u8; 8]),
    Len(&'a [u8]),
    Fixed32([u8; 4]),
}

impl<'a> PbValue<'a> {
    fn as_f32(&self) -> Option<f32> {
        match self {
            PbValue::Fixed32(b) => Some(f32::from_le_bytes(*b)),
            _ => None,
        }
    }
    fn as_u64(&self) -> Option<u64> {
        match self {
            PbValue::Varint(v) => Some(*v),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            PbValue::Varint(v) => Some(*v != 0),
            _ => None,
        }
    }
    fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            PbValue::Len(b) => Some(b),
            _ => None,
        }
    }
    fn as_string(&self) -> Option<String> {
        match self {
            PbValue::Len(b) => std::str::from_utf8(b).ok().map(|s| s.to_string()),
            _ => None,
        }
    }
}

/// Cursor over a protobuf message body, yielding `(field_number, value)` pairs.
/// Stops on a truncated / malformed field rather than panicking, so a partial or
/// unexpected response degrades to "fewer fields decoded".
struct PbReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PbReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None; // overlong varint → malformed
            }
        }
    }

    fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
}

impl<'a> Iterator for PbReader<'a> {
    type Item = (u32, PbValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x7) as u8;
        let val = match wire {
            0 => PbValue::Varint(self.read_varint()?),
            1 => {
                let b = self.read_bytes(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                PbValue::Fixed64(a)
            }
            2 => {
                let len = self.read_varint()? as usize;
                PbValue::Len(self.read_bytes(len)?)
            }
            5 => {
                let b = self.read_bytes(4)?;
                let mut a = [0u8; 4];
                a.copy_from_slice(b);
                PbValue::Fixed32(a)
            }
            _ => return None, // groups / unknown wire type → stop
        };
        Some((field, val))
    }
}

/// First length-delimited field with the given number, if present.
fn find_len_field(buf: &[u8], field: u32) -> Option<&[u8]> {
    PbReader::new(buf).find_map(|(f, v)| if f == field { v.as_bytes() } else { None })
}

// ── DishGetStatusResponse decoding ──

fn decode_device_info(
    buf: &[u8],
) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    let (mut id, mut hw, mut sw, mut cc) = (None, None, None, None);
    for (f, v) in PbReader::new(buf) {
        match f {
            1 => id = v.as_string(),
            2 => hw = v.as_string(),
            3 => sw = v.as_string(),
            4 => cc = v.as_string(),
            _ => {}
        }
    }
    (id, hw, sw, cc)
}

fn decode_device_state_uptime(buf: &[u8]) -> Option<u64> {
    PbReader::new(buf).find_map(|(f, v)| if f == 1 { v.as_u64() } else { None })
}

fn decode_obstruction(buf: &[u8]) -> (Option<f32>, Option<bool>) {
    let (mut frac, mut now) = (None, None);
    for (f, v) in PbReader::new(buf) {
        match f {
            1 => frac = v.as_f32(),
            5 => now = v.as_bool(),
            _ => {}
        }
    }
    (frac, now)
}

fn decode_alerts(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for (f, v) in PbReader::new(buf) {
        if v.as_bool() == Some(true) {
            match ALERT_NAMES.iter().find(|(n, _)| *n == f) {
                Some((_, name)) => out.push((*name).to_string()),
                None => out.push(format!("alert_{f}")),
            }
        }
    }
    out
}

/// Decode a `DishGetStatusResponse` body into [`StarlinkMetrics`]. Pure +
/// tolerant (unknown fields skipped); `bars` are derived from the result.
fn decode_dish_get_status(buf: &[u8]) -> StarlinkMetrics {
    let mut m = StarlinkMetrics {
        state: StarlinkState::Unknown,
        obstruction_fraction: None,
        currently_obstructed: None,
        downlink_bps: None,
        uplink_bps: None,
        pop_ping_latency_ms: None,
        pop_ping_drop_rate: None,
        seconds_to_first_nonempty_slot: None,
        snr: None,
        uptime_s: None,
        alerts: Vec::new(),
        device_id: None,
        hardware_version: None,
        software_version: None,
        country_code: None,
        bars: None,
        last_error: None,
        sampled_at_unix_ms: None,
    };
    // Whether the explicit DishState field (1006) was present. Newer firmware
    // (Starlink Mini, 2026.06+) omits it from dish_get_status while connected,
    // so we infer connectivity below when it's absent.
    let mut state_explicit = false;
    for (f, v) in PbReader::new(buf) {
        match f {
            1 => {
                if let Some(b) = v.as_bytes() {
                    let (id, hw, sw, cc) = decode_device_info(b);
                    m.device_id = id;
                    m.hardware_version = hw;
                    m.software_version = sw;
                    m.country_code = cc;
                }
            }
            2 => {
                if let Some(b) = v.as_bytes() {
                    m.uptime_s = decode_device_state_uptime(b);
                }
            }
            1001 => m.snr = v.as_f32(),
            1002 => {
                // Sanity-bound: recent firmware repurposes field 1002 to an
                // implausible value (~1.78e9), so only accept a realistic
                // "seconds until obstruction" figure; otherwise treat as absent.
                m.seconds_to_first_nonempty_slot =
                    v.as_f32().filter(|s| s.is_finite() && *s >= 0.0 && *s < 1.0e6);
            }
            1003 => m.pop_ping_drop_rate = v.as_f32(),
            1004 => {
                if let Some(b) = v.as_bytes() {
                    let (frac, now) = decode_obstruction(b);
                    m.obstruction_fraction = frac;
                    m.currently_obstructed = now;
                }
            }
            1005 => {
                if let Some(b) = v.as_bytes() {
                    m.alerts = decode_alerts(b);
                }
            }
            1006 => {
                if let Some(s) = v.as_u64() {
                    m.state = StarlinkState::from_proto(s as i64);
                    state_explicit = true;
                }
            }
            1007 => m.downlink_bps = v.as_f32(),
            1008 => m.uplink_bps = v.as_f32(),
            1009 => m.pop_ping_latency_ms = v.as_f32(),
            _ => {}
        }
    }
    // When the dish omits the explicit DishState (newer firmware does, while
    // connected), infer connectivity from observable health: a PoP-ping RTT only
    // exists once the dish is locked and passing traffic, so treat its presence
    // as Connected. Otherwise leave the decoded/Unknown state as-is.
    if !state_explicit && m.pop_ping_latency_ms.is_some() {
        m.state = StarlinkState::Connected;
    }
    m.bars = super::derive_bars(&m);
    m
}

/// Parse a complete gRPC length-prefixed `Response` frame (the bytes reqwest
/// returns as the response body) into [`StarlinkMetrics`]. Returns a
/// human-readable `Err` on a malformed frame or a response that carries no
/// `dish_get_status` (e.g. the router endpoint, which returns `wifi_get_status`).
pub fn parse_get_status_response(frame: &[u8]) -> Result<StarlinkMetrics, String> {
    if frame.len() < 5 {
        return Err(format!("gRPC response too short ({} bytes)", frame.len()));
    }
    if frame[0] != 0 {
        return Err("dish returned a compressed gRPC message (expected identity)".to_string());
    }
    let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
    let body = frame.get(5..5 + len).ok_or_else(|| {
        format!(
            "gRPC length prefix ({len}) exceeds {} body bytes",
            frame.len().saturating_sub(5)
        )
    })?;
    let dish = find_len_field(body, RESP_DISH_GET_STATUS_FIELD).ok_or_else(|| {
        "response carried no dish_get_status — point this at the dish gRPC \
         (192.168.100.1:9200), not the Starlink router endpoint (:9000)"
            .to_string()
    })?;
    Ok(decode_dish_get_status(dish))
}

// ── HTTP/2 transport ──

/// A built, ready-to-poll Starlink dish source.
pub struct StarlinkSource {
    url: String,
    client: reqwest::Client,
}

/// Build the dish sources from config, keyed by the interface they annotate.
/// Skips blank interfaces and any client that fails to build.
pub fn build_sources(uplinks: &[StarlinkUplinkConfig]) -> HashMap<String, StarlinkSource> {
    let mut out = HashMap::new();
    for u in uplinks {
        if u.interface.trim().is_empty() {
            continue;
        }
        let addr = normalize_address(&u.address);
        let client = match build_client(u.source_address.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "starlink uplink '{}': failed to build HTTP/2 client: {e}",
                    u.interface
                );
                continue;
            }
        };
        out.insert(
            u.interface.clone(),
            StarlinkSource {
                url: format!("http://{addr}{HANDLE_PATH}"),
                client,
            },
        );
    }
    out
}

/// Normalise a config address into `host:port`, defaulting the port to 9200.
fn normalize_address(address: &str) -> String {
    let a = address.trim().trim_end_matches('/');
    if a.is_empty() {
        return DEFAULT_DISH_ADDR.to_string();
    }
    // Bracketed IPv6 (`[::1]:9200` / `[::1]`) or a host already carrying a port.
    if a.starts_with('[') {
        if a.ends_with(']') {
            return format!("{a}:9200");
        }
        return a.to_string();
    }
    if a.contains(':') {
        a.to_string()
    } else {
        format!("{a}:9200")
    }
}

/// HTTP/2-prior-knowledge (h2c) client — the dish speaks cleartext HTTP/2 with
/// no TLS, so no cert handling is needed (unlike the RutOS source).
///
/// `source_address`, when set, binds the poll to that local IP via
/// `local_address`. This is what lets several dishes that all share the fixed
/// `192.168.100.1:9200` be polled over different interfaces: pair each leg's
/// source IP here with a host policy route (`ip rule from <src> table N`).
/// Unprivileged; a no-op (today's behaviour) when unset.
fn build_client(source_address: Option<&str>) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .http2_prior_knowledge()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("bilbycast-edge/", env!("CARGO_PKG_VERSION")));
    if let Some(ip) = source_address
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
    {
        builder = builder.local_address(ip);
    }
    builder.build()
}

impl StarlinkSource {
    /// Single sample. `Ok(metrics)` on success; `Err(reason)` carries a
    /// human-readable failure cause (surfaced on the failing-uplink UI row and
    /// the "Test reachability" button) rather than a silent `None`.
    pub async fn sample(&self) -> Result<StarlinkMetrics, String> {
        let resp = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/grpc")
            .header("te", "trailers")
            .header("grpc-encoding", "identity")
            .body(GET_STATUS_FRAME.to_vec())
            .send()
            .await
            .map_err(|e| format!("dish gRPC request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("dish gRPC returned HTTP {}", resp.status()));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("reading dish gRPC response failed: {e}"))?;
        if body.is_empty() {
            return Err(
                "dish returned an empty gRPC response (check the endpoint / grpc-status)"
                    .to_string(),
            );
        }
        parse_get_status_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tiny protobuf encoder for synthesising a response ──
    fn enc_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    fn tag(out: &mut Vec<u8>, field: u32, wire: u8) {
        enc_varint(out, ((field as u64) << 3) | wire as u64);
    }
    fn put_f32(out: &mut Vec<u8>, field: u32, v: f32) {
        tag(out, field, 5);
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_varint(out: &mut Vec<u8>, field: u32, v: u64) {
        tag(out, field, 0);
        enc_varint(out, v);
    }
    fn put_len(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
        tag(out, field, 2);
        enc_varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }
    fn put_str(out: &mut Vec<u8>, field: u32, s: &str) {
        put_len(out, field, s.as_bytes());
    }
    /// Wrap a protobuf body in a single uncompressed gRPC length-prefixed frame.
    fn grpc_frame(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn varint_and_fixed32_reader() {
        // field 1 varint = 300 (0xAC 0x02), field 2 fixed32 = 1.5f.
        let mut buf = Vec::new();
        put_varint(&mut buf, 1, 300);
        put_f32(&mut buf, 2, 1.5);
        let mut r = PbReader::new(&buf);
        let (f1, v1) = r.next().unwrap();
        assert_eq!((f1, v1.as_u64()), (1, Some(300)));
        let (f2, v2) = r.next().unwrap();
        assert_eq!(f2, 2);
        assert_eq!(v2.as_f32(), Some(1.5));
        assert!(r.next().is_none());
    }

    #[test]
    fn truncated_message_does_not_panic() {
        // A length-delimited field claiming more bytes than present → reader
        // stops cleanly instead of panicking / over-reading.
        let bytes = hex("0a ff ff ff 7f"); // field 1, len ~268M, no payload
        let collected: Vec<_> = PbReader::new(&bytes).collect();
        assert!(collected.is_empty());
    }

    #[test]
    fn device_info_decode_from_live_golden_capture() {
        // Real `get_device_info` Response captured live from the Starlink Mini
        // at 192.168.4.1:9000 (router endpoint). Response field 1004 =
        // GetDeviceInfoResponse, whose field 1 = the common DeviceInfo message —
        // the SAME type embedded as field 1 inside dish_get_status. This pins the
        // DeviceInfo field numbers against real device bytes.
        let resp = hex(
            "0000 00 00 a5 18 6d e2 3e 9f 01 0a 9c 01 0a 1f \
             52 6f 75 74 65 72 2d 30 31 30 30 30 30 30 30 30 \
             30 30 30 30 30 30 30 30 31 45 44 33 43 31 44 12 \
             02 76 34 1a 12 32 30 32 35 2e 31 30 2e 30 33 2e \
             6d 72 36 31 38 32 31 22 02 41 55 40 07 48 01 5a \
             12 32 30 32 35 2e 31 30 2e 30 33 2e 6d 72 36 31 \
             38 32 31 68 01 70 03 ca 3e 40 0a 04 08 05 10 01 \
             0a 04 08 01 10 01 0a 04 08 00 10 05 18 07 3a 14 \
             32 30 32 36 2e 30 36 2e 31 31 2e 6d 72 37 39 33 \
             30 34 2e 31 42 12 32 30 32 35 2e 31 30 2e 30 33 \
             2e 6d 72 36 31 38 32 31 48 6d",
        );
        // Strip the 5-byte gRPC frame header, find GetDeviceInfoResponse (1004),
        // then its DeviceInfo (field 1), then decode.
        let body = &resp[5..];
        let gdi = find_len_field(body, 1004).expect("get_device_info field 1004 present");
        let di = find_len_field(gdi, 1).expect("device_info field 1 present");
        let (id, hw, sw, cc) = decode_device_info(di);
        assert_eq!(id.as_deref(), Some("Router-010000000000000001ED3C1D"));
        assert_eq!(hw.as_deref(), Some("v4"));
        assert_eq!(sw.as_deref(), Some("2025.10.03.mr61821"));
        assert_eq!(cc.as_deref(), Some("AU"));
    }

    #[test]
    fn dish_get_status_round_trip() {
        // Build DeviceInfo { id, hw, sw, cc }.
        let mut device_info = Vec::new();
        put_str(&mut device_info, 1, "ut01000000-00000000-0049fee5");
        put_str(&mut device_info, 2, "rev3_proto2");
        put_str(&mut device_info, 3, "2025.10.03.mr61821");
        put_str(&mut device_info, 4, "AU");
        // DeviceState { uptime_s = 987654 }.
        let mut device_state = Vec::new();
        put_varint(&mut device_state, 1, 987_654);
        // DishObstructionStats { fraction_obstructed = 0.012, currently_obstructed = true }.
        let mut obstruction = Vec::new();
        put_f32(&mut obstruction, 1, 0.012);
        put_varint(&mut obstruction, 5, 1);
        // DishAlerts { thermal_throttle = true (field 3), slow_ethernet_speeds = true (field 6) }.
        let mut alerts = Vec::new();
        put_varint(&mut alerts, 3, 1);
        put_varint(&mut alerts, 6, 1);

        // DishGetStatusResponse.
        let mut dish = Vec::new();
        put_len(&mut dish, 1, &device_info);
        put_len(&mut dish, 2, &device_state);
        put_f32(&mut dish, 1003, 0.02); // pop_ping_drop_rate
        put_len(&mut dish, 1004, &obstruction);
        put_len(&mut dish, 1005, &alerts);
        put_varint(&mut dish, 1006, 1); // state = CONNECTED
        put_f32(&mut dish, 1007, 138_000_000.0); // downlink_bps
        put_f32(&mut dish, 1008, 14_500_000.0); // uplink_bps
        put_f32(&mut dish, 1009, 31.5); // pop_ping_latency_ms

        // Response { dish_get_status = field 2004 } + unrelated field for realism.
        let mut response = Vec::new();
        put_varint(&mut response, 3, 109); // some other Response field (skipped)
        put_len(&mut response, RESP_DISH_GET_STATUS_FIELD, &dish);

        let frame = grpc_frame(&response);
        let m = parse_get_status_response(&frame).expect("decodes");

        assert_eq!(m.state, StarlinkState::Connected);
        assert_eq!(m.device_id.as_deref(), Some("ut01000000-00000000-0049fee5"));
        assert_eq!(m.hardware_version.as_deref(), Some("rev3_proto2"));
        assert_eq!(m.software_version.as_deref(), Some("2025.10.03.mr61821"));
        assert_eq!(m.country_code.as_deref(), Some("AU"));
        assert_eq!(m.uptime_s, Some(987_654));
        assert_eq!(m.pop_ping_drop_rate, Some(0.02));
        assert_eq!(m.obstruction_fraction, Some(0.012));
        assert_eq!(m.currently_obstructed, Some(true));
        assert_eq!(m.downlink_bps, Some(138_000_000.0));
        assert_eq!(m.uplink_bps, Some(14_500_000.0));
        assert_eq!(m.pop_ping_latency_ms, Some(31.5));
        assert_eq!(m.alerts, vec!["thermal_throttle", "slow_ethernet_speeds"]);
        // bars: worst-of drop(0.02→4) and obstruction(0.012→2) = 2.
        assert_eq!(m.bars, Some(2));
    }

    #[test]
    fn router_endpoint_response_is_a_clear_error() {
        // Response carrying wifi_get_status (field 3004) but NOT dish_get_status
        // (2004) — i.e. pointed at the router endpoint. Must error helpfully.
        let mut wifi = Vec::new();
        put_str(&mut wifi, 1, "Router-XYZ");
        let mut response = Vec::new();
        put_len(&mut response, 3004, &wifi);
        let frame = grpc_frame(&response);
        let err = parse_get_status_response(&frame).unwrap_err();
        assert!(err.contains("dish_get_status"), "got: {err}");
        assert!(err.contains("192.168.100.1:9200"), "got: {err}");
    }

    #[test]
    fn unknown_fields_and_alerts_are_tolerant() {
        // A response with a future unknown field (4999) and an unknown alert bit
        // (field 9 = true) decodes without error; the unknown alert surfaces as
        // a generic name.
        let mut alerts = Vec::new();
        put_varint(&mut alerts, 9, 1); // unknown alert bit
        let mut dish = Vec::new();
        put_varint(&mut dish, 1006, 1);
        put_len(&mut dish, 1005, &alerts);
        put_str(&mut dish, 4999, "future-field"); // unknown future field, skipped
        let mut response = Vec::new();
        put_len(&mut response, RESP_DISH_GET_STATUS_FIELD, &dish);
        let m = parse_get_status_response(&grpc_frame(&response)).expect("tolerant decode");
        assert_eq!(m.state, StarlinkState::Connected);
        assert_eq!(m.alerts, vec!["alert_9"]);
    }

    #[test]
    fn live_starlink_mini_get_status_golden() {
        // Real `get_status` response captured live from a Starlink Mini
        // (hardware mini1_panda_prod2, firmware 2026.06.04.mr80926) over the
        // dish gRPC at 192.168.100.1:9200. This firmware OMITS the explicit
        // DishState field (1006) and pop_ping_drop_rate (1003) while connected,
        // and repurposes field 1002 to an implausible value — exactly the shapes
        // the synthetic round-trip can't cover. Pins the decoder + the
        // connected-inference + the 1002 sanity clamp against real bytes
        // (fixture is the raw gRPC response body captured from the dish).
        let frame = include_bytes!("testdata/mini_get_status.bin");
        let m = parse_get_status_response(frame).expect("live golden decodes");
        // device_info (the DISH's own id — a UTerminal id, not the router id).
        assert_eq!(m.device_id.as_deref(), Some("ut31a88798-0611580a-5aec96df"));
        assert_eq!(m.hardware_version.as_deref(), Some("mini1_panda_prod2"));
        assert_eq!(m.software_version.as_deref(), Some("2026.06.04.mr80926"));
        assert_eq!(m.country_code.as_deref(), Some("AU"));
        assert_eq!(m.uptime_s, Some(12688));
        // Link figures decode sensibly (idle dish: ~9 kbps each way, ~64 ms).
        let dl = m.downlink_bps.expect("downlink");
        let ul = m.uplink_bps.expect("uplink");
        let lat = m.pop_ping_latency_ms.expect("latency");
        assert!((9000.0..9500.0).contains(&dl), "downlink {dl}");
        assert!((9000.0..9500.0).contains(&ul), "uplink {ul}");
        assert!((60.0..70.0).contains(&lat), "latency {lat}");
        let obs = m.obstruction_fraction.expect("obstruction fraction");
        assert!(obs > 0.0 && obs < 0.01, "obstruction {obs}");
        // Fields this firmware omits / repurposes → absent after decode.
        assert_eq!(m.pop_ping_drop_rate, None, "1003 absent on this firmware");
        assert_eq!(m.snr, None, "1001 absent");
        assert_eq!(
            m.seconds_to_first_nonempty_slot, None,
            "1002 garbage value must be sanity-filtered out"
        );
        assert!(m.alerts.is_empty(), "no alerts active");
        // The explicit state field is absent → inferred Connected from the PoP RTT.
        assert_eq!(m.state, StarlinkState::Connected);
        // bars derived from obstruction alone (drop rate absent): 0.0059 → 3.
        assert_eq!(m.bars, Some(3));
    }

    // Live end-to-end check of the real reqwest h2c transport against a
    // reachable dish — exercises http2_prior_knowledge + the te:trailers header
    // path that curl proved but a unit test can't mock. Ignored by default
    // (needs the dish + host route); run with:
    //   cargo test --lib live_dish_sample -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live: needs a reachable Starlink dish at 192.168.100.1:9200"]
    async fn live_dish_sample() {
        let cfgs = vec![StarlinkUplinkConfig {
            interface: "wlo5".into(),
            address: "192.168.100.1:9200".into(),
            source_address: None,
        }];
        let sources = build_sources(&cfgs);
        let src = sources.get("wlo5").expect("source built");
        match src.sample().await {
            Ok(m) => {
                eprintln!(
                    "LIVE dish: state={:?} sw={:?} hw={:?} down={:?} up={:?} lat={:?}ms obs={:?} bars={:?} alerts={:?}",
                    m.state, m.software_version, m.hardware_version, m.downlink_bps,
                    m.uplink_bps, m.pop_ping_latency_ms, m.obstruction_fraction, m.bars, m.alerts
                );
                assert_eq!(m.state, StarlinkState::Connected);
                assert!(m.software_version.is_some());
            }
            Err(e) => panic!("live dish sample failed: {e}"),
        }
    }

    #[test]
    fn normalize_address_defaults_port() {
        assert_eq!(normalize_address("192.168.100.1"), "192.168.100.1:9200");
        assert_eq!(normalize_address("192.168.100.1:9200"), "192.168.100.1:9200");
        assert_eq!(normalize_address("192.168.4.1:9000"), "192.168.4.1:9000");
        assert_eq!(normalize_address("  192.168.100.1/  "), "192.168.100.1:9200");
        assert_eq!(normalize_address(""), "192.168.100.1:9200");
        assert_eq!(normalize_address("[fd00::1]"), "[fd00::1]:9200");
        assert_eq!(normalize_address("[fd00::1]:9200"), "[fd00::1]:9200");
    }

    #[test]
    fn build_sources_skips_blank_interface() {
        let cfgs = vec![
            StarlinkUplinkConfig {
                interface: "wlo5".into(),
                address: "192.168.100.1:9200".into(),
                source_address: None,
            },
            StarlinkUplinkConfig {
                interface: "  ".into(),
                address: "x".into(),
                source_address: None,
            },
        ];
        let sources = build_sources(&cfgs);
        assert!(sources.contains_key("wlo5"));
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources.get("wlo5").unwrap().url,
            "http://192.168.100.1:9200/SpaceX.API.Device.Device/Handle"
        );
    }

    #[test]
    fn build_sources_with_source_bind() {
        // Two dishes at the identical fixed address, disambiguated by source IP
        // (the multi-dish customer case). Both sources build; the bound local
        // address isn't observable via reqwest, so we assert structure + that a
        // valid source IP doesn't break the build.
        let cfgs = vec![
            StarlinkUplinkConfig {
                interface: "wlo5".into(),
                address: "192.168.100.1:9200".into(),
                source_address: Some("192.168.4.102".into()),
            },
            StarlinkUplinkConfig {
                interface: "eth2".into(),
                address: "192.168.100.1:9200".into(),
                source_address: Some("192.168.5.102".into()),
            },
        ];
        let sources = build_sources(&cfgs);
        assert_eq!(sources.len(), 2);
        assert!(sources.contains_key("wlo5") && sources.contains_key("eth2"));
        assert_eq!(
            sources.get("eth2").unwrap().url,
            "http://192.168.100.1:9200/SpaceX.API.Device.Device/Handle"
        );
        // A blank / unparseable source_address is ignored (no bind), not fatal.
        let degraded = build_sources(&[StarlinkUplinkConfig {
            interface: "wlo5".into(),
            address: "192.168.100.1:9200".into(),
            source_address: Some("not-an-ip".into()),
        }]);
        assert_eq!(degraded.len(), 1);
    }
}
