#!/usr/bin/env bash
# Pack the release archive of one platform:
#
#   package-release.sh <version> <os> <arch> <bindir> [outdir]
#
# writes <outdir>/forskap_<version>_<os>_<arch>.tar.gz (outdir: dist) and
# prints its path. <bindir> holds the release builds of forskapd and forskap;
# the completions are the ones forskap-cli/build.rs wrote into the source
# tree while building them.
#
# This defines the layout the Homebrew formula installs from: flat, no top
# directory.
#
#   forskapd
#   forskap
#   LICENSE
#   README.md
#   completions/forskap.bash
#   completions/_forskap
#   completions/forskap.fish
#   completions/forskap.nu
#
# The macOS release job and brew.yml's checkout mode pack with this script;
# GoReleaser packs the Linux archives itself, and its `archives` entry in
# .goreleaser.yaml has to give them the same layout.
set -euo pipefail

usage="usage: package-release.sh <version> <os> <arch> <bindir> [outdir]"
version="${1:?$usage}"
os="${2:?$usage}"
arch="${3:?$usage}"
bindir="${4:?$usage}"
outdir="${5:-dist}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
completions="$root/forskap-cli/completions"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/completions" "$outdir"
outdir="$(cd "$outdir" && pwd)"

install -m 0755 "$bindir/forskapd" "$bindir/forskap" "$stage/"
install -m 0644 "$root/LICENSE" "$root/README.md" "$stage/"
install -m 0644 "$completions/forskap.bash" "$completions/_forskap" \
  "$completions/forskap.fish" "$completions/forskap.nu" "$stage/completions/"

archive="$outdir/forskap_${version}_${os}_${arch}.tar.gz"
# Files only, no directory entries, as GoReleaser writes them. COPYFILE_DISABLE
# keeps macOS tar from adding AppleDouble (._*) files for extended attributes.
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$archive" forskapd forskap LICENSE README.md \
  completions/forskap.bash completions/_forskap completions/forskap.fish \
  completions/forskap.nu
echo "$archive"
