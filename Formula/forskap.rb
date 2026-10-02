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

  stable do
    on_macos do
      url "https://github.com/BjarneSeger/forskap/releases/download/v1.0.0/forskap_1.0.0_darwin_arm64.tar.gz"
      sha256 "da7f4afdf5f5ec880e2f3a50d35819a503af9a53a8f36a411bca3a8377a35f49"
      depends_on ForskapAppleSiliconRequirement
    end
    on_linux do
      on_arm do
        url "https://github.com/BjarneSeger/forskap/releases/download/v1.0.0/forskap_1.0.0_linux_arm64.tar.gz"
        sha256 "2430ebfbf3fbace04e81f046fe27b51e6c9c173647227a0b55ad4412ecaeb4cf"
      end
      on_intel do
        url "https://github.com/BjarneSeger/forskap/releases/download/v1.0.0/forskap_1.0.0_linux_amd64.tar.gz"
        sha256 "049e0d761c5cf09ea479c3c44ae0818c7315bf3b5408fc69b49e17e3c8282e18"
      end
    end
  end

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
      assert_equal "[]", shell_output("#{bin}/forskap queue list --output json").strip
    ensure
      Process.kill("TERM", pid)
      Process.wait(pid)
    end
  end
end
