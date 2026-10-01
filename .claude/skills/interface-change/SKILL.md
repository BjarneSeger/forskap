---
name: interface-change
description: Checklist for changing the varlink API (adding/changing methods, types, or errors) — every generated artifact, handler, mock, doc, and client that must move together, and the CI traps if one is missed.
---

# Changing the varlink interface

Single source of truth: `forskap-api/varlink/org.thehoster.forskapd.varlink`.
Everything else is generated from it or must be updated by hand to match. Work through
this list top to bottom.

## 1. Edit the `.varlink` file

- Interface name is `org.thehoster.forskapd`. Keep the existing style: one blank
  line between declarations, optional params/fields as `?type`.
- Document in the file itself: a short `#` comment on the lines before each new type,
  method and error, and before each field or enum variant whose name doesn't say it
  all (units, when it is absent or empty, since which version it is sent). Comments go
  on their own lines: that is what both generators and systemd's `varlinkctl` parse,
  and what introspection shows. Leave the `interface` line without one: the Go
  generator would turn it into a second package comment of the binding.
- Bump the version in `forskap-api/Cargo.toml` **in the same feature commit**.
  Convention (see git history): the api crate's version moves inside the commit that
  changes the interface; the workspace version moves only in separate
  `chore: Bump version` commits. The api crate is dual-licensed MIT/Apache-2.0 —
  don't paste GPL-licensed code into it.

## 2. Rust side regenerates itself

`forskap-api/build.rs` runs `varlink_generator` into `$OUT_DIR` on every build;
`lib.rs` `include!`s it. No manual step — the next `cargo build` yields the new
`VarlinkInterface` trait, `Call_*` traits, and request/reply structs. Compile errors in
the daemon are the to-do list.

## 3. Daemon handlers

- Implement the method in `forskapd/src/handlers/varlink.rs`
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
  **Exception: a write that creates something** (`CreateWorkItem`) is not a `WriteOp` and
  never goes through `perform_write`/`defer`. `WriteOp`s are persisted and address an
  existing `(kind, project_id, iid)`; a create has no `iid` and no idempotency key, so
  a queued or replayed one could file its item twice. It calls GitLab once from the
  handler, replies `NotAuthenticated` for every dormancy reason and `GitlabError` for
  every failure, and after GitLab succeeded it must not fail any more: the created row
  goes to the sync worker (`SyncHandle::land_issue`, the only store writer) and the
  reply is sent whether or not that landed in time.
- **Read methods** only read the sync store (`self.sync.store()`); never call GitLab.
  New data to serve? Add a mirror type (`sync/model.rs`, `Resource` + `Stored`), a
  `Listing` variant (`gitlab.rs`) and a `Job` (`sync/jobs.rs`, planned in
  `sync/planner.rs`), then project it in `handlers/wire.rs`.
- New GitLab call needed? Reads are a new `Listing` variant (no trait change). A new
  write method, or a read of one object that is no listing (the epic lookup), goes on
  the `GitlabApi` trait in `gitlab.rs` **and** on the shared fake in `testing.rs` and
  `DemoGitlab` in `demo.rs`.
- **New method? Add its arm to the hand-written dispatcher** `handle_forskapd` in
  `forskapd/src/service.rs` (clone the arm of an argument-identical method) plus a
  `dispatch_has_an_arm_for_<method>` test next to `dispatch_has_an_arm_for_search`. A
  missing arm compiles fine and only fails at runtime as `MethodNotFound`. Every arm
  parses its `*_Args` through `args!()`, an empty one too (`let WhoAmI_Args {} =
  args!();`): that is what refuses an argument the method doesn't have.
- New field on a wire type? The store holds GitLab mirrors, not wire types: add the
  field to the mirror in `sync/model.rs` (lenient `serde(default)`) and bump that
  resource's `SCHEMA`, so every job syncing it runs full once and refills old rows.

## 4. CLI

New subcommand module in its group under `forskap-cli/src/cmd/`, wired into
`forskap-cli/src/cli.rs` and covered in `forskap-cli/src/cli_tests.rs`. Shell completions
are dynamic for bash/zsh/fish/nushell and through carapace (the shell calls `forskap`, so a
new subcommand completes by itself); a new argument taking an issue/MR number or a project
gets its completer in `forskap-cli/src/complete.rs` (`cli_tests.rs` counts them). The files
under `forskap-cli/completions/` (registration scripts, carapace spec) regenerate on every
build and are gitignored.

## 5. Docs

Update `forskapd/docs/varlink_interface.md` with what the `.varlink` comments can't
say: what a method does beyond its line, ordering, caching, edge cases, tables,
examples. Don't copy type bodies or field lists there — the doc points to the
`.varlink` file for the shapes, and a copy drifts.

## 6. Go binding (CI trap)

```sh
cd clients/go
go generate ./...   # copies the .varlink in, runs varlink-go-interface-generator
go build ./... && go vet ./...
```

Commit the regenerated `orgthehosterforskapd.go` — never hand-edit it. CI
(`.github/workflows/go-binding.yml`) regenerates and fails on `git diff` if the
committed binding is stale.

Once the change is on `main`, tag the binding with the api crate's version and push the
tag — without one, `go get` only offers consumers a pseudo-version:

```sh
git tag clients/go/v0.25.0 <commit on main>   # the version in forskap-api/Cargo.toml
git push origin clients/go/v0.25.0
```

A pushed tag is final: proxy.golang.org and the Go checksum database keep the first
content they saw, so never move or re-create one. A fix to the binding alone (`client.go`,
its README) bumps the api crate's patch version and gets the next tag. The `Release`
workflow only reacts to `v*` tags, so this one publishes nothing — and GoReleaser is kept
from reading it as the project's version (`git.ignore_tags` in `.goreleaser.yaml`, plus
the tag the workflow picks itself), which would put slashes into the artifact paths.

## 7. Verify

`cargo test`, then the `verify` skill to drive the new method end-to-end over the real
socket (mind its warning: real keychain credentials, real GitLab).
