# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

GitLab time-tracking helpers: a caching daemon (`gitlab-trackrd`) that talks to GitLab and serves a varlink IPC socket, and a thin CLI (`tt`, crate `tt-cli`) that talks only to the daemon. Cargo workspace, Rust edition 2024, requires Rust 1.85+.

## Commands

```sh
cargo build                                   # whole workspace
cargo test                                    # all tests (inline #[cfg(test)] modules, no tests/ dirs)
cargo test -p gitlab-trackrd <name>           # single test by name filter
cargo fmt                                     # formatting is enforced; run before committing
cargo run -p gitlab-trackrd --bin gen-config-template   # annotated default daemon config
cargo bench -p gitlab-trackrd                 # local perf suite (never in CI; see gitlab-trackrd/docs/benchmarks.md)
cargo bench -p gitlab-trackrd -- --save-baseline main   # record baseline before a change
cargo bench -p gitlab-trackrd -- --baseline main        # compare against it after
```

Daemon logging: `GITLAB_TRACKRD_LOG=debug` (env-filter syntax, default `gitlab_trackrd=info`).

To verify daemon/CLI changes end-to-end at the real varlink surface, use the `verify` skill (`.claude/skills/verify/SKILL.md`): it builds both binaries and runs an isolated instance via `XDG_*` overrides. Note the daemon reads real keychain credentials and talks to the real GitLab — avoid driving write commands during verification.

## Workspace layout

- `gitlab-trackr-api/` — the varlink interface crate. **Single source of truth is `gitlab-trackr-api/varlink/org.thehoster.gitlab.trackrd.varlink`**; Rust types/traits are generated from it at build time (`build.rs` + `varlink_generator`). Versioned independently from the workspace and dual-licensed MIT/Apache-2.0 (the rest is GPL-3.0-only).
- `gitlab-trackrd/` — the daemon. Human-facing interface docs in `gitlab-trackrd/docs/varlink_interface.md` — keep in sync with the `.varlink` file.
- `tt-cli/` — binary `tt`. One module per subcommand under `src/cmd/`; shell-hook snippets under `src/hooks/`.
- `clients/go/` — **generated** Go binding. After changing the `.varlink` interface, run `go generate ./...` in `clients/go` and commit the result; CI (`go-binding.yml`) fails if the committed binding is stale. Never hand-edit `orgthehostergitlabtrackrd.go`.
- `gitlab-trackr/` + `catalog.toml` — noctalia-shell launcher provider (`/gl`, Luau + `plugin.toml`). Shells out to `tt search --output json` / `tt open`; no daemon access of its own. The repo root is a noctalia plugin *source* (`plugins source add … git <repo>`): the plugin dir must equal the id suffix and `catalog.toml` mirrors `plugin.toml` (id, version, plugin_api) — bump both together.

So an interface change typically touches: the `.varlink` file → daemon handler impls → `tt` command → `docs/varlink_interface.md` → regenerated Go binding. Use the `interface-change` skill for the full checklist. Related skills: `test-patterns` (mock/test conventions before writing daemon tests), `config-key` (adding daemon config keys).

## Daemon architecture (gitlab-trackrd)

The daemon is built around a shared session slot and the principle that it **never refuses to start or serve**:

- **Session state**: `ConnState` (`Connected(Session)` | `Dormant(DormancyReason)`) lives in `SessionSlot = Arc<RwLock<ConnState>>` (`handlers/mod.rs`). Dormant still serves cached reads; auth-requiring calls reply `NotAuthenticated` carrying the specific dormancy reason for the CLI to report.
- **Credentials** come only from the OS keychain (`secrets.rs`; oo7/Secret Service on Linux, Keychain on macOS), set via `tt login` — never from config files.
- **GitLab access** is confined to `gitlab.rs`, the only module that knows the `gitlab` crate. It sits behind the `GitlabApi` trait: reads are one generic paginated `list(&Listing)` plus the GraphQL `list_timelogs`, writes are one method each. Tests use the shared listing-routed fake in `testing.rs`.
- **Sync** (`sync/`): the only GitLab reader and the only writer of the store. `model.rs` holds serde mirrors of the GitLab resources (issues, MRs, projects, groups, boards, events, timelogs), deserialized straight from responses. `store.rs` keeps one fjall keyspace per resource keyed big-endian (a project's issues or a time window is one range scan), plus views (ordered key lists like "assigned to me"), per-job state and the account identity; a `Commit` lands rows, scope reconciliation and job state in one atomic batch. `engine.rs` runs one job at a time — demanded jobs first, else due jobs by priority — with per-job jittered due times (`schedule.rs`, pure) and a jittered gap between jobs; a 429 pauses the worker, a 5xx/rejection backs off only its job, a network error demotes the session. `planner.rs` derives the per-project jobs from the tracked set: projects with assigned issues/MRs, contribution events (`GET /events`) or timelogs inside `search.tracked_retention_hours` — so it works for users who never log time. Job states persist: a restart inside an interval costs GitLab nothing. There is deliberately **no TTL** and no read-through.
- **Reads** are pure store readers (`handlers/varlink.rs`, projections in `handlers/wire.rs`), corrected at read time for writes the sync hasn't picked up yet: a queued or just-applied close/unassign hides the item from the assigned views. `ClearCache` sends a clear to the worker (cancelling a fetch in flight) and waits for the foreground jobs to refill.
- **Writes** (`PostTime`, `Close`, …) share one cascade (`write.rs` `Write::apply`): try GitLab once, else queue in the persistent `RetryQueue` (`queue.rs`): fjall-backed, exponential backoff, dead-lettered after the retry window and surfaced via `tt queue`. 429 always retries, 5xx only for idempotent ops (a PostTime may already have landed). A write never demotes the session; once it lands, the jobs displaying it rerun.
- **Reconnect** (`reconnect.rs`): a background supervisor retries the connection whenever the session is `Dormant(Unreachable)` — at boot or after the sync worker demotes it mid-run (`commit_unreachable`, guarded on client identity so a stale in-flight failure can't clobber a fresh `tt login` session). The sync worker is the demotion authority; a 429/5xx at connect counts as unreachable, not as a rejected token.
- **Config**: layered TOML via confique (user `$XDG_CONFIG_HOME/gitlab-trackrd/config.toml` → package default → baked-in), hot-reloaded by `reload.rs` watching the file; a reload re-plans the sync worker.

## tt-cli conventions

`tt` is deliberately thin: argument parsing, local state, interactive UI — all GitLab access goes through the daemon socket. GitLab issues have a global `id` and a per-project `iid`; users know the `iid`, so issue-acting commands take `iid` positionally and resolve the project lazily (`cmd/project.rs`).

## Error handling

Prefer thiserror `#[from]` derivation over hand-written `From` impls. The daemon distinguishes transient (network), throttled (429/5xx) and permanent errors (`error.rs`); that split drives queue-retry vs dead-letter, session demotion vs per-job backoff, and connect-time dormancy reasons.
