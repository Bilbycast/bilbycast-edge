// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

// Phase 1 step 3: focused RFC 4566 SDP generator + parser scoped to SMPTE
// ST 2110-30 (PCM), -31 (AES3), and -40 (ancillary). Wired into NMOS IS-04
// `manifest_href` and the manager UI's SDP viewer in step 6/9.
#![allow(dead_code)]

//! Pure-Rust SDP generator and parser for SMPTE ST 2110.
//!
//! ## Scope
//!
//! ST 2110 essence flows are described to receivers via Session Description
//! Protocol (RFC 4566) documents. NMOS IS-04 publishes these as the
//! `manifest_href` of every Sender resource; IS-05 PATCH /staged accepts an
//! SDP from the controller. Bilbycast generates SDPs for its own outputs and
//! parses SDPs that come back from manager-mediated receivers.
//!
//! This module deliberately scopes itself to the SMPTE ST 2110 essence types
//! we ship in Phase 1 (-30, -31, -40) plus the common attributes every
//! interop-compliant device emits. We do **not** pull a heavy WebRTC SDP
//! crate — those crates target browser-style negotiation and add 5,000+ LOC
//! and several deps the rest of the codebase does not need.
//!
//! ## Data model
//!
//! [`SdpDocument`] is the parsed-or-built representation: session-level
//! header fields plus a `Vec<SdpMedia>` where each entry is one essence
//! flow (audio, video, or data). The convenience constructors
//! [`SdpDocument::for_st2110_30`], [`SdpDocument::for_st2110_31`], and
//! [`SdpDocument::for_st2110_40`] build a complete document from the
//! corresponding [`crate::config::models`] structs.
//!
//! ## Round-trip guarantee
//!
//! Every document this module generates can be re-parsed by [`parse`] into
//! an [`SdpDocument`] that round-trips back to a byte-identical wire form.
//! The unit tests assert this for representative -30/-31/-40 fixtures.
//!
//! ## Pure Rust
//!
//! Implementation uses only `std`. No regex, no nom, no PEG. The parser is a
//! line-by-line state machine, which is the natural shape of SDP.

use std::fmt::{self, Write as _};
use std::net::IpAddr;
use std::str::FromStr;

use crate::config::models::{
    St2110AncillaryOutputConfig, St2110AudioOutputConfig,
};

/// SDP `m=` line media kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdpMediaKind {
    /// `m=audio` — used by ST 2110-30 and -31.
    Audio,
    /// `m=video` — used by ST 2110-20 (uncompressed video) and -40
    /// (ancillary data, per RFC 8331 §5.3 the `smpte291` payload format
    /// is registered with media type `video`).
    Video,
    /// `m=application` — uncommon for ST 2110 but part of RFC 4566.
    Application,
    /// Any other value not recognized by this module. Preserved verbatim
    /// so a parser → re-emitter round-trip is byte-identical.
    Other(String),
}

impl fmt::Display for SdpMediaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SdpMediaKind::Audio => f.write_str("audio"),
            SdpMediaKind::Video => f.write_str("video"),
            SdpMediaKind::Application => f.write_str("application"),
            SdpMediaKind::Other(s) => f.write_str(s),
        }
    }
}

impl SdpMediaKind {
    fn parse(s: &str) -> Self {
        match s {
            "audio" => SdpMediaKind::Audio,
            "video" => SdpMediaKind::Video,
            "application" => SdpMediaKind::Application,
            _ => SdpMediaKind::Other(s.to_string()),
        }
    }
}

/// `a=rtpmap:<pt> <encoding>/<clock_rate>[/<channels>]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpRtpmap {
    pub payload_type: u8,
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u8>,
}

impl fmt::Display for SdpRtpmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}/{}", self.payload_type, self.encoding, self.clock_rate)?;
        if let Some(ch) = self.channels {
            write!(f, "/{ch}")?;
        }
        Ok(())
    }
}

/// `a=source-filter:incl IN <addr_type> <dest> <source>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpSourceFilter {
    pub addr_type: String,
    pub dest: IpAddr,
    pub source: IpAddr,
}

/// `a=ts-refclk:ptp=IEEE1588-2008:<gmid>:<domain>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpTsRefClk {
    pub version: String,
    pub gmid: String,
    pub domain: u8,
}

impl SdpTsRefClk {
    /// Build a default `IEEE1588-2008` refclk pointing at the given grandmaster.
    pub fn ieee1588(gmid: impl Into<String>, domain: u8) -> Self {
        Self {
            version: "IEEE1588-2008".to_string(),
            gmid: gmid.into(),
            domain,
        }
    }
}

/// One `m=` block plus its attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct SdpMedia {
    pub kind: SdpMediaKind,
    pub port: u16,
    pub protocol: String,
    pub payload_type: u8,
    pub connection_addr: Option<IpAddr>,
    pub source_filter: Option<SdpSourceFilter>,
    pub rtpmap: Option<SdpRtpmap>,
    /// Raw `a=fmtp:<pt> <params>` value (everything after the payload type).
    /// We do not interpret the parameters here — call sites that need them
    /// (the channel mapping UI, the stats display) parse them locally.
    pub fmtp: Option<String>,
    /// `a=ptime:<ms>` — packetization period in milliseconds. Stored as f64
    /// to preserve fractional values like `0.125`.
    pub ptime_ms: Option<f64>,
    pub ts_refclk: Option<SdpTsRefClk>,
    /// `a=mediaclk:<value>` — typically `direct=0` for ST 2110.
    pub mediaclk: Option<String>,
    /// `a=channel-order:<value>` — used by ST 2110-30 to convey channel
    /// ordering per ST 2110-30 Annex C.
    pub channel_order: Option<String>,
}

