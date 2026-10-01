#!/usr/bin/env bash
# Point Formula/forskap.rb at a release tag: rewrites its `url` and `sha256`.
#
#   bump-formula.sh v1.2.3 [formula]
#
# Run by the release workflow after GoReleaser published the tag. Exits 0
# without touching the formula for a pre-release tag or one that is not newer
# than what the formula already names, so re-running an old release is safe.
set -euo pipefail

tag="${1:?usage: bump-formula.sh <tag> [formula]}"
formula="${2:-Formula/forskap.rb}"
repo="${GITHUB_REPOSITORY:-BjarneSeger/forskap}"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "$tag is not a stable release tag; formula left alone"
  exit 0
fi

current="$(sed -n 's|^  url ".*/archive/refs/tags/\(v[^"]*\)\.tar\.gz"$|\1|p' "$formula")"
if [[ -z "$current" ]]; then
  echo "no release url found in $formula" >&2
  exit 1
fi
if [[ "$current" != "$tag" && "$(printf '%s\n' "$current" "$tag" | sort -V | tail -n 1)" == "$current" ]]; then
  echo "formula is at $current, newer than $tag; formula left alone"
  exit 0
fi

url="https://github.com/$repo/archive/refs/tags/$tag.tar.gz"
tarball="$(mktemp)"
trap 'rm -f "$tarball"' EXIT
curl --fail --silent --show-error --location --retry 5 --output "$tarball" "$url"
sha="$(sha256sum "$tarball" | cut -d ' ' -f 1)"

sed -i.bak \
  -e "s|^  url \".*\"\$|  url \"$url\"|" \
  -e "s|^  sha256 \".*\"\$|  sha256 \"$sha\"|" \
  "$formula"
rm -f "$formula.bak"

grep -q "^  url \"$url\"\$" "$formula"
grep -q "^  sha256 \"$sha\"\$" "$formula"
echo "$formula now names $tag ($sha)"
