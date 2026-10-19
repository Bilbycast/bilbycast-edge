// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::DASHBOARD_HTML;
    use crate::stats::models::{SrtBondingMemberStats, SrtBondingStats, SrtLegStats};

    /// The source of `function <name>(`, up to the next top-level function.
    fn js_function(name: &str) -> &'static str {
        let start = DASHBOARD_HTML
            .find(&format!("function {name}("))
            .unwrap_or_else(|| panic!("dashboard has no {name}"));
        let body = &DASHBOARD_HTML[start..];
        let end = body[1..].find("\nfunction ").map_or(body.len(), |i| i + 1);
        &body[..end]
    }

    /// Every field `body` reads off the variable `var` (`var.field`), where
    /// `var` stands alone rather than ending a longer name.
    fn reads(body: &str, var: &str) -> BTreeSet<String> {
        let needle = format!("{var}.");
        body.match_indices(&needle)
            .filter(|(i, _)| {
                !body[..*i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            })
            .map(|(i, _)| {
                body[i + needle.len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .filter(|field| !field.is_empty())
            .collect()
    }

    fn fields<T: serde::Serialize>(value: &T) -> BTreeSet<String> {
        serde_json::to_value(value)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    fn assert_reads_exist(function: &str, var: &str, exists: &BTreeSet<String>) {
        let read = reads(js_function(function), var);
        assert!(!read.is_empty(), "{function} reads nothing off {var}");
        let missing: Vec<String> = read.difference(exists).map(|f| format!("{var}.{f}")).collect();
        assert!(missing.is_empty(), "{function} reads {missing:?}, which the edge never sends");
    }

    /// The SRT renderers read names the stats never carried (`latency_ms`,
    /// `pkt_loss`, member `id` / `rtt_ms` …); invisible while `srt_stats` was
    /// never published, a row of zeros once it was.
    #[test]
    fn srt_renderers_read_fields_the_edge_serializes() {
        let leg = fields(&SrtLegStats::default());
        assert_reads_exist("renderLegSrt", "s", &leg);
        assert_reads_exist("renderSrtBonding", "s", &fields(&SrtBondingStats::default()));
        assert_reads_exist("renderSrtBonding", "agg", &leg);
        assert_reads_exist("renderSrtBonding", "p", &fields(&SrtBondingMemberStats::default()));
        assert_reads_exist("renderSrtBonding", "st", &leg);
    }
}
