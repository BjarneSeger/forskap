# forskap

<img src="forskap-cli/packaging/icons/hicolor/scalable/apps/org.thehoster.forskap.svg" alt="" width="96">

A cached GitLab CLI with time-tracking helpers. Formerly `gitlab-trackr` / `tt` —
see [Upgrading from gitlab-trackr](#upgrading-from-gitlab-trackr).

Ever wanted / had to use gitlabs timetracking, but never quite managed to integrate
it into you workflow? Then this is the repo for you! We have:

- [A background daemon that handles auth and caching](forskapd/README.md)
- [A cli to communicate with it and to remind you to track](forskap-cli/README.md)
- [A ready-to-import Go binding for the daemon's varlink interface](clients/go/README.md)

# Quickstart

## 1. Install

Install the `forskap-utils` package (deb, rpm and arch packages plus prebuilt
binaries are on the releases tab). It ships the `forskapd` daemon, the `forskap`
CLI, shell completions, and systemd user units.

On macOS, install with [Homebrew](https://brew.sh). This repository is its own tap:

```sh
brew tap bjarneseger/forskap https://github.com/BjarneSeger/forskap
brew install forskap
```

Starting with the first release after 0.12.0, the formula installs that release's
prebuilt binaries (macOS on Apple silicon, Linux amd64 and arm64) and the shell
completions. Intel Macs are not supported by the formula: it refuses to install there
and points to `brew install --HEAD`, which builds `main` from source.

## 2. Start the daemon

The daemon is a systemd user unit — enable and start it:

```sh
systemctl enable --now --user forskapd.service
```

With Homebrew it is a launchd service, logging to `$(brew --prefix)/var/log/forskapd.log`:

```sh
brew services start forskap
```

## 3. Log in

```sh
forskap auth login --host gitlab.com
```

forskap needs GitLab 12 or newer; the project avatars need 16.9.

This walks you through creating a personal access token with the appropriate scopes.
Paste it back to the prompt and you are logged in — the token is stored in your
platform's keystore (Secret Service/keyring on Linux, Keychain on macOS), never in a
file.

The suggested scopes include `self_rotate`: with it the daemon replaces the token by a
fresh one shortly before it expires, so a short expiry doesn't mean logging in again
([details](forskapd/README.md#token-rotation)). `forskap auth status` shows when the
token expires and whether it is rotated.

The daemon keeps working offline: reads serve the local cache, and time you log
while GitLab is unreachable is queued and posted once it reconnects.

## 4. Get reminded to track

```sh
forskap time hook YOUR_SHELL >> YOUR_SHELL_RC    # bash, zsh, fish or nu
```

The hook fires a prompt at regular intervals asking what you were working on, with a
list of your assigned issues to pick from.

## 5. Everyday use

```sh
forskap issue list              # your assigned issues, straight from the cache
forskap mr list                 # your open merge requests
forskap issue view 42           # what the cache knows about issue #42
forskap issue open 42           # open it in the browser (and count the open)
forskap mr close 7 -p team/api  # close merge request !7 of that project
forskap issue create Fix the login -p team/api   # file an issue, assigned to you
forskap search oauth token      # cached search; issues/MRs you open often rank first
forskap epic open 5             # open epic &5 in the browser (GitLab Premium and up)
forskap time log 42 1h30m       # log time on issue #42
forskap time log '!42' 1h30m    # ... on merge request !42 (or: forskap time log 42 1h30m --mr)
forskap time history            # what you tracked recently (including queued entries)
forskap activity --days 30      # what you did on GitLab: pushes, comments, opened and merged items
forskap queue list              # writes that failed permanently; `retry`/`dismiss` them
forskap sync refresh            # drop the cache and fetch again
forskap sync jobs               # what the background sync runs now, next, and what failed
```

`forskap issue` and `forskap mr` share their verbs (`list`, `view`, `open`, `close`, `assign`,
`unassign`) and take the number shown in GitLab. The project is looked up in the
cache; when a number exists in several projects, name one with `-p`, as full path
or numeric ID. `forskap issue create <title> -p <project>` files a new issue
(`--description`, `--label`, `--epic`, `--no-assign`); it always names its project,
and unlike the other writes it is not queued while GitLab is unreachable: it fails,
so nothing is created behind your back later. `forskap epic` has `view` and `open`; epics belong to a group, so an
ambiguous number takes `-g`. Commands that print data take `-o json` or `-o yaml`.
On a terminal the text output colours state words, headings and item numbers;
`--color always|never` overrules that, and `NO_COLOR` is honoured.
`forskap sync jobs` and `forskap queue list` take `-w`/`--watch [SECS]` to redraw
their text view every SECS seconds (2 by default) until Ctrl-C; while the daemon
is away the watch shows the error and keeps trying.

`forskap sync jobs` puts the jobs that are done for good (a fetched avatar per
project) on one line per kind; `-a`/`--all` lists each.

## Config

The CLI config lives at the path shown by `forskap config path`; get an annotated default
with `forskap config template`. The daemon reads its own config — see
[forskapd/README.md](forskapd/README.md#configuration).

# Scripting and integrating

Script `forskap` itself, or talk to the daemon's varlink socket directly — the interface
is documented in [the interface docs](forskapd/docs/varlink_interface.md) and
available as the [`forskap-api`](forskap-api/README.md) Rust crate or the
[Go binding](clients/go/README.md).

## Noctalia launcher

[`forskap/`](forskap/README.md) is a
[noctalia-shell](https://noctalia.dev) launcher provider: type `/gl <query>` to search
issues, merge requests, epics, projects and groups through `forskap search`, and activate a result
to open it in the browser via `forskap issue open` / `forskap mr open` / `forskap epic open` — which also counts the open, so the things you
visit most float to the top (and `/gl` on its own lists them). Results show the avatar
of their project. This repository doubles as a noctalia plugin source (`catalog.toml`):

```sh
noctalia msg plugins source add forskap git https://github.com/BjarneSeger/forskap
noctalia msg plugins enable thehoster/forskap
```

## GNOME Shell and KRunner

`forskap integration search-provider` serves the same search to GNOME Shell's overview search and to
KRunner (Plasma 6) over D-Bus. The session bus starts it on demand and it exits again
when idle; picking a result opens it in the browser and counts the open like `forskap issue open`.
Type `mr oauth`, `#42` or `!42` to narrow the kind, or just `oauth`. Results show the
avatar of their project, where it has one.

The deb/rpm/arch package installs the registration files. From a `cargo install`, write
them yourself — GNOME Shell only reads providers from `$XDG_DATA_DIRS`, so this needs root
(`sudo` does not search `~/.cargo/bin`, hence the explicit path):

```sh
sudo "$(which forskap)" integration search-provider install            # /usr/local/share
forskap integration search-provider install --prefix ~/.local/share    # KRunner only, no root
```

Then reload the bus (`busctl --user call org.freedesktop.DBus /org/freedesktop/DBus
org.freedesktop.DBus ReloadConfig`), log out and in for GNOME Shell, or `kquitapp6 krunner`
for Plasma. To answer only searches that start with a word, like the `/gl` prefix, set
`search_provider.trigger_word = "gl"` in `forskap config path` (KRunner reads it at startup).

# Upgrading from gitlab-trackr

The project was renamed: `gitlab-trackrd` is now `forskapd`, `tt` is `forskap`, the
package is `forskap-utils` (it replaces `gitlab-trackr-utils`).

Config, cache, queued writes and the login move over on the first start. What is left
to do by hand:

```sh
systemctl disable --now --user gitlab-trackrd.service gitlab-trackrd.socket
systemctl enable --now --user forskapd.socket
```

- The package keeps a `tt` symlink, so installed shell hooks keep working; re-run
  `forskap time hook YOUR_SHELL` to get the new snippet.
- After a `cargo install`, run `forskap integration search-provider install` again; it
  removes the old registration files.
- noctalia: add the source again and enable `thehoster/forskap`.
- Varlink clients: the interface is now `org.thehoster.forskapd`, the Go module
  `github.com/BjarneSeger/forskap/clients/go`.
- `GITLAB_TRACKRD_SOCKET` and `GITLAB_TRACKRD_LOG` are still read; prefer
  `FORSKAPD_SOCKET` and `FORSKAPD_LOG`.

# License

The daemon, the CLI and the noctalia plugin are licensed under
[GPL-3.0-only](LICENSE). The [`forskap-api`](forskap-api/README.md) crate and the
[Go binding](clients/go/README.md) are licensed under either Apache-2.0 or MIT, at
your option; both directories carry the two license texts.
