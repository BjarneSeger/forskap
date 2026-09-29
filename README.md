# Various gitlab timetracking helpers

Ever wanted / had to use gitlabs timetracking, but never quite managed to integrate
it into you workflow? Then this is the repo for you! We have:

- [A background daemon that handles auth and caching](gitlab-trackrd/README.md)
- [A cli to communicate with it and to remind you to track](tt-cli/README.md)
- [A ready-to-import Go binding for the daemon's varlink interface](clients/go/README.md)

# Quickstart

## 1. Install

Install the `gitlab-trackr-utils` package (deb, rpm and arch packages plus prebuilt
binaries are on the releases tab). It ships the `gitlab-trackrd` daemon, the `tt`
CLI, shell completions, and systemd user units.

## 2. Start the daemon

The daemon is a systemd user unit — enable and start it:

```sh
systemctl enable --now --user gitlab-trackrd.service
```

## 3. Log in

```sh
tt auth login --host gitlab.com
```

This walks you through creating a personal access token with the appropriate scopes.
Paste it back to the prompt and you are logged in — the token is stored in your
platform's keystore (Secret Service/keyring on Linux, Keychain on macOS), never in a
file.

The daemon keeps working offline: reads serve the local cache, and time you log
while GitLab is unreachable is queued and posted once it reconnects.

## 4. Get reminded to track

```sh
tt time hook YOUR_SHELL >> YOUR_SHELL_RC    # bash, zsh, fish or nu
```

The hook fires a prompt at regular intervals asking what you were working on, with a
list of your assigned issues to pick from.

## 5. Everyday use

```sh
tt issue list              # your assigned issues, straight from the cache
tt mr list                 # your open merge requests
tt issue view 42           # what the cache knows about issue #42
tt issue open 42           # open it in the browser (and count the open)
tt mr close 7 -p team/api  # close merge request !7 of that project
tt search oauth token      # cached search; issues/MRs you open often rank first
tt time log 42 1h30m       # log time on issue #42
tt time log '!42' 1h30m    # ... on merge request !42 (or: tt time log 42 1h30m --mr)
tt time history            # what you tracked recently (including queued entries)
tt queue list              # writes that failed permanently; `retry`/`dismiss` them
tt sync refresh            # drop the cache and fetch again
```

`tt issue` and `tt mr` share their verbs (`list`, `view`, `open`, `close`, `assign`,
`unassign`) and take the number shown in GitLab. The project is looked up in the
cache; when a number exists in several projects, name one with `-p`, as full path
or numeric ID. Commands that print data take `-o json`.

## Config

The CLI config lives at the path shown by `tt config path`; get an annotated default
with `tt config template`. The daemon reads its own config — see
[gitlab-trackrd/README.md](gitlab-trackrd/README.md#configuration).

# Scripting and integrating

Script `tt` itself, or talk to the daemon's varlink socket directly — the interface
is documented in [the interface docs](gitlab-trackrd/docs/varlink_interface.md) and
available as the [`gitlab-trackr-api`](gitlab-trackr-api/README.md) Rust crate or the
[Go binding](clients/go/README.md).

## Noctalia launcher

[`gitlab-trackr/`](gitlab-trackr/README.md) is a
[noctalia-shell](https://noctalia.dev) launcher provider: type `/gl <query>` to search
issues, merge requests, projects and groups through `tt search`, and activate a result
to open it in the browser via `tt issue open` / `tt mr open` — which also counts the open, so the things you
visit most float to the top (and `/gl` on its own lists them). This repository doubles
as a noctalia plugin source (`catalog.toml`):

```sh
noctalia msg plugins source add gitlab-trackr git https://github.com/BjarneSeger/gitlab_trackr
noctalia msg plugins enable thehoster/gitlab-trackr
```

## GNOME Shell and KRunner

`tt integration search-provider` serves the same search to GNOME Shell's overview search and to
KRunner (Plasma 6) over D-Bus. The session bus starts it on demand and it exits again
when idle; picking a result opens it in the browser and counts the open like `tt issue open`.
Type `mr oauth`, `#42` or `!42` to narrow the kind, or just `oauth`.

The deb/rpm/arch package installs the registration files. From a `cargo install`, write
them yourself — GNOME Shell only reads providers from `$XDG_DATA_DIRS`, so this needs root
(`sudo` does not search `~/.cargo/bin`, hence the explicit path):

```sh
sudo "$(which tt)" integration search-provider install            # /usr/local/share
tt integration search-provider install --prefix ~/.local/share    # KRunner only, no root
```

Then reload the bus (`busctl --user call org.freedesktop.DBus /org/freedesktop/DBus
org.freedesktop.DBus ReloadConfig`), log out and in for GNOME Shell, or `kquitapp6 krunner`
for Plasma. To answer only searches that start with a word, like the `/gl` prefix, set
`search_provider.trigger_word = "gl"` in `tt config path` (KRunner reads it at startup).
