#!/usr/bin/env bash
# Point Formula/forskap.rb at a release tag: renders it anew from the
# release's checksums file (render-formula.sh), so it downloads that
# release's prebuilt binaries.
#
#   bump-formula.sh v1.2.3 [formula]
#
# Run by the release workflow after GoReleaser published the tag; needs `gh`
# with a token (GH_TOKEN). Exits 0 without touching the formula for a
# pre-release tag or one that is not newer than what the formula already
# names, so re-running an old release is safe. Fails if the release lacks the
# archive of a platform the formula serves.
set -euo pipefail

tag="${1:?usage: bump-formula.sh <tag> [formula]}"
formula="${2:-Formula/forskap.rb}"
repo="${GITHUB_REPOSITORY:-BjarneSeger/forskap}"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "$tag is not a stable release tag; formula left alone"
  exit 0
fi

# The tag of the first url: a source tarball (archive/refs/tags/<tag>.tar.gz,
# the formula before 0.13) or a release asset (releases/download/<tag>/...).
current="$(sed -nE 's#^ +url ".*/(archive/refs/tags|releases/download)/(v[0-9][^/"]*)[/"].*#\2#p' "$formula" | head -n 1)"
current="${current%.tar.gz}"
if [[ -z "$current" ]]; then
  echo "no release url found in $formula" >&2
  exit 1
fi
if [[ "$current" != "$tag" && "$(printf '%s\n' "$current" "$tag" | sort -V | tail -n 1)" == "$current" ]]; then
  echo "formula is at $current, newer than $tag; formula left alone"
  exit 0
fi

version="${tag#v}"
checksums="forskap_${version}_checksums.txt"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
gh release download "$tag" --repo "$repo" --pattern "$checksums" --dir "$dir"

bash "$(dirname "${BASH_SOURCE[0]}")/render-formula.sh" --strict \
  "$version" "https://github.com/$repo/releases/download/$tag" "$dir/$checksums" >"$dir/forskap.rb"
mv "$dir/forskap.rb" "$formula"
echo "$formula now names $tag"