/// Top-level SDP document.
#[derive(Debug, Clone, PartialEq)]
pub struct SdpDocument {
    /// `o=` originator: username, session id, session version, addr type, address.
    pub originator: SdpOriginator,
    /// `s=` session name.
    pub session_name: String,
    /// `c=` session-level connection (optional; can be overridden per-media).
    pub session_connection: Option<IpAddr>,
    /// Media descriptions, in declaration order.
    pub media: Vec<SdpMedia>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpOriginator {
    pub username: String,
    pub session_id: u64,
    pub session_version: u64,
    pub address: IpAddr,
}

impl SdpOriginator {
    /// Build a default originator suitable for bilbycast-emitted SDPs.
    pub fn bilbycast(session_id: u64, address: IpAddr) -> Self {
        Self {
            username: "-".to_string(),
            session_id,
            session_version: session_id,
            address,
        }
    }
}

impl SdpDocument {
    /// Build an SDP describing a SMPTE ST 2110-30 (linear PCM) sender.
    ///
    /// `dest`/`port` is the multicast (or unicast) destination the sender is
    /// publishing to. `source` is the bilbycast node's source IP for the
    /// `source-filter` attribute. `gmid` is the optional PTP grandmaster
    /// identifier (formatted as 8 hex bytes colon-separated, e.g.
    /// `de:ad:be:ef:ca:fe:ba:be`); when `None`, the `ts-refclk` line uses an
    /// all-zero placeholder.
    pub fn for_st2110_30(
        cfg: &St2110AudioOutputConfig,
        dest: IpAddr,
        port: u16,
        source: IpAddr,
        gmid: Option<&str>,
        node_address: IpAddr,
    ) -> Self {
        Self::build_audio(
            cfg, dest, port, source, gmid, node_address, audio_encoding_30(cfg.bit_depth),
        )
    }

    /// Build an SDP describing a SMPTE ST 2110-31 (AES3 transparent) sender.
    pub fn for_st2110_31(
        cfg: &St2110AudioOutputConfig,
        dest: IpAddr,
        port: u16,
        source: IpAddr,
        gmid: Option<&str>,
        node_address: IpAddr,
    ) -> Self {
        Self::build_audio(
            cfg, dest, port, source, gmid, node_address, "AM824".to_string(),
        )
    }

    /// Build an SDP describing a SMPTE ST 2110-40 ancillary data sender.
    ///
    /// Per RFC 8331 §5.3 the payload format is `smpte291` and the media kind
    /// is `video` (because ANC is co-timed with video frames even when no
    /// video is present).
    pub fn for_st2110_40(
        cfg: &St2110AncillaryOutputConfig,
        dest: IpAddr,
        port: u16,
        source: IpAddr,
        gmid: Option<&str>,
        node_address: IpAddr,
    ) -> Self {
        let session_id = synthetic_session_id(&cfg.id);
        let ts_refclk = Some(SdpTsRefClk::ieee1588(
            gmid.unwrap_or("00:00:00:00:00:00:00:00"),
            cfg.clock_domain.unwrap_or(0),
        ));
        let media = SdpMedia {
            kind: SdpMediaKind::Video,
            port,
            protocol: "RTP/AVP".to_string(),
            payload_type: cfg.payload_type,
            connection_addr: Some(dest),
            source_filter: Some(SdpSourceFilter {
                addr_type: ip_addr_type(&dest).to_string(),
                dest,
                source,
            }),
            rtpmap: Some(SdpRtpmap {
                payload_type: cfg.payload_type,
                encoding: "smpte291".to_string(),
                clock_rate: 90_000,
                channels: None,
            }),
            fmtp: None,
            ptime_ms: None,
            ts_refclk,
            mediaclk: Some("direct=0".to_string()),
            channel_order: None,
        };
        Self {
            originator: SdpOriginator::bilbycast(session_id, node_address),
            session_name: format!("bilbycast ST 2110-40 {}", cfg.name),
            session_connection: None,
            media: vec![media],
        }
    }

