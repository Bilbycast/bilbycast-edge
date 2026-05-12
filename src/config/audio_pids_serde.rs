// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Serde helper for `Option<BTreeMap<u16, u16>>` on
//! `TsPidOverridesEntry::audio_pids`.
//!
//! Same quirk as [`crate::config::pid_overrides_serde`] and
//! [`crate::config::pid_map_serde`]: serde's default derive writes numeric
//! `u16` keys as strings in JSON and round-trips them via the key's
//! `FromStr` impl, but that path breaks inside internally-tagged enums
//! (`#[serde(tag = "type")]`). The outer `pid_overrides` map already takes
//! the tagged-enum hit and its values flow through the `Content`
//! intermediate — so this *inner* `BTreeMap<u16, u16>` needs its own
//! helper for the same reason.

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
            let mut out: BTreeMap<u16, u16> = BTreeMap::new();
            for (k, v) in m {
                let src: u16 = k.parse().map_err(|_| {
                    serde::de::Error::custom(format!(
                        "audio_pids key must be a u16 source PID (0-65535), got {k:?}"
                    ))
                })?;
                out.insert(src, v);
            }
            Ok(Some(out))
        }
    }
}
