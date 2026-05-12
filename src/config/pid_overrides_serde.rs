// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Serde helper for `Option<TsPidOverridesMap>` fields.
//!
//! Same quirk as [`crate::config::pid_map_serde`]: serde's default derive
//! writes numeric `u16` keys as strings when the target format is JSON and
//! round-trips them via the key's `FromStr` impl, but that path breaks
//! inside internally-tagged enums (`#[serde(tag = "type")]`). `serde_json`
//! routes those through a `Content` intermediate that only delivers string
//! keys on the deserialise side, so a top-level `BTreeMap<u16, _>` works
//! while the same map nested inside `InputConfig` / `OutputConfig` fails
//! with `invalid type: string "<prog>", expected u16`.
//!
//! This module sidesteps the quirk by always going through
//! `BTreeMap<String, TsPidOverridesEntry>` on the wire and converting at
//! the boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::models::{TsPidOverridesEntry, TsPidOverridesMap};

pub fn serialize<S: Serializer>(
    map: &Option<TsPidOverridesMap>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match map {
        None => serializer.serialize_none(),
        Some(m) => {
            let stringed: BTreeMap<String, &TsPidOverridesEntry> =
                m.iter().map(|(k, v)| (k.to_string(), v)).collect();
            stringed.serialize(serializer)
        }
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TsPidOverridesMap>, D::Error> {
    let opt: Option<BTreeMap<String, TsPidOverridesEntry>> =
        Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(m) => {
            let mut out = TsPidOverridesMap::new();
            for (k, v) in m {
                let prog: u16 = k.parse().map_err(|_| {
                    serde::de::Error::custom(format!(
                        "pid_overrides key must be a u16 program_number (0-65535), got {k:?}"
                    ))
                })?;
                out.insert(prog, v);
            }
            Ok(Some(out))
        }
    }
}