    fn build_audio(
        cfg: &St2110AudioOutputConfig,
        dest: IpAddr,
        port: u16,
        source: IpAddr,
        gmid: Option<&str>,
        node_address: IpAddr,
        encoding: String,
    ) -> Self {
        let session_id = synthetic_session_id(&cfg.id);
        let ts_refclk = Some(SdpTsRefClk::ieee1588(
            gmid.unwrap_or("00:00:00:00:00:00:00:00"),
            cfg.clock_domain.unwrap_or(0),
        ));
        let media = SdpMedia {
            kind: SdpMediaKind::Audio,
            port,
            protocol: "RTP/AVP".to_string(),
            payload_type: cfg.payload_type,
            connection_addr: Some(dest),
            source_filter: Some(SdpSourceFilter {
                addr_type: ip_addr_type(&dest).to_string(),
                dest,
                source,
            }),
            rtpmap: Some(SdpRtpmap {
                payload_type: cfg.payload_type,
                encoding,
                clock_rate: cfg.sample_rate,
                channels: Some(cfg.channels),
            }),
            fmtp: None,
            ptime_ms: Some(cfg.packet_time_us as f64 / 1000.0),
            ts_refclk,
            mediaclk: Some("direct=0".to_string()),
            channel_order: Some(default_channel_order(cfg.channels)),
        };
        Self {
            originator: SdpOriginator::bilbycast(session_id, node_address),
            session_name: format!("bilbycast {}", cfg.name),
            session_connection: None,
            media: vec![media],
        }
    }
}

/// Render an [`SdpDocument`] to its RFC 4566 wire form.
///
/// Lines are separated by `\r\n` per RFC 4566 §5. Optional fields are
/// emitted only when present.
pub fn generate(doc: &SdpDocument) -> String {
    // Conservative initial capacity; grows as needed.
    let mut out = String::with_capacity(512);
    out.push_str("v=0\r\n");

    // o=<username> <session-id> <session-version> IN <addr-type> <addr>
    let _ = write!(
        out,
        "o={} {} {} IN {} {}\r\n",
        doc.originator.username,
        doc.originator.session_id,
        doc.originator.session_version,
        ip_addr_type(&doc.originator.address),
        doc.originator.address,
    );
    let _ = writeln_crlf(&mut out, "s", &doc.session_name);
    if let Some(addr) = doc.session_connection {
        let _ = write!(
            out,
            "c=IN {} {}\r\n",
            ip_addr_type(&addr),
            addr,
        );
    }
    out.push_str("t=0 0\r\n");

    for m in &doc.media {
        let _ = write!(
            out,
            "m={} {} {} {}\r\n",
            m.kind, m.port, m.protocol, m.payload_type,
        );
        if let Some(addr) = m.connection_addr {
            let _ = write!(out, "c=IN {} {}\r\n", ip_addr_type(&addr), addr);
        }
        if let Some(sf) = &m.source_filter {
            let _ = write!(
                out,
                "a=source-filter:incl IN {} {} {}\r\n",
                sf.addr_type, sf.dest, sf.source,
            );
        }
        if let Some(rtpmap) = &m.rtpmap {
            let _ = write!(out, "a=rtpmap:{}\r\n", rtpmap);
        }
        if let Some(fmtp) = &m.fmtp {
            let _ = write!(out, "a=fmtp:{} {}\r\n", m.payload_type, fmtp);
        }
        if let Some(p) = m.ptime_ms {
            let _ = write!(out, "a=ptime:{}\r\n", format_ptime(p));
        }
        if let Some(co) = &m.channel_order {
            let _ = write!(out, "a=channel-order:{}\r\n", co);
        }
        if let Some(ts) = &m.ts_refclk {
            let _ = write!(
                out,
                "a=ts-refclk:ptp={}:{}:{}\r\n",
                ts.version, ts.gmid, ts.domain,
            );
        }
        if let Some(mc) = &m.mediaclk {
            let _ = write!(out, "a=mediaclk:{}\r\n", mc);
        }
    }
    out
}

/// Format a `ptime` value: integers as integers, fractions to 3 decimal places.
fn format_ptime(p: f64) -> String {
    if (p - p.round()).abs() < f64::EPSILON {
        format!("{}", p as i64)
    } else {
        // Trim trailing zeros after the decimal point.
        let mut s = format!("{:.3}", p);
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }
}

fn writeln_crlf(out: &mut String, key: &str, value: &str) -> fmt::Result {
    write!(out, "{key}={value}\r\n")
}

/// Errors returned by [`parse`].
#[derive(Debug)]
pub enum SdpParseError {
    /// Document is empty or missing the mandatory `v=0` first line.
    NoVersion,
    /// `v=` value other than `0`.
    UnsupportedVersion(String),
    /// Required field missing (e.g. `o=`, `s=`).
    MissingField(&'static str),
    /// A line could not be parsed (with the offending line).
    BadLine(String),
    /// An IP address failed to parse.
    BadAddress(String),
    /// `m=` block was malformed.
    BadMediaLine(String),
    /// `a=rtpmap` was malformed.
    BadRtpmap(String),
    /// PTP refclk fields could not be parsed.
    BadTsRefClk(String),
}

impl fmt::Display for SdpParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SdpParseError::NoVersion => f.write_str("SDP document missing v= line"),
            SdpParseError::UnsupportedVersion(s) => {
                write!(f, "unsupported SDP version: {s}")
            }
            SdpParseError::MissingField(s) => write!(f, "missing required field: {s}"),
            SdpParseError::BadLine(s) => write!(f, "could not parse line: {s}"),
            SdpParseError::BadAddress(s) => write!(f, "invalid IP address: {s}"),
            SdpParseError::BadMediaLine(s) => write!(f, "malformed m= line: {s}"),
            SdpParseError::BadRtpmap(s) => write!(f, "malformed rtpmap: {s}"),
            SdpParseError::BadTsRefClk(s) => write!(f, "malformed ts-refclk: {s}"),
        }
    }
}

