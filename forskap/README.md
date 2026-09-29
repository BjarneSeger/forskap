# Forskap — noctalia launcher provider

A [noctalia-shell](https://noctalia.dev) plugin that puts `forskap search` behind the
launcher prefix `/gl`. Results come from the daemon's cache (instant, works offline),
activating an issue or merge request opens it in the browser through `forskap issue open` / `forskap mr open` — which
also counts the open, so what you visit most ranks first. Projects and groups open via
`xdg-open`.

Requires noctalia ≥ 5.1 (plugin API 24) and a running `forskapd` with `forskap` on the
`PATH` noctalia sees (or set the **forskap binary** setting).

## Install

The repository is a noctalia plugin **source** (`catalog.toml` at the root, this plugin
in `forskap/`), so add it as a git source and enable the plugin — noctalia clones
it, exports the plugin and keeps it updated alongside its other sources:

```sh
noctalia msg plugins source add forskap git https://github.com/BjarneSeger/forskap
noctalia msg plugins enable thehoster/forskap
```

(noctalia clones a git source lazily — on `enable` or when the plugin store is opened —
so `noctalia msg plugins list` only shows the plugin after one of those.)

Or declaratively, in `~/.config/noctalia/config.toml`:

```toml
[plugins]
enabled = ["thehoster/forskap"]

[[plugins.source]]
name     = "forskap"
kind     = "git"
location = "https://github.com/BjarneSeger/forskap"
```

For development, point a **path source** at a checkout instead (read-only for noctalia;
edits to `launcher.luau` hot-reload in place):

```sh
noctalia msg plugins source add forskap-dev path ~/src/forskap
```

Settings (**Settings → Plugins → Forskap**):

| Setting | Default | Meaning |
|---|---|---|
| forskap binary | `forskap` | Path to the `forskap` CLI, for when `~/.cargo/bin` is not on noctalia's `PATH`. |
| Results per kind | `15` | `--limit` passed to `forskap search`. |

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

Reset the statistics with `forskap sync refresh --scope usage`.

## Changing the prefix

Move the trigger word (for example when another provider already owns `gl`) in your
noctalia config:

```toml
[shell.launcher.providers."thehoster/forskap:search"]
prefix = "gt"
```
