class Forskap < Formula
  desc "Cached GitLab CLI and daemon with time-tracking helpers"
  homepage "https://github.com/BjarneSeger/forskap"
  # Builds the source of the release it names. At the next stable tag the
  # release workflow replaces this file with a formula that installs the
  # release's prebuilt binaries (.github/scripts/bump-formula.sh renders it
  # with render-formula.sh).
  url "https://github.com/BjarneSeger/forskap/archive/refs/tags/v0.12.0.tar.gz"
  sha256 "973e4ca319135535c14bd832e2b2b1ad71ab69041cac5b6ce797aa716f12cf63"
  license "GPL-3.0-only"
  head "https://github.com/BjarneSeger/forskap.git", branch: "main"

  depends_on "rust" => :build

  def install
    # The daemon crate also has a packaging-only bin (gen-config-template).
    system "cargo", "install", "--bin", "forskapd", *std_cargo_args(path: "forskapd")
    system "cargo", "install", *std_cargo_args(path: "forskap-cli")

    # Written by forskap-cli/build.rs, as shipped in the deb/rpm/arch packages.
    # `COMPLETE=zsh forskap` prints the same registration minus the lines that
    # make an autoloaded `_forskap` complete on the first Tab already.
    bash_completion.install "forskap-cli/completions/forskap.bash" => "forskap"
    zsh_completion.install "forskap-cli/completions/_forskap"
    fish_completion.install "forskap-cli/completions/forskap.fish"
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