impl std::error::Error for SdpParseError {}

/// Parse an SDP document from its RFC 4566 wire form.
///
/// Accepts both `\r\n` and `\n` line terminators. Returns
/// [`SdpParseError::NoVersion`] when the input is empty or missing `v=0`.
pub fn parse(input: &str) -> Result<SdpDocument, SdpParseError> {
    let mut lines = input.lines().filter(|l| !l.trim().is_empty());

    // First line must be v=0.
    let first = lines.next().ok_or(SdpParseError::NoVersion)?;
    let v_value = first
        .strip_prefix("v=")
        .ok_or(SdpParseError::NoVersion)?;
    if v_value.trim() != "0" {
        return Err(SdpParseError::UnsupportedVersion(v_value.to_string()));
    }

    let mut originator: Option<SdpOriginator> = None;
    let mut session_name: Option<String> = None;
    let mut session_connection: Option<IpAddr> = None;
    let mut media: Vec<SdpMedia> = Vec::new();

    for raw in lines {
        let line = raw.trim_end_matches('\r');
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => return Err(SdpParseError::BadLine(line.to_string())),
        };
        match k {
            "o" => originator = Some(parse_origin(v)?),
            "s" => session_name = Some(v.to_string()),
            "c" if media.is_empty() => {
                session_connection = Some(parse_connection(v)?);
            }
            "c" => {
                if let Some(last) = media.last_mut() {
                    last.connection_addr = Some(parse_connection(v)?);
                }
            }
            "t" => {
                // Ignore — bilbycast doesn't honor session timing.
            }
            "m" => media.push(parse_media_line(v)?),
            "a" => {
                let (attr, value) = match v.split_once(':') {
                    Some((a, val)) => (a, Some(val)),
                    None => (v, None),
                };
                if let Some(last) = media.last_mut() {
                    apply_media_attribute(last, attr, value)?;
                } else {
                    // Session-level attributes are silently ignored in
                    // Phase 1 — none of them affect ST 2110 essence flows.
                }
            }
            _ => {
                // Unknown line type — preserve forward-compat by ignoring.
            }
        }
    }

    let originator = originator.ok_or(SdpParseError::MissingField("o"))?;
    let session_name = session_name.ok_or(SdpParseError::MissingField("s"))?;

    Ok(SdpDocument {
        originator,
        session_name,
        session_connection,
        media,
    })
}

fn parse_origin(v: &str) -> Result<SdpOriginator, SdpParseError> {
    // <username> <session-id> <session-version> IN <addr-type> <addr>
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    if parts.len() != 6 {
        return Err(SdpParseError::BadLine(format!("o={v}")));
    }
    let username = parts[0].to_string();
    let session_id: u64 = parts[1].parse().map_err(|_| SdpParseError::BadLine(format!("o={v}")))?;
    let session_version: u64 = parts[2].parse().map_err(|_| SdpParseError::BadLine(format!("o={v}")))?;
    if parts[3] != "IN" {
        return Err(SdpParseError::BadLine(format!("o={v}")));
    }
    let _addr_type = parts[4]; // we trust the address itself
    let address = IpAddr::from_str(parts[5])
        .map_err(|_| SdpParseError::BadAddress(parts[5].to_string()))?;
    Ok(SdpOriginator {
        username,
        session_id,
        session_version,
        address,
    })
}

fn parse_connection(v: &str) -> Result<IpAddr, SdpParseError> {
    // c=IN <addr-type> <addr>[/ttl[/n]]
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    if parts.len() < 3 || parts[0] != "IN" {
        return Err(SdpParseError::BadLine(format!("c={v}")));
    }
    // Strip optional TTL/count suffix from the address (e.g. "239.1.1.1/64").
    let addr_str = parts[2].split('/').next().unwrap_or(parts[2]);
    IpAddr::from_str(addr_str).map_err(|_| SdpParseError::BadAddress(addr_str.to_string()))
}

fn parse_media_line(v: &str) -> Result<SdpMedia, SdpParseError> {
    // <media> <port> <proto> <fmt>
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    if parts.len() < 4 {
        return Err(SdpParseError::BadMediaLine(v.to_string()));
    }
    let kind = SdpMediaKind::parse(parts[0]);
    let port: u16 = parts[1].parse().map_err(|_| SdpParseError::BadMediaLine(v.to_string()))?;
    let protocol = parts[2].to_string();
    let payload_type: u8 = parts[3]
        .parse()
        .map_err(|_| SdpParseError::BadMediaLine(v.to_string()))?;
    Ok(SdpMedia {
        kind,
        port,
        protocol,
        payload_type,
        connection_addr: None,
        source_filter: None,
        rtpmap: None,
        fmtp: None,
        ptime_ms: None,
        ts_refclk: None,
        mediaclk: None,
        channel_order: None,
    })
}

