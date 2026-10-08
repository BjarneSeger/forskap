#!/usr/bin/env bash
# The section of CHANGELOG.md a release's notes open with:
#
#   release-notes.sh <version> [changelog]
#   release-notes.sh --check [changelog]
#
# prints the `## <version> - <date>` section, heading included. A pre-release
# (1.2.0-rc.1) without a section of its own gets `## Unreleased`. Fails when
# there is no such section or it holds no entry: the release workflow runs
# this before it builds anything, so a tag without notes publishes nothing.
#
# Wrapped entries and paragraphs come out as one line each: GitHub renders the
# line breaks of a release's notes as written, unlike those of the file.
#
# --check fails on a `## ` heading that is neither `## Unreleased` nor
# `## <version> - YYYY-MM-DD`, the ones the lookup would miss.
#
# The release workflow hands the output to GoReleaser as the header of the
# release notes; GitHub's generated list of pull requests follows it.
set -euo pipefail

usage="usage: release-notes.sh <version> | --check [changelog]"
what="${1:?$usage}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
changelog="${2:-$root/CHANGELOG.md}"

if [[ "$what" == --check ]]; then
  heading='^## (Unreleased|[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)? - [0-9]{4}-[0-9]{2}-[0-9]{2})$'
  bad=0
  n=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    n=$((n + 1))
    if [[ "$line" == "## "* && ! "$line" =~ $heading ]]; then
      echo "$changelog:$n: neither '## Unreleased' nor '## <version> - YYYY-MM-DD': $line" >&2
      bad=1
    fi
  done <"$changelog"
  exit "$bad"
fi

# One section, its heading first. The version is compared as text: as a
# pattern its dots would match anything. A line that starts no block of its
# own (heading, list item, quote, table row) continues the one before it;
# fenced code is left alone.
section() {
  awk -v want="$1" '
    function flush() { if (block != "") print block; block = "" }
    /^## / { flush(); on = ($0 == "## " want || index($0, "## " want " - ") == 1) }
    !on { next }
    /^```/ { flush(); print; fenced = !fenced; next }
    fenced { print; next }
    /^[[:space:]]*$/ { flush(); print ""; next }
    /^#+ / || /^\|/ { flush(); print; next }
    /^[[:space:]]*([-*+]|[0-9]+\.) / || /^>/ { flush(); block = $0; next }
    { sub(/^[[:space:]]+/, ""); block = (block == "" ? $0 : block " " $0) }
    END { flush() }
  ' "$changelog"
}

notes="$(section "$what")"
if [[ -z "$notes" && "$what" == *-* ]]; then
  notes="$(section Unreleased)"
fi
if [[ -z "$notes" ]]; then
  echo "release-notes.sh: no section for $what in $changelog" >&2
  exit 1
fi
if ! grep -q '^- ' <<<"$notes"; then
  echo "release-notes.sh: the section for $what in $changelog has no entry" >&2
  exit 1
fi
printf '%s\n' "$notes"
