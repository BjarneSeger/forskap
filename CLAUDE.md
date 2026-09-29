# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A cached GitLab CLI with time-tracking helpers: a caching daemon (`forskapd`) that talks to GitLab and serves a varlink IPC socket, and a thin CLI (`forskap`, crate `forskap-cli`) that talks only to the daemon. Cargo workspace, Rust edition 2024, requires Rust 1.85+.

## Commands

```sh
cargo build                                   # whole workspace
cargo test                                    # all tests (inline #[cfg(test)] modules, no tests/ dirs)
cargo test -p forskapd <name>                 # single test by name filter
cargo fmt                                     # formatting is enforced; run before committing
cargo run -p forskapd --bin gen-config-template   # annotated default daemon config
cargo bench -p forskapd                       # local perf suite (never in CI; see forskapd/docs/benchmarks.md)
cargo bench -p forskapd -- --save-baseline main   # record baseline before a change
cargo bench -p forskapd -- --baseline main        # compare against it after
```

Daemon logging: `FORSKAPD_LOG=debug` (env-filter syntax, default `forskapd=info`).

To verify daemon/CLI changes end-to-end at the real varlink surface, use the `verify` skill (`.claude/skills/verify/SKILL.md`): it builds both binaries and runs an isolated instance via `XDG_*` overrides. Note the daemon reads real keychain credentials and talks to the real GitLab — avoid driving write commands during verification.

## Workspace layout

- `forskap-api/` — the varlink interface crate. **Single source of truth is `forskap-api/varlink/org.thehoster.forskapd.varlink`**; Rust types/traits are generated from it at build time (`build.rs` + `varlink_generator`). Versioned independently from the workspace and dual-licensed MIT/Apache-2.0 (the rest is GPL-3.0-only).
- `forskapd/` — the daemon. Human-facing interface docs in `forskapd/docs/varlink_interface.md` — keep in sync with the `.varlink` file.
- `forskap-cli/` — binary `forskap`. `src/cmd/` mirrors the command tree (one module per group, one per subcommand inside it); `cli.rs` is `include!`d by `build.rs` for the completions, so it stays clap-only; shell-hook snippets under `src/hooks/`; `packaging/` holds the GNOME Shell / KRunner / D-Bus registration files that both `forskap integration search-provider install` (via `include_str!`) and the nfpm package ship.
- `clients/go/` — **generated** Go binding. After changing the `.varlink` interface, run `go generate ./...` in `clients/go` and commit the result; CI (`go-binding.yml`) fails if the committed binding is stale. Never hand-edit `orgthehosterforskapd.go`.
- `forskap/` + `catalog.toml` — noctalia-shell launcher provider (`/gl`, Luau + `plugin.toml`). Shells out to `forskap search --output json` / `forskap issue|mr open`; no daemon access of its own. The repo root is a noctalia plugin *source* (`plugins source add … git <repo>`): the plugin dir must equal the id suffix and `catalog.toml` mirrors `plugin.toml` (id, version, plugin_api) — bump both together.

So an interface change typically touches: the `.varlink` file → daemon handler impls → `forskap` command → `docs/varlink_interface.md` → regenerated Go binding. Use the `interface-change` skill for the full checklist. Related skills: `test-patterns` (mock/test conventions before writing daemon tests), `config-key` (adding daemon config keys).

## Daemon architecture (forskapd)

The daemon is built around a shared session slot and the principle that it **never refuses to start or serve**:

- **Session state**: `ConnState` (`Connected(Session)` | `Dormant(DormancyReason)`) lives in `SessionSlot = Arc<RwLock<ConnState>>` (`handlers/mod.rs`). Dormant still serves cached reads; auth-requiring calls reply `NotAuthenticated` carrying the specific dormancy reason for the CLI to report.
- **Credentials** come only from the OS keychain (`secrets.rs`; oo7/Secret Service on Linux, Keychain on macOS), set via `forskap auth login` — never from config files.
- **GitLab access** is confined to `gitlab.rs`, the only module that knows the `gitlab` crate. It sits behind the `GitlabApi` trait: reads are one generic paginated `list(&Listing)` plus the GraphQL `list_timelogs` and the raw `project_avatar` download, writes are one method each, as are the two token calls (`token_info`, `rotate_token`). Tests use the shared listing-routed fake in `testing.rs`.
- **Sync** (`sync/`): the only GitLab reader and the only writer of the store. `model.rs` holds serde mirrors of the GitLab resources (issues, MRs, projects, groups, boards, events, timelogs), deserialized straight from responses. `store.rs` keeps one fjall keyspace per resource keyed big-endian (a project's issues or a time window is one range scan), plus views (ordered key lists like "assigned to me"), per-job state and the account identity; a `Commit` lands rows, scope reconciliation and job state in one atomic batch. `engine.rs` runs one job at a time — demanded jobs first, else due jobs by priority — with per-job jittered due times (`schedule.rs`, pure) and a jittered gap between jobs; a 429 pauses the worker, a 5xx/rejection backs off only its job, a network error backs off its job and demotes the session, a 401 parks the session as `TokenRejected` until `forskap auth login` (which clears every job's backoff) unless the keychain holds a newer token. `planner.rs` derives the per-project jobs from the tracked set: projects with assigned issues/MRs, contribution events (`GET /events`) or timelogs inside `search.tracked_retention_hours` — so it works for users who never log time. Only tracked *member* projects get a corpus (an assigned MR in gitlab-org/gitlab must not pull its history), capped at `search.max_items_per_project` most recently updated items each. Job states persist: a restart inside an interval costs GitLab nothing. There is deliberately **no TTL** and no read-through.
- **Project avatars** (`sync/avatars.rs`): launchers want a local file and a private project's avatar needs the token, so `Job::ProjectAvatar` (lowest priority, one per member project with an `avatar_url`) downloads `GET /projects/:id/avatar` into `$XDG_CACHE_HOME/forskapd/avatars/`. The job runs once: its fingerprint covers the hash of the URL (`Plan::fingerprint`), so only a changed avatar is fetched again. An `Avatar` row names the file (empty for a 404, an oversized or unknown image), so reads never touch the filesystem; the file name carries the fingerprint, so a new avatar is a new path. The worker sweeps files no row names after a prune, clear or account change, and at boot drops rows whose file is gone.
- **Reads** are pure store readers (`handlers/varlink.rs`, projections in `handlers/wire.rs`), corrected at read time for writes the sync hasn't picked up yet: a queued or just-applied close/unassign hides the item from the assigned views. `ClearCache` sends a clear to the worker (cancelling a fetch into the cleared slice) and waits for the jobs refilling what it cleared.
- **Writes** (`PostTime`, `Close`, …) share one cascade (`write.rs` `Write::apply`): try GitLab once, else queue in the persistent `RetryQueue` (`queue.rs`): fjall-backed; one coordinator task owns the stores and launches up to `queue.max_in_flight` attempts at once as spawned futures (writes to one issuable run one at a time, in enqueue order), backs each off exponentially, pauses all launches on a 429, and dead-letters after the retry window (surfaced via `forskap queue`). 429 always retries, 5xx only for idempotent ops (a PostTime may already have landed). A write never demotes the session; once it lands, the jobs displaying it rerun.
- **Reconnect** (`reconnect.rs`): a background supervisor retries the connection whenever the session is `Dormant(Unreachable)` — at boot or after the sync worker demotes it mid-run (`commit_unreachable`, guarded on client identity so a stale in-flight failure can't clobber a fresh `forskap auth login` session). The sync worker is the demotion authority; a 429/5xx at connect counts as unreachable, not as a rejected token.
- **Token rotation** (`rotate.rs`): a background supervisor reads the session token's scopes and lifetime (`GET /personal_access_tokens/self`) and rotates it once less than `min(auth.rotate_before_days, lifetime / 3)` is left (moved up to a day earlier by a per-daemon random share, so machines sharing a keychain don't rotate together) — by default only tokens carrying `self_rotate` (`auth.rotate`), never one without an expiry. GitLab revokes the old token with its answer, so the new one is written to the keychain first and the session swapped after (if the keychain keeps failing: swapped anyway, write retried in the background). The swap replaces only the rotated session or what a 401 of its revoked token left of it; a login or logout in between wins. A failed rotation never demotes the session. On a 401 the sync worker asks the keychain before parking the session: if it holds another token (rotated by a machine sharing the keychain), the session goes `Dormant(Unreachable)` and the reconnect supervisor connects with that one.
- **Rename migration** (`migrate.rs`, `secrets.rs`; `migrate.rs` in the CLI): the pre-rename `gitlab-trackrd` / `tt` directories, keychain entry and env vars are moved or still read, and the package ships a `tt` symlink for installed shell hooks.
- **Config**: layered TOML via confique (user `$XDG_CONFIG_HOME/forskapd/config.toml` → package default → baked-in), hot-reloaded by `reload.rs` watching the file; a reload re-plans the sync worker and re-evaluates the token rotation.

## forskap-cli conventions

`forskap` is deliberately thin: argument parsing, local state, interactive UI — all GitLab access goes through the daemon socket. GitLab issues have a global `id` and a per-project `iid`; users know the `iid`, so `forskap issue` / `forskap mr` take the `iid` positionally and resolve the project lazily (`cmd/project.rs`).

## Error handling

Prefer thiserror `#[from]` derivation over hand-written `From` impls. The daemon distinguishes transient (network), throttled (429/5xx) and permanent errors (`error.rs`); that split drives queue-retry vs dead-letter, session demotion vs per-job backoff, and connect-time dormancy reasons.