fn apply_media_attribute(
    m: &mut SdpMedia,
    attr: &str,
    value: Option<&str>,
) -> Result<(), SdpParseError> {
    match (attr, value) {
        ("rtpmap", Some(v)) => m.rtpmap = Some(parse_rtpmap(v)?),
        ("fmtp", Some(v)) => {
            // Format: "<pt> <params>". We strip the PT prefix if it matches
            // the media's payload type; otherwise we keep the whole string.
            if let Some((pt, rest)) = v.split_once(' ') {
                if pt.parse::<u8>().ok() == Some(m.payload_type) {
                    m.fmtp = Some(rest.to_string());
                    return Ok(());
                }
            }
            m.fmtp = Some(v.to_string());
        }
        ("ptime", Some(v)) => {
            m.ptime_ms = Some(v.trim().parse().map_err(|_| SdpParseError::BadLine(format!("ptime:{v}")))?);
        }
        ("channel-order", Some(v)) => m.channel_order = Some(v.to_string()),
        ("source-filter", Some(v)) => m.source_filter = Some(parse_source_filter(v)?),
        ("ts-refclk", Some(v)) => m.ts_refclk = Some(parse_ts_refclk(v)?),
        ("mediaclk", Some(v)) => m.mediaclk = Some(v.to_string()),
        _ => {
            // Unknown attribute — ignore for forward compat.
        }
    }
    Ok(())
}

fn parse_rtpmap(v: &str) -> Result<SdpRtpmap, SdpParseError> {
    // <pt> <encoding>/<clock_rate>[/<channels>]
    let (pt_str, rest) = v
        .split_once(' ')
        .ok_or_else(|| SdpParseError::BadRtpmap(v.to_string()))?;
    let payload_type: u8 = pt_str
        .parse()
        .map_err(|_| SdpParseError::BadRtpmap(v.to_string()))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 2 {
        return Err(SdpParseError::BadRtpmap(v.to_string()));
    }
    let encoding = parts[0].to_string();
    let clock_rate: u32 = parts[1]
        .parse()
        .map_err(|_| SdpParseError::BadRtpmap(v.to_string()))?;
    let channels = parts
        .get(2)
        .and_then(|c| c.parse::<u8>().ok());
    Ok(SdpRtpmap {
        payload_type,
        encoding,
        clock_rate,
        channels,
    })
}

fn parse_source_filter(v: &str) -> Result<SdpSourceFilter, SdpParseError> {
    // incl IN <addr_type> <dest> <source>
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    if parts.len() != 5 || parts[0] != "incl" || parts[1] != "IN" {
        return Err(SdpParseError::BadLine(format!("source-filter:{v}")));
    }
    let addr_type = parts[2].to_string();
    let dest = IpAddr::from_str(parts[3])
        .map_err(|_| SdpParseError::BadAddress(parts[3].to_string()))?;
    let source = IpAddr::from_str(parts[4])
        .map_err(|_| SdpParseError::BadAddress(parts[4].to_string()))?;
    Ok(SdpSourceFilter {
        addr_type,
        dest,
        source,
    })
}

fn parse_ts_refclk(v: &str) -> Result<SdpTsRefClk, SdpParseError> {
    // ptp=IEEE1588-2008:<gmid>:<domain>
    let body = v
        .strip_prefix("ptp=")
        .ok_or_else(|| SdpParseError::BadTsRefClk(v.to_string()))?;
    // Split into at most three sections so the GMID's internal colons stay intact.
    let mut parts = body.splitn(3, ':');
    let version = parts
        .next()
        .ok_or_else(|| SdpParseError::BadTsRefClk(v.to_string()))?;
    let gmid_and_domain = parts
        .next()
        .ok_or_else(|| SdpParseError::BadTsRefClk(v.to_string()))?;
    let trailer = parts.next();

    // The GMID is 8 hex bytes separated by ':' (23 chars total). The domain
    // is the integer that follows the last colon.
    let (gmid, domain) = if let Some(domain_str) = trailer {
        // Reassemble: gmid_and_domain holds bytes 0..7 of the GMID, trailer
        // is the domain. But more likely the splitn(3, ':') consumed only
        // the first colon, so gmid_and_domain is the first byte and trailer
        // is "rest:domain" — we have to recombine.
        let combined = format!("{gmid_and_domain}:{domain_str}");
        split_gmid_domain(&combined)?
    } else {
        split_gmid_domain(gmid_and_domain)?
    };
    Ok(SdpTsRefClk {
        version: version.to_string(),
        gmid,
        domain,
    })
}

/// Split `<gmid>:<domain>` where the GMID itself contains 7 internal colons.
///
/// We rely on the fixed structure: the GMID is exactly 23 ASCII characters
/// (8 × 2 hex digits + 7 separating colons) followed by `:<domain>`.
fn split_gmid_domain(s: &str) -> Result<(String, u8), SdpParseError> {
    const GMID_LEN: usize = 8 * 2 + 7; // 23
    if s.len() < GMID_LEN + 2 {
        return Err(SdpParseError::BadTsRefClk(s.to_string()));
    }
    let gmid = &s[..GMID_LEN];
    if &s[GMID_LEN..GMID_LEN + 1] != ":" {
        return Err(SdpParseError::BadTsRefClk(s.to_string()));
    }
    let domain_str = &s[GMID_LEN + 1..];
    let domain: u8 = domain_str
        .trim()
        .parse()
        .map_err(|_| SdpParseError::BadTsRefClk(s.to_string()))?;
    Ok((gmid.to_string(), domain))
}

