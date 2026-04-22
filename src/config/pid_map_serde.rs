// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Serde helpers for `Option<BTreeMap<u16, u16>>` pid_map fields.
//!
//! Serde's default derive writes numeric u16 keys as strings when the
//! target format is JSON, then round-trips them back via the key's
//! `FromStr` impl. That path works for top-level structs but breaks
//! inside internally-tagged enums (`#[serde(tag = "type")]`), because
//! `serde_json` routes those through a `Content` intermediate that
//! only accepts string keys when deserialising. This module sidesteps
//! that by always going through a `BTreeMap<String, u16>` on the wire
//! and converting at the boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(
    map: &Option<BTreeMap<u16, u16>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match map {
        None => serializer.serialize_none(),
        Some(m) => {
            let stringed: BTreeMap<String, u16> =
                m.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            stringed.serialize(serializer)
        }
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeMap<u16, u16>>, D::Error> {
    let opt: Option<BTreeMap<String, u16>> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(m) => {
            let mut out = BTreeMap::new();
            for (k, v) in m {
                let pid: u16 = k.parse().map_err(|_| {
                    serde::de::Error::custom(format!(
                        "pid_map key must be a u16 (0-65535), got {k:?}"
                    ))
                })?;
                out.insert(pid, v);
            }
            Ok(Some(out))
        }
    }
}
