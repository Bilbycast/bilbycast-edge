#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Guard against silent drift of the vendored `is` crate patch.
#
# Cargo.toml carries `[patch.crates-io] is = { path = "vendor/is" }` to
# work around upstream's strict ICE-PRIORITY parsing (breaks ffmpeg WHIP).
# When `str0m` bumps and depends on a different `is` version, Cargo logs
#
#   warning: Patch `is v...` was not used in the crate graph.
#
# and silently falls back to upstream — re-introducing the WHIP failure.
#
# This script runs `cargo build` and fails if that warning appears, so a
# version bump can't quietly regress. Run it from CI after every dependency
# update.
#
# Usage:
#   scripts/check-vendored-patch.sh                 # debug build
#   CARGO_PROFILE=release scripts/check-vendored-patch.sh

set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE_FLAG=""
if [[ "${CARGO_PROFILE:-}" == "release" ]]; then
    PROFILE_FLAG="--release"
fi

BUILD_LOG="$(mktemp -t check-vendored-patch.XXXXXX)"
trap 'rm -f "$BUILD_LOG"' EXIT

echo "Running: cargo build $PROFILE_FLAG"
# Capture stderr (where cargo emits warnings) and stdout. Fail the script
# if cargo itself fails.
if ! cargo build $PROFILE_FLAG 2>&1 | tee "$BUILD_LOG"; then
    echo "ERROR: cargo build failed" >&2
    exit 1
fi

if grep -E 'Patch .* was not used' "$BUILD_LOG" >/dev/null; then
    echo >&2
    echo "ERROR: vendored patch is no longer used in the crate graph." >&2
    echo "       The [patch.crates-io] block in Cargo.toml targets a version" >&2
    echo "       of \`is\` that no longer matches the resolved dependency." >&2
    echo "       See the long comment above [patch.crates-io] in Cargo.toml" >&2
    echo "       for re-applying the patch (or removing it if upstream landed" >&2
    echo "       a permissive parser)." >&2
    grep -E 'Patch .* was not used' "$BUILD_LOG" >&2
    exit 2
fi

echo "OK: vendored patch is in use."