fn ip_addr_type(addr: &IpAddr) -> &'static str {
    match addr {
        IpAddr::V4(_) => "IP4",
        IpAddr::V6(_) => "IP6",
    }
}

/// Default ST 2110-30 channel-order string for the given channel count.
///
/// Per ST 2110-30 Annex C the channel order is expressed as a series of
/// channel groupings: `M01` (mono), `ST` (stereo), `51` (5.1), `71` (7.1),
/// etc. For Phase 1 we emit a generic `M`-prefixed list when no specific
/// surround layout applies.
fn default_channel_order(channels: u8) -> String {
    match channels {
        1 => "SMPTE2110.(M)".to_string(),
        2 => "SMPTE2110.(ST)".to_string(),
        4 => "SMPTE2110.(M,M,M,M)".to_string(),
        6 => "SMPTE2110.(51)".to_string(),
        8 => "SMPTE2110.(71)".to_string(),
        16 => "SMPTE2110.(M,M,M,M,M,M,M,M,M,M,M,M,M,M,M,M)".to_string(),
        n => format!("SMPTE2110.({})", vec!["M"; n as usize].join(",")),
    }
}

fn audio_encoding_30(bit_depth: u8) -> String {
    if bit_depth == 16 {
        "L16".to_string()
    } else {
        "L24".to_string()
    }
}

