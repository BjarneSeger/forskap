# forskap-cli
A CLI for GitLab, served from the cache of `forskapd`.

Besides issues, merge requests and search, forskap integrates time tracking into your
workflow by regularly asking you what you worked on, in the terminal.

# Setup

## Installation
Prebuilt binaries are available in the releases and packages are built for debian,
rpm and arch.
After installing the package, make sure to enable the systemd socket:

```sh
systemctl enable --now --user forskapd.socket
```

On macOS, install with [Homebrew](https://brew.sh) from the tap in this repository
(built from source, daemon included; needs a release newer than 0.12.0) and run the
daemon as a service:

```sh
brew tap bjarneseger/forskap https://github.com/BjarneSeger/forskap
brew install forskap
brew services start forskap
```

Then run

```sh
forskap time hook <SHELL>
```

to get the snippet to add to your respective shellrc. After that, you will be asked
after 30 minutes when the next cli prompt should appear what you are working on, 
with a list of assigned issues.

### Completions
Out of the box, completions are installed for fish, zsh and bash. They are
dynamic: the shell asks `forskap` on every Tab, so besides commands and flags
it completes what the daemon has cached — issue and merge request numbers
(the item you last logged time on, then your assigned ones, then the ones you
opened before), epic numbers (the ones you opened before), and project and
group paths for `--project` and `--group`. fish and zsh show the title and
project or group next to each number; bash only the numbers. Without a running
daemon you still get the commands and flags.

If you didn't install the package, register them from your shell's rc file:

```sh
source <(COMPLETE=bash forskap)     # ~/.bashrc
source <(COMPLETE=zsh forskap)      # ~/.zshrc, after compinit
COMPLETE=fish forskap | source      # ~/.config/fish/config.fish
```

Completions are also provided for carapace, but they need to be manually linked from
`/usr/share/carapace/specs/forskap.yaml` to `~/.config/carapace/specs/forskap.yaml`, as
carapace does not currently support globally installed specs. The carapace
spec is static: commands and flags only. Nushell has no completions of its own.

### Ambiguous numbers
`forskap issue` / `forskap mr` and `forskap time log` take the number alone and find
the project themselves. If the number exists in several projects, a picker
lists them by project and title; Esc cancels. When not run from a terminal (scripts,
pipes, launchers) this stays an error asking for `--project`.

`forskap epic` does the same across groups, with `--group`.

### Is it working?
`forskap status` checks the whole setup in one go: whether the daemon answers
on its socket and runs the same version as the CLI, whether it is logged in to
GitLab (and how long the token lasts), whether sync jobs fail, hang or are left
waiting, and whether queued writes failed for good. Each check is `ok`,
`warning` or `error` — or `skipped`, when the daemon can't be reached at all —
with a line on what to do. `-o json` gives the same as a document with the
numbers behind it, and `-w` keeps redrawing it.

The exit status is non-zero only when a check is an error: an unreachable or
stuck daemon, a login that needs `forskap auth login`, a sync job that hangs.
Warnings, like a project whose merge requests GitLab refuses, exit zero, so
`forskap status >/dev/null || …` in a script or prompt fires only when
something is broken.

### Colour
On a terminal the text output colours the state words (`opened`, `merged`, a
sync job's `running`, a check's `warning`, …), the headings and the item
numbers (`#42`, `!7`, `&5`).
Pipes, scripts and launchers get plain text, and `--output json` is never
coloured. `--color always` or `--color never` overrules that on any command;
`NO_COLOR` turns it off and `CLICOLOR_FORCE` on.

## Config
The config lives at `$XDG_CONFIG_HOME/` or `$HOME/.config/` under
`forskap/config.toml` (on macOS: `~/Library/Application Support/forskap/config.toml`).
You can run `forskap config path` to see what it 
resolves to on your system. To get a sample config, run

```sh
forskap config template
# Save the default config
# forskap config template > $(forskap config path)
```
