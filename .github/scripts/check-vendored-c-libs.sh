#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: MPL-2.0
#
# Check every vendored C library submodule pin against its upstream releases.
#
# WHY THIS EXISTS
# ---------------
# `cargo audit` and `cargo deny` read the *Rust* dependency graph via
# `cargo metadata`. They have no concept of a git submodule holding C source,
# so neither tool can ever flag a vulnerable libsrt / FFmpeg / opus pin. That
# blind spot let Haivision libsrt v1.5.5 sit vendored for 9 days after v1.5.6
# shipped fixes for CVE-2026-55869 and CVE-2026-55868 (both CVSS 9.1, both
# reachable pre-authentication on a public SRT listener).
#
# Adding `submodules: recursive` to the cargo-deny job would NOT have caught
# it — the vendored C is invisible to those tools whether or not it is on disk.
# This script closes the gap directly: it compares each pinned gitlink against
# the upstream tag list.
#
# It deliberately reads only the parent repo's gitlink SHA (`git ls-tree`) and
# upstream's tag list (`git ls-remote`). No submodule content is fetched, so
# this stays cheap and does not depend on git.ffmpeg.org / gitlab.xiph.org
# serving a full clone.
#
# EXIT CODES
#   0  every pin is the newest stable release in its own major.minor series
#   1  at least one pin is behind a patch release in its series (these are
#      overwhelmingly security/bugfix releases — treat as actionable)
#
# A newer *minor* or *major* upstream release is reported as NOTICE only:
# moving series is a deliberate engineering decision (build-flag churn, ABI
# changes, broadcast-quality re-verification), not something CI should force.
#
# Usage: check-vendored-c-libs.sh [monorepo-root]   (default: cwd)

set -uo pipefail

ROOT="${1:-$PWD}"
status=0

# name | parent repo dir | submodule path | upstream url | stable-tag regex
LIBS=(
  "libsrt|bilbycast-libsrt-rs|libsrt-sys/vendor/srt|https://github.com/Haivision/srt.git|^v[0-9]+\.[0-9]+\.[0-9]+$"
  "ffmpeg|bilbycast-ffmpeg-video-rs|libffmpeg-video-sys/vendor/ffmpeg|https://git.ffmpeg.org/ffmpeg.git|^n[0-9]+\.[0-9]+(\.[0-9]+)?$"
  "opus|bilbycast-ffmpeg-video-rs|libffmpeg-video-sys/vendor/opus|https://gitlab.xiph.org/xiph/opus.git|^v[0-9]+\.[0-9]+(\.[0-9]+)?$"
  "fdk-aac|bilbycast-fdk-aac-rs|libfdk-aac-sys/vendor/fdk-aac|https://github.com/mstorsjo/fdk-aac.git|^v[0-9]+\.[0-9]+\.[0-9]+$"
  "mxl|bilbycast-mxl-rs|vendor/mxl|https://github.com/dmf-mxl/mxl.git|^v[0-9]+\.[0-9]+\.[0-9]+$"
)

# major.minor of a tag, with any leading v/n stripped.
series_of() { sed -E 's/^[vn]//; s/^([0-9]+\.[0-9]+).*/\1/' <<<"$1"; }

printf '%-10s %-12s %-12s %s\n' "LIBRARY" "PINNED" "LATEST" "VERDICT"
printf '%s\n' "-------------------------------------------------------------------------------"

for entry in "${LIBS[@]}"; do
  IFS='|' read -r name repo path upstream stable_re <<<"$entry"
  repo_dir="$ROOT/$repo"

  if [[ ! -d "$repo_dir/.git" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "-" "-" "SKIP (no $repo checkout)"
    continue
  fi

  pinned_sha="$(git -C "$repo_dir" ls-tree HEAD "$path" 2>/dev/null | awk '$2=="commit"{print $3}')"
  if [[ -z "$pinned_sha" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "-" "-" "SKIP (no gitlink at $path)"
    continue
  fi

  # tag -> commit sha. Annotated tags expose the commit via the ^{} peel line,
  # which must win over the tag-object SHA on the bare line.
  refs="$(git ls-remote --tags "$upstream" 2>/dev/null)"
  if [[ -z "$refs" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "?" "?" "SKIP (upstream unreachable)"
    continue
  fi

  declare -A TAGSHA=()
  while read -r sha ref; do
    [[ -z "${ref:-}" ]] && continue
    tag="${ref#refs/tags/}"
    if [[ "$tag" == *'^{}' ]]; then
      TAGSHA["${tag%^\{\}}"]="$sha"          # peeled commit — authoritative
    else
      [[ -z "${TAGSHA[$tag]+x}" ]] && TAGSHA["$tag"]="$sha"
    fi
  done <<<"$refs"

  pinned_tag=""
  for t in "${!TAGSHA[@]}"; do
    [[ "${TAGSHA[$t]}" == "$pinned_sha" ]] && { pinned_tag="$t"; break; }
  done

  # Guard every negative index explicitly: under `set -u`, ${arr[-1]} on an
  # empty array writes "bad array subscript" to stderr even when a :- default
  # rescues the value. In a security job that stray line reads like a defect.
  mapfile -t stable < <(printf '%s\n' "${!TAGSHA[@]}" | grep -E "$stable_re" | sort -V)
  latest=""
  (( ${#stable[@]} > 0 )) && latest="${stable[-1]}"

  if [[ -z "$pinned_tag" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "${pinned_sha:0:9}" "${latest:-?}" \
      "REVIEW (pin is not a release tag)"
    unset TAGSHA
    continue
  fi

  # Newest stable tag sharing the pinned tag's major.minor.
  want="$(series_of "$pinned_tag")"
  in_series=()
  for t in "${stable[@]}"; do
    [[ "$(series_of "$t")" == "$want" ]] && in_series+=("$t")
  done
  latest_in_series="$pinned_tag"
  (( ${#in_series[@]} > 0 )) && latest_in_series="${in_series[-1]}"

  if [[ "$pinned_tag" != "$latest_in_series" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "$pinned_tag" "$latest_in_series" \
      "BEHIND -- patch release in series (likely security)"
    status=1
  elif [[ "$pinned_tag" != "$latest" ]]; then
    printf '%-10s %-12s %-12s %s\n' "$name" "$pinned_tag" "$latest" \
      "NOTICE -- newer series available (manual call)"
  else
    printf '%-10s %-12s %-12s %s\n' "$name" "$pinned_tag" "$latest" "OK"
  fi

  unset TAGSHA
done

echo
if (( status != 0 )); then
  echo "FAIL: a vendored C library is behind a patch release in its own series."
  echo "      Patch releases on these projects are near-always security or"
  echo "      correctness fixes. Bump the submodule, rebuild, and re-run the"
  echo "      broadcast quality gates in testbed/BROADCAST_QUALITY_GATES.md"
  echo "      before releasing."
else
  echo "PASS: every vendored C library is at the newest patch release in its series."
fi
exit $status