/// Hash a flow output id into a deterministic 32-bit session id so the
/// emitted SDP is stable across restarts. The hash is intentionally simple
/// (FNV-1a) — we don't depend on `std::hash::DefaultHasher` because its
/// output isn't guaranteed stable across Rust releases.
fn synthetic_session_id(seed: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{St2110AncillaryOutputConfig, St2110AudioOutputConfig};

    fn audio_cfg(name: &str, bit_depth: u8, channels: u8) -> St2110AudioOutputConfig {
        St2110AudioOutputConfig {
            active: true,
            group: None,
            id: format!("{name}-id"),
            name: name.to_string(),
            dest_addr: "239.10.10.1:5004".to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth,
            channels,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: Some(0),
            dscp: 46,
            ssrc: None,
            transcode: None,
        }
    }

    fn anc_cfg(name: &str) -> St2110AncillaryOutputConfig {
        St2110AncillaryOutputConfig {
            active: true,
            group: None,
            id: format!("{name}-id"),
            name: name.to_string(),
            dest_addr: "239.10.10.10:5006".to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            payload_type: 100,
            clock_domain: Some(0),
            dscp: 46,
            ssrc: None,
        }
    }

    fn ipv4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn test_format_ptime_integer() {
        assert_eq!(format_ptime(1.0), "1");
    }

    #[test]
    fn test_format_ptime_fraction() {
        assert_eq!(format_ptime(0.125), "0.125");
        assert_eq!(format_ptime(0.333), "0.333");
    }

    #[test]
    fn test_format_ptime_trims_trailing_zero() {
        assert_eq!(format_ptime(0.5), "0.5");
    }

    #[test]
    fn test_default_channel_order_stereo() {
        assert_eq!(default_channel_order(2), "SMPTE2110.(ST)");
    }

    #[test]
    fn test_audio_encoding_30() {
        assert_eq!(audio_encoding_30(16), "L16");
        assert_eq!(audio_encoding_30(24), "L24");
    }

    #[test]
    fn test_synthetic_session_id_stable_across_calls() {
        let a = synthetic_session_id("audio-1");
        let b = synthetic_session_id("audio-1");
        assert_eq!(a, b);
        assert_ne!(synthetic_session_id("audio-1"), synthetic_session_id("audio-2"));
    }

    #[test]
    fn test_generate_st2110_30_basic() {
        let cfg = audio_cfg("Audio One", 24, 2);
        let doc = SdpDocument::for_st2110_30(
            &cfg,
            ipv4("239.10.10.1"),
            5004,
            ipv4("10.0.0.5"),
            Some("de:ad:be:ef:ca:fe:ba:be"),
            ipv4("10.0.0.5"),
        );
        let sdp = generate(&doc);

        // Spot-check the key lines.
        assert!(sdp.starts_with("v=0\r\n"));
        assert!(sdp.contains("o=- "));
        assert!(sdp.contains(" IN IP4 10.0.0.5"));
        assert!(sdp.contains("s=bilbycast Audio One\r\n"));
        assert!(sdp.contains("m=audio 5004 RTP/AVP 97\r\n"));
        assert!(sdp.contains("c=IN IP4 239.10.10.1\r\n"));
        assert!(sdp.contains("a=source-filter:incl IN IP4 239.10.10.1 10.0.0.5\r\n"));
        assert!(sdp.contains("a=rtpmap:97 L24/48000/2\r\n"));
        assert!(sdp.contains("a=ptime:1\r\n"));
        assert!(sdp.contains("a=channel-order:SMPTE2110.(ST)\r\n"));
        assert!(sdp.contains("a=ts-refclk:ptp=IEEE1588-2008:de:ad:be:ef:ca:fe:ba:be:0\r\n"));
        assert!(sdp.contains("a=mediaclk:direct=0\r\n"));
    }

    #[test]
    fn test_generate_st2110_31_uses_am824() {
        let cfg = audio_cfg("AES3 One", 24, 2);
        let doc = SdpDocument::for_st2110_31(
            &cfg,
            ipv4("239.10.10.2"),
            5004,
            ipv4("10.0.0.5"),
            None,
            ipv4("10.0.0.5"),
        );
        let sdp = generate(&doc);
        assert!(sdp.contains("a=rtpmap:97 AM824/48000/2\r\n"));
        assert!(sdp.contains("a=ts-refclk:ptp=IEEE1588-2008:00:00:00:00:00:00:00:00:0\r\n"));
    }

    #[test]
    fn test_generate_st2110_40_uses_smpte291_video() {
        let cfg = anc_cfg("ANC One");
        let doc = SdpDocument::for_st2110_40(
            &cfg,
            ipv4("239.10.10.10"),
            5006,
            ipv4("10.0.0.5"),
            Some("aa:bb:cc:dd:ee:ff:00:11"),
            ipv4("10.0.0.5"),
        );
        let sdp = generate(&doc);
        assert!(sdp.contains("m=video 5006 RTP/AVP 100\r\n"));
        assert!(sdp.contains("a=rtpmap:100 smpte291/90000\r\n"));
        // -40 has no ptime/channel-order.
        assert!(!sdp.contains("a=ptime"));
        assert!(!sdp.contains("a=channel-order"));
        assert!(sdp.contains("a=ts-refclk:ptp=IEEE1588-2008:aa:bb:cc:dd:ee:ff:00:11:0\r\n"));
    }

    #[test]
    fn test_round_trip_st2110_30() {
        let cfg = audio_cfg("RT30", 24, 2);
        let original = SdpDocument::for_st2110_30(
            &cfg,
            ipv4("239.20.20.1"),
            5004,
            ipv4("10.0.0.7"),
            Some("11:22:33:44:55:66:77:88"),
            ipv4("10.0.0.7"),
        );
        let wire = generate(&original);
        let parsed = parse(&wire).expect("parse round-trip");
        assert_eq!(parsed, original);
        // And re-emit must be byte-identical.
        assert_eq!(generate(&parsed), wire);
    }

    #[test]
    fn test_round_trip_st2110_31() {
        let cfg = audio_cfg("RT31", 24, 4);
        let original = SdpDocument::for_st2110_31(
            &cfg,
            ipv4("239.20.20.2"),
            5004,
            ipv4("10.0.0.7"),
            Some("aa:bb:cc:dd:ee:ff:00:01"),
            ipv4("10.0.0.7"),
        );
        let wire = generate(&original);
        let parsed = parse(&wire).expect("parse round-trip");
        assert_eq!(parsed, original);
        assert_eq!(generate(&parsed), wire);
    }

    #[test]
    fn test_round_trip_st2110_40() {
        let cfg = anc_cfg("RT40");
        let original = SdpDocument::for_st2110_40(
            &cfg,
            ipv4("239.20.20.10"),
            5006,
            ipv4("10.0.0.7"),
            Some("01:02:03:04:05:06:07:08"),
            ipv4("10.0.0.7"),
        );
        let wire = generate(&original);
        let parsed = parse(&wire).expect("parse round-trip");
        assert_eq!(parsed, original);
        assert_eq!(generate(&parsed), wire);
    }

    #[test]
    fn test_round_trip_audio_with_fraction_ptime() {
        let mut cfg = audio_cfg("FRACT", 24, 2);
        cfg.packet_time_us = 125;
        let original = SdpDocument::for_st2110_30(
            &cfg,
            ipv4("239.20.20.20"),
            5004,
            ipv4("10.0.0.7"),
            None,
            ipv4("10.0.0.7"),
        );
        let wire = generate(&original);
        assert!(wire.contains("a=ptime:0.125\r\n"));
        let parsed = parse(&wire).expect("parse round-trip");
        assert_eq!(parsed, original);
    }

    #[test]
    fn test_parse_rejects_missing_version() {
        assert!(matches!(parse(""), Err(SdpParseError::NoVersion)));
        assert!(matches!(parse("o=- 1 1 IN IP4 10.0.0.1\r\n"), Err(SdpParseError::NoVersion)));
    }

    #[test]
    fn test_parse_rejects_unsupported_version() {
        let s = "v=1\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=foo\r\n";
        assert!(matches!(parse(s), Err(SdpParseError::UnsupportedVersion(_))));
    }

    #[test]
    fn test_parse_rejects_missing_originator() {
        let s = "v=0\r\ns=foo\r\n";
        assert!(matches!(parse(s), Err(SdpParseError::MissingField("o"))));
    }

    #[test]
    fn test_parse_rejects_missing_session_name() {
        let s = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\n";
        assert!(matches!(parse(s), Err(SdpParseError::MissingField("s"))));
    }

    #[test]
    fn test_parse_handles_unix_line_endings() {
        let s = "v=0\no=- 1 1 IN IP4 10.0.0.1\ns=test\nt=0 0\nm=audio 5004 RTP/AVP 97\na=rtpmap:97 L24/48000/2\n";
        let doc = parse(s).expect("parse");
        assert_eq!(doc.media.len(), 1);
        assert_eq!(doc.media[0].rtpmap.as_ref().unwrap().encoding, "L24");
    }

    #[test]
    fn test_parse_ignores_unknown_attributes() {
        let s = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=test\r\nt=0 0\r\nm=audio 5004 RTP/AVP 97\r\na=rtpmap:97 L24/48000/2\r\na=mid:audio0\r\na=control:trackID=0\r\n";
        let doc = parse(s).expect("parse");
        assert_eq!(doc.media[0].rtpmap.as_ref().unwrap().clock_rate, 48_000);
    }

    #[test]
    fn test_parse_rejects_bad_origin_arity() {
        let s = "v=0\r\no=- 1 IN IP4 10.0.0.1\r\ns=test\r\n";
        assert!(parse(s).is_err());
    }

    #[test]
    fn test_parse_rtpmap_without_channels() {
        let r = parse_rtpmap("100 smpte291/90000").unwrap();
        assert_eq!(r.payload_type, 100);
        assert_eq!(r.encoding, "smpte291");
        assert_eq!(r.clock_rate, 90_000);
        assert!(r.channels.is_none());
    }

    #[test]
    fn test_parse_rtpmap_with_channels() {
        let r = parse_rtpmap("97 L24/48000/2").unwrap();
        assert_eq!(r.channels, Some(2));
    }

    #[test]
    fn test_parse_ts_refclk() {
        let r = parse_ts_refclk("ptp=IEEE1588-2008:de:ad:be:ef:ca:fe:ba:be:0").unwrap();
        assert_eq!(r.version, "IEEE1588-2008");
        assert_eq!(r.gmid, "de:ad:be:ef:ca:fe:ba:be");
        assert_eq!(r.domain, 0);
    }

    #[test]
    fn test_parse_ts_refclk_nonzero_domain() {
        let r = parse_ts_refclk("ptp=IEEE1588-2008:11:22:33:44:55:66:77:88:42").unwrap();
        assert_eq!(r.domain, 42);
    }

    #[test]
    fn test_parse_ts_refclk_rejects_short() {
        assert!(parse_ts_refclk("ptp=IEEE1588-2008:short:0").is_err());
    }

    #[test]
    fn test_parse_source_filter() {
        let sf = parse_source_filter("incl IN IP4 239.20.20.1 10.0.0.7").unwrap();
        assert_eq!(sf.addr_type, "IP4");
        assert_eq!(sf.dest, ipv4("239.20.20.1"));
        assert_eq!(sf.source, ipv4("10.0.0.7"));
    }

    #[test]
    fn test_parse_source_filter_rejects_excl() {
        assert!(parse_source_filter("excl IN IP4 239.20.20.1 10.0.0.7").is_err());
    }

    #[test]
    fn test_parse_real_world_lawo_audio_sdp() {
        // Trimmed-down sample of the kind of SDP a Lawo device emits for an
        // ST 2110-30 stereo sender. This exercises tolerance of extra
        // attributes (a=mid, a=control, a=tool) and integer ptime.
        let sample = "\
v=0\r\n\
o=- 1234 1234 IN IP4 192.168.1.10\r\n\
s=Lawo MC2 56 audio sender\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 97\r\n\
c=IN IP4 239.100.10.1/64\r\n\
a=source-filter:incl IN IP4 239.100.10.1 192.168.1.10\r\n\
a=rtpmap:97 L24/48000/2\r\n\
a=fmtp:97 channel-order=SMPTE2110.(ST)\r\n\
a=ptime:1\r\n\
a=mediaclk:direct=0\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00:00:00:00:00:00:00:00:0\r\n\
a=mid:audio0\r\n\
a=tool:Lawo SDP composer\r\n\
";
        let doc = parse(sample).expect("parse Lawo SDP");
        assert_eq!(doc.media.len(), 1);
        let m = &doc.media[0];
        assert_eq!(m.kind, SdpMediaKind::Audio);
        assert_eq!(m.port, 5004);
        assert_eq!(m.payload_type, 97);
        assert_eq!(m.connection_addr, Some(ipv4("239.100.10.1")));
        let sf = m.source_filter.as_ref().unwrap();
        assert_eq!(sf.dest, ipv4("239.100.10.1"));
        assert_eq!(sf.source, ipv4("192.168.1.10"));
        let r = m.rtpmap.as_ref().unwrap();
        assert_eq!(r.encoding, "L24");
        assert_eq!(r.clock_rate, 48_000);
        assert_eq!(r.channels, Some(2));
        assert_eq!(m.ptime_ms, Some(1.0));
        assert_eq!(m.mediaclk.as_deref(), Some("direct=0"));
        let ts = m.ts_refclk.as_ref().unwrap();
        assert_eq!(ts.gmid, "00:00:00:00:00:00:00:00");
        // fmtp value should have the PT prefix stripped.
        assert_eq!(m.fmtp.as_deref(), Some("channel-order=SMPTE2110.(ST)"));
    }
}
