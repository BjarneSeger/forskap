---
name: interface-change
description: Checklist for changing the varlink API (adding/changing methods, types, or errors) — every generated artifact, handler, mock, doc, and client that must move together, and the CI traps if one is missed.
---

# Changing the varlink interface

Single source of truth: `gitlab-trackr-api/varlink/org.thehoster.gitlab.trackrd.varlink`.
Everything else is generated from it or must be updated by hand to match. Work through
this list top to bottom.

## 1. Edit the `.varlink` file

- Interface name is `org.thehoster.gitlab.trackrd`. Keep the existing style: one blank
  line between declarations, optional params/fields as `?type`.
- Bump the version in `gitlab-trackr-api/Cargo.toml` **in the same feature commit**.
  Convention (see git history): the api crate's version moves inside the commit that
  changes the interface; the workspace version moves only in separate
  `chore: Bump version` commits. The api crate is dual-licensed MIT/Apache-2.0 —
  don't paste GPL-licensed code into it.

## 2. Rust side regenerates itself

`gitlab-trackr-api/build.rs` runs `varlink_generator` into `$OUT_DIR` on every build;
`lib.rs` `include!`s it. No manual step — the next `cargo build` yields the new
`VarlinkInterface` trait, `Call_*` traits, and request/reply structs. Compile errors in
the daemon are the to-do list.

## 3. Daemon handlers

- Implement the method in `gitlab-trackrd/src/handlers/varlink.rs`
  (`impl VarlinkInterface for Handlers`). Follow the cascade style: validate eagerly
  (`issue_ref_error`, `looks_like_duration` in `handlers/mod.rs`), consult cache,
  fall back to GitLab, reply.
- Error replies: GitLab rejection → `call.reply_gitlab_error(msg)`; dormant session →
  `call.reply_not_authenticated(reason, detail)` via `dormant_args(&e)`.
- **Write methods** (anything mutating GitLab) go through the shared cascade: add a
  `WriteOp` variant in `write.rs` (its `apply` arm and `idempotent()` answer), then call
  `perform_write` and `reply_write!` in `varlink.rs` like the existing writes. It tries
  once, queues on `Unreachable` or a retryable error, and only a real GitLab rejection
  returns `GitlabError`. A write never demotes the session — the sync worker is the
  demotion authority. Extend `Job::affected_by` so the right views re-sync after it.
- **Read methods** only read the sync store (`self.sync.store()`); never call GitLab.
  New data to serve? Add a mirror type (`sync/model.rs`, `Resource` + `Stored`), a
  `Listing` variant (`gitlab.rs`) and a `Job` (`sync/jobs.rs`, planned in
  `sync/planner.rs`), then project it in `handlers/wire.rs`.
- New GitLab call needed? Reads are a new `Listing` variant (no trait change). A new
  write method goes on the `GitlabApi` trait in `gitlab.rs` **and** on the shared fake in
  `testing.rs`.
- **New method? Add its arm to the hand-written dispatcher** `handle_trackrd` in
  `gitlab-trackrd/src/service.rs` (clone the arm of an argument-identical method) plus a
  `dispatch_has_an_arm_for_<method>` test next to `dispatch_has_an_arm_for_search`. A
  missing arm compiles fine and only fails at runtime as `MethodNotFound`.
- New field on a wire type? The store holds GitLab mirrors, not wire types: add the
  field to the mirror in `sync/model.rs` (lenient `serde(default)`) and bump that
  resource's `SCHEMA`, so every job syncing it runs full once and refills old rows.

## 4. CLI

New subcommand module under `tt-cli/src/cmd/`, wired into `tt-cli/src/cli.rs`.
Shell completions under `tt-cli/completions/` regenerate from `cli.rs` on every build
(`tt-cli/build.rs`) and are committed — include the diff.

## 5. Docs

Update `gitlab-trackrd/docs/varlink_interface.md` — it documents every method, type,
and error. Keep it complete; it is the human-facing contract.

## 6. Go binding (CI trap)

```sh
cd clients/go
go generate ./...   # copies the .varlink in, runs varlink-go-interface-generator
go build ./... && go vet ./...
```

Commit the regenerated `orgthehostergitlabtrackrd.go` — never hand-edit it. CI
(`.github/workflows/go-binding.yml`) regenerates and fails on `git diff` if the
committed binding is stale.

## 7. Verify

`cargo test`, then the `verify` skill to drive the new method end-to-end over the real
socket (mind its warning: real keychain credentials, real GitLab).
