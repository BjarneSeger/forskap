#!/usr/bin/env bash
# Render the Homebrew formula that installs a release's prebuilt binaries:
#
#   render-formula.sh [--strict] <version> <base-url> <checksums>
#
# <checksums> is a GoReleaser checksums file (`<sha256>  <file>` per line),
# <base-url> where the archives it names are downloaded from, e.g.
# https://github.com/BjarneSeger/forskap/releases/download/v1.2.3. The formula
# goes to stdout and downloads the archive of each platform the checksums file
# has (forskap_<version>_<os>_<arch>.tar.gz, laid out by package-release.sh):
# darwin_arm64, linux_arm64 and linux_amd64. With --strict a missing one is
# an error, as it is for a release; brew.yml renders the formula for the one
# archive it packed. `head` builds from source on every platform.
set -euo pipefail

usage="usage: render-formula.sh [--strict] <version> <base-url> <checksums>"
strict=false
if [[ "${1:-}" == --strict ]]; then
  strict=true
  shift
fi
version="${1:?$usage}"
base="${2:?$usage}"
base="${base%/}"
checksums="${3:?$usage}"

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "render-formula.sh: not a version: $version" >&2
  exit 1
fi

# sha_<platform>: what the checksums file has for that archive, if anything.
# (No associative arrays: macOS runs this with bash 3.2.)
found=""
missing=""
for platform in darwin_arm64 linux_arm64 linux_amd64; do
  name="forskap_${version}_${platform}.tar.gz"
  sum="$(awk -v name="$name" '$2 == name || $2 == "*" name { print $1; exit }' "$checksums")"
  if [[ -z "$sum" ]]; then
    missing="$missing $name"
  elif [[ ! "$sum" =~ ^[0-9a-f]{64}$ ]]; then
    echo "render-formula.sh: $checksums: bad sha256 for $name: $sum" >&2
    exit 1
  else
    found="$found $platform"
  fi
  printf -v "sha_$platform" '%s' "$sum"
done
if [[ -z "$found" ]] || { $strict && [[ -n "$missing" ]]; }; then
  echo "render-formula.sh: $checksums lacks$missing" >&2
  exit 1
fi

# url and sha256 of one platform's archive, indented by $2.
archive() {
  local sum="sha_$1"
  printf '%surl "%s/forskap_%s_%s.tar.gz"\n' "$2" "$base" "$version" "$1"
  printf '%ssha256 "%s"\n' "$2" "${!sum}"
}

stable() {
  echo "  stable do"
  # Homebrew reads the version from a GitHub release URL, but not from the
  # archive name alone: from a file:// URL it would read `64` (arm64).
  if [[ "$base" != https://github.com/*/releases/download/v"$version" ]]; then
    echo "    version \"$version\""
  fi
  if [[ -n "$sha_darwin_arm64" ]]; then
    # One archive for every Mac; the requirement turns Intel Macs away.
    echo "    on_macos do"
    archive darwin_arm64 "      "
    echo "      depends_on ForskapAppleSiliconRequirement"
    echo "    end"
  fi
  if [[ -n "$sha_linux_arm64$sha_linux_amd64" ]]; then
    echo "    on_linux do"
    if [[ -n "$sha_linux_arm64" ]]; then
      echo "      on_arm do"
      archive linux_arm64 "        "
      echo "      end"
    fi
    if [[ -n "$sha_linux_amd64" ]]; then
      echo "      on_intel do"
      archive linux_amd64 "        "
      echo "      end"
    fi
    echo "    end"
  fi
  echo "  end"
}

cat <<'RUBY'
# Rendered by .github/scripts/render-formula.sh, which the release workflow
# runs for every stable tag: change the script, not this file.

# The prebuilt macOS binaries are arm64 only. HEAD has no such requirement
# and builds from source on an Intel Mac as well.
class ForskapAppleSiliconRequirement < Requirement
  fatal true

  satisfy(build_env: false) { Hardware::CPU.arm? }

  def display_s
    "Apple silicon"
  end

  def message
    <<~EOS
      The prebuilt macOS binaries are for Apple silicon (arm64) only.
      To build forskap from source on an Intel Mac, run:
        brew install --HEAD bjarneseger/forskap/forskap
    EOS
  end
end

class Forskap < Formula
  desc "Cached GitLab CLI and daemon with time-tracking helpers"
  homepage "https://github.com/BjarneSeger/forskap"
  license "GPL-3.0-only"

RUBY
stable
cat <<'RUBY'

  head do
    url "https://github.com/BjarneSeger/forskap.git", branch: "main"

    depends_on "rust" => :build
  end

  def install
    if build.head?
      # The daemon crate also has a packaging-only bin (gen-config-template).
      system "cargo", "install", "--bin", "forskapd", *std_cargo_args(path: "forskapd")
      system "cargo", "install", *std_cargo_args(path: "forskap-cli")
      # Written by forskap-cli/build.rs; the release archives carry them.
      completions = buildpath/"forskap-cli/completions"
    else
      bin.install "forskapd", "forskap"
      completions = buildpath/"completions"
    end

    # As shipped in the deb/rpm/arch packages. `COMPLETE=zsh forskap` prints
    # the same registration minus the lines that make an autoloaded
    # `_forskap` complete on the first Tab already.
    bash_completion.install completions/"forskap.bash" => "forskap"
    zsh_completion.install completions/"_forskap"
    fish_completion.install completions/"forskap.fish"
    # Nushell loads every file there at startup.
    (share/"nushell/vendor/autoload").install completions/"forskap.nu"
  end

  # A unit of the user who starts it, and so is its log; on Linux that
  # user's journal has it.
  service do
    run opt_bin/"forskapd"
    keep_alive true
    # Keeps colour escapes out of the log file.
    environment_variables NO_COLOR: "1"
    if OS.mac?
      log_path "~/Library/Logs/forskapd.log"
      error_log_path "~/Library/Logs/forskapd.log"
    end
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/forskap --version")

    # The daemon serves without credentials. Only its config file can move the
    # socket on macOS, where the XDG variables are not read.
    ENV["XDG_CONFIG_HOME"] = testpath/".config"
    ENV["XDG_DATA_HOME"] = testpath/".local/share"
    ENV["XDG_STATE_HOME"] = testpath/".local/state"
    ENV["XDG_CACHE_HOME"] = testpath/".cache"
    ENV["XDG_RUNTIME_DIR"] = testpath
    socket = testpath/"forskapd.socket"
    config_home = OS.mac? ? testpath/"Library/Application Support" : testpath/".config"
    (config_home/"forskapd/config.toml").write <<~TOML
      [server]
      socket = "#{socket}"
    TOML
    ENV["FORSKAPD_SOCKET"] = "unix:#{socket}"

    pid = spawn bin/"forskapd"
    begin
      60.times do
        break if socket.exist?

        sleep 0.5
      end
      assert_predicate socket, :socket?
      # Nothing queued and nothing failed: one object since 1.3, the array of
      # failures before.
      listed = JSON.parse(shell_output("#{bin}/forskap queue list --output json"))
      assert_empty listed["queued"]
      assert_empty listed["failures"]
    ensure
      Process.kill("TERM", pid)
      Process.wait(pid)
    end
  end
end
RUBY
