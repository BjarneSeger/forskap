# GitLab Trackr — noctalia launcher provider

A [noctalia-shell](https://noctalia.dev) plugin that puts `tt search` behind the
launcher prefix `/gl`. Results come from the daemon's cache (instant, works offline),
activating an issue or merge request opens it in the browser through `tt open` — which
also counts the open, so what you visit most ranks first. Projects and groups open via
`xdg-open`.

Requires noctalia ≥ 5.1 (plugin API 24) and a running `gitlab-trackrd` with `tt` on the
`PATH` noctalia sees (or set the **tt binary** setting).

## Install

The repository is a noctalia plugin **source** (`catalog.toml` at the root, this plugin
in `gitlab-trackr/`), so add it as a git source and enable the plugin — noctalia clones
it, exports the plugin and keeps it updated alongside its other sources:

```sh
noctalia msg plugins source add gitlab-trackr git https://github.com/BjarneSeger/gitlab_trackr
noctalia msg plugins enable thehoster/gitlab-trackr
```

(noctalia clones a git source lazily — on `enable` or when the plugin store is opened —
so `noctalia msg plugins list` only shows the plugin after one of those.)

Or declaratively, in `~/.config/noctalia/config.toml`:

```toml
[plugins]
enabled = ["thehoster/gitlab-trackr"]

[[plugins.source]]
name     = "gitlab-trackr"
kind     = "git"
location = "https://github.com/BjarneSeger/gitlab_trackr"
```

For development, point a **path source** at a checkout instead (read-only for noctalia;
edits to `launcher.luau` hot-reload in place):

```sh
noctalia msg plugins source add gitlab-trackr-dev path ~/src/gitlab_trackr
```

Settings (**Settings → Plugins → GitLab Trackr**):

| Setting | Default | Meaning |
|---|---|---|
| tt binary | `tt` | Path to the `tt` CLI, for when `~/.cargo/bin` is not on noctalia's `PATH`. |
| Results per kind | `15` | `--limit` passed to `tt search`. |

## Use

| Input | Result |
|---|---|
| `/gl` | Issues/MRs you have opened, most opened first |
| `/gl oauth` | Everything matching `oauth` (issues, MRs, projects, groups) |
| `/gl mr oauth` | Merge requests only (`i`/`issue`, `mr`, `p`/`project`, `g`/`group` also work) |
| `/gl !42` / `/gl #42` | MR / issue number 42 |

Ranking is the daemon's (`open_count` desc, then last opened, then updated); the
plugin only forwards it as each row's `score`. With `shell.launcher.categories = true`
the launcher additionally shows Issues / Merge requests / Projects / Groups filter
buttons (`F6` cycles them).

Reset the statistics with `tt refresh --usage`.

## Changing the prefix

Move the trigger word (for example when another provider already owns `gl`) in your
noctalia config:

```toml
[shell.launcher.providers."thehoster/gitlab-trackr:search"]
prefix = "gt"
```
