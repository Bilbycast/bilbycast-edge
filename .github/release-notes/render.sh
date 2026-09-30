#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Render a release's notes from edge.md and print them on stdout.
#
#   render.sh <version> <sdi_built: true|false> <changes-file>
#
# The notes used to be one double-quoted `gh release create --notes "..."`
# argument in nightly-release.yml, and bash's quoting leaked into what was
# published: every `\\\\` line continuation reached readers as a literal `\\`,
# so each multi-line command in the notes failed when pasted, and the double
# quotes inside the h264_auto example were stripped. The text now lives in
# plain Markdown files and is substituted as data, so what is in them is what
# gets published. Run this locally to see exactly what a release will say.
set -euo pipefail

# The quoted replacements below are literal only from bash 4.3 on (older
# bash keeps the quotes); macOS still ships 3.2 as /bin/bash.
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 3) )); then
    echo "render.sh needs bash 4.3 or newer (this is ${BASH_VERSION})" >&2
    exit 2
fi

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <version> <sdi_built: true|false> <changes-file>" >&2
    exit 2
fi

dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
version="$1"
sdi_built="$2"
changes_file="$3"

case "${sdi_built}" in
    true)
        sdi_note="$(cat "${dir}/sdi-built.md")"
        sdi_runtime="$(cat "${dir}/sdi-runtime.md")"
        ;;
    false)
        sdi_note="$(cat "${dir}/sdi-absent.md")"
        sdi_runtime=""
        ;;
    *)
        echo "sdi_built must be true or false, got '${sdi_built}'" >&2
        exit 2
        ;;
esac

template="$(cat "${dir}/edge.md")"
# Check the template before filling it: each placeholder the right number of
# times, and no other brace anywhere. A placeholder that lost or gained a
# brace would otherwise be published as written, so the templates hold no
# braces of their own.
count() { grep -o "$1" <<< "${template}" | wc -l; }
if [ "$(count '{{VERSION}}')" -lt 1 ] \
    || [ "$(count '{{SDI_NOTE}}')" -ne 1 ] \
    || [ "$(count '{{SDI_RUNTIME}}')" -ne 1 ] \
    || [ "$(count '{{CHANGES}}')" -ne 1 ]; then
    echo "edge.md needs {{VERSION}} at least once and {{SDI_NOTE}}, {{SDI_RUNTIME}} and {{CHANGES}} exactly once" >&2
    exit 1
fi
stripped="${template//'{{VERSION}}'/}"
stripped="${stripped//'{{SDI_NOTE}}'/}"
stripped="${stripped//'{{SDI_RUNTIME}}'/}"
stripped="${stripped//'{{CHANGES}}'/}"
if grep -n '[{}]' <<< "${stripped}" >&2; then
    echo "edge.md has a brace outside a known placeholder (above; a misspelt one?)" >&2
    exit 1
fi
if grep -n '[{}]' <<< "${sdi_note}${sdi_runtime}" >&2; then
    echo "the SDI fragments must not contain braces (above)" >&2
    exit 1
fi

# Quoted replacements are literal: with bash 5.2's patsub_replacement an
# unquoted `&` in the replacement would stand for the matched text. CHANGES
# goes in last, so a commit subject that happens to contain a placeholder is
# published as written.
notes="${template//'{{VERSION}}'/"${version}"}"
notes="${notes//'{{SDI_NOTE}}'/"${sdi_note}"}"
notes="${notes//'{{SDI_RUNTIME}}'/"${sdi_runtime}"}"
notes="${notes//'{{CHANGES}}'/"$(cat "${changes_file}")"}"

printf '%s\n' "${notes}"
