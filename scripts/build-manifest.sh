#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Build the canonical `manifest.json` for a release.
#
# Usage:
#   build-manifest.sh <version> <channel> <sequence> <artifact_dir>
#
# Where `<artifact_dir>` contains the just-built tarballs alongside their
# `.sha256` siblings (one per arch/variant). The script discovers each
# pair, plucks the SHA-256 from the sidecar file, and emits a single
# canonical JSON document on stdout. The JSON is canonicalised via
# `jq -cS` so the byte sequence we sign is reproducible — every release
# workflow run that builds the same set of artefacts produces the same
# manifest bytes.
#
# The manifest schema is documented in `bilbycast-edge/src/upgrade/manifest.rs`.

set -euo pipefail

VERSION="${1:?usage: build-manifest.sh <version> <channel> <sequence> <artifact_dir>}"
CHANNEL="${2:?missing channel}"
SEQUENCE="${3:?missing sequence}"
ARTIFACT_DIR="${4:?missing artifact directory}"
DEVICE_TYPE="${5:-edge}"
RELEASE_REPO="${RELEASE_REPO:-Bilbycast/bilbycast-edge}"

if [[ ! -d "${ARTIFACT_DIR}" ]]; then
    echo "build-manifest.sh: artifact directory ${ARTIFACT_DIR} does not exist" >&2
    exit 1
fi

# Released-at: ISO-8601 UTC, second precision. The release workflow's
# clock is the source of truth — operators can later confirm signing
# time via the Rekor entry's integratedTime.
RELEASED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Walk artifact_dir for `*.tar.gz.sha256` sidecars. Each pair represents
# one (arch, variant) tuple. Naming convention (mirrors the existing
# release workflow):
#   bilbycast-edge-<arch>-<linux|darwin>[-<variant>].tar.gz
#   bilbycast-edge-<arch>-<linux|darwin>[-<variant>].tar.gz.sha256
artefacts_json="["
first=1
shopt -s nullglob
for sha_file in "${ARTIFACT_DIR}"/*.tar.gz.sha256; do
    tarball_name="$(basename "${sha_file%.sha256}")"
    tarball_path="${ARTIFACT_DIR}/${tarball_name}"
    if [[ ! -f "${tarball_path}" ]]; then
        echo "build-manifest.sh: skipping ${sha_file} — no matching tarball" >&2
        continue
    fi

    # First field of the sha256sum sidecar is the digest.
    sha256="$(awk '{ print $1 }' < "${sha_file}")"
    if [[ -z "${sha256}" || ${#sha256} -ne 64 ]]; then
        echo "build-manifest.sh: invalid sha256 in ${sha_file}" >&2
        exit 1
    fi

    # Parse arch + variant out of the tarball name.
    # Example tarballs:
    #   bilbycast-edge-x86_64-linux-full.tar.gz        → arch=x86_64-linux, variant=full
    #   bilbycast-edge-aarch64-linux-full.tar.gz       → arch=aarch64-linux, variant=full
    #   bilbycast-edge-x86_64-linux.tar.gz             → arch=x86_64-linux, variant=default (legacy)
    bare="${tarball_name#bilbycast-edge-}"
    bare="${bare%.tar.gz}"
    case "${bare}" in
        *-linux-rockchip)
            # Rockchip = the aarch64 full bundle PLUS the RK3568/RK3588 RKMPP
            # HW encoder, shipped as a distinct VARIANT in the SAME arch bucket
            # (arch stays aarch64-linux). An edge compiled with
            # `video-encoder-rkmpp` reports variant `rockchip`
            # (src/upgrade/mod.rs::default_variant) and selects this artefact,
            # while generic aarch64 nodes keep pulling `full`. Must precede the
            # `*-linux` arm (which would otherwise not match, but keep it first
            # for clarity).
            arch="${bare%-rockchip}"
            variant="rockchip"
            ;;
        *-linux-full)
            arch="${bare%-full}"
            variant="full"
            ;;
        *-linux)
            arch="${bare}"
            variant="default"
            ;;
        *-darwin-full|*-darwin)
            arch="${bare%-full}"
            variant="${bare##*-}"
            [[ "${variant}" != "full" ]] && variant="default"
            ;;
        *)
            echo "build-manifest.sh: cannot parse arch/variant from ${tarball_name}" >&2
            exit 1
            ;;
    esac

    url="https://github.com/${RELEASE_REPO}/releases/download/v${VERSION}/${tarball_name}"

    if [[ ${first} -eq 0 ]]; then
        artefacts_json+=","
    fi
    first=0
    artefacts_json+=$(jq -nc \
        --arg arch "${arch}" \
        --arg variant "${variant}" \
        --arg url "${url}" \
        --arg sha256 "${sha256}" \
        '{arch: $arch, variant: $variant, url: $url, sha256: $sha256}')
done
artefacts_json+="]"

if [[ "${artefacts_json}" == "[]" ]]; then
    echo "build-manifest.sh: no tarballs found under ${ARTIFACT_DIR}" >&2
    exit 1
fi

# Canonicalise via `jq -cS` so the byte sequence we sign is
# deterministic across runs and reorderable inputs.
jq -cS \
    --arg version "${VERSION}" \
    --arg device_type "${DEVICE_TYPE}" \
    --arg channel "${CHANNEL}" \
    --arg released_at "${RELEASED_AT}" \
    --argjson sequence "${SEQUENCE}" \
    --argjson artefacts "${artefacts_json}" \
    -n '{version: $version, device_type: $device_type, channel: $channel, released_at: $released_at, sequence: $sequence, artefacts: $artefacts}'
