---
name: config-key
description: Checklist for adding or changing a daemon config key (confique struct conventions, accessors, clamping, hot-reload rules, which docs to touch and which artifacts are generated).
---

# Adding / changing a daemon config key

Config lives in `forskapd/src/config.rs` (confique, layered TOML: user file →
`/usr/share/forskapd/config.toml` → baked-in defaults; every key optional).

## 1. The field

- Add it to the sub-struct matching its TOML table (`ServerConfig`, `Quick/SlowRefreshConfig`,
  `HistoryConfig`, `QueueConfig`, `ReconnectConfig`, `SearchConfig`, `UsageConfig`,
  `SyncConfig`) with `#[config(default = …)]` and a
  doc comment. **The doc comment becomes the annotation in the generated config
  template** — write it for end users, include the default's meaning ("30 min", "90 days").
- New TOML table → new `#[config(nested)]` struct, owned by the module that consumes it.
  confique bakes defaults per *type*, so two tables with the same shape but different
  defaults need two structs (see `QuickRefreshConfig` vs `SlowRefreshConfig`).
- Raw `*_secs`/`*_hours` field + typed accessor is the convention: `interval() -> Duration`,
  `retention() -> Duration`, etc. Consumers never convert units themselves.
- Backoff-style `base/max` pair? Add a `normalize_backoff(..)` call in `load()` so a
  hand-edited config can't busy-spin or invert the schedule. Intervals get a floor
  (`normalize_refresh`, `normalize_search`) and fractions a clamp (`normalize_sync`).

## 2. The consumer

- Read through `SharedConfig` (`Arc<RwLock<Config>>`) **at the moment of use**: extract
  the value in a single statement so the guard drops before any `.await` — never hold it
  across one.
- Periodic work belongs to the sync worker: a job's cadence comes from
  `Job::cadence(&Config)` and is re-read on every due computation, and a reload calls
  `SyncHandle::reconfigure` to re-plan. A key that changes what a job fetches (a window,
  a schema) belongs in `Job::fingerprint`, so a change makes the job run full at once.
- Hot-reload semantics (`reload.rs`): parse errors keep the last-good config; a changed
  `server.socket` only logs a warning (socket changes need a restart). If the new key
  cannot take effect without a restart, make `reload.rs` warn about it the same way.

## 3. Docs — one manual spot, the rest is generated

- Update the config table in `forskapd/README.md` (key, default, description).
  This is the only hand-maintained copy.
- Do **not** touch `forskapd/packaging/config.toml` or any template output: the
  shipped default config is regenerated on every release by the goreleaser hook running
  `cargo run -p forskapd --bin gen-config-template`.

## 4. Verify

```sh
cargo run -p forskapd --bin gen-config-template   # new key + annotation present?
cargo test -p forskapd config                     # config unit tests
```

For reload behavior, the `verify` skill's isolated instance + editing
`$XDG_CONFIG_HOME/forskapd/config.toml` exercises the watcher end-to-end.
