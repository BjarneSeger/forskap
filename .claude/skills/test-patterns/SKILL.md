---
name: test-patterns
description: The established mock and test conventions in this workspace (the shared FakeGitlab, handler and sync-engine scaffolding, varlink call driving, timing rules) — read before writing or extending daemon tests so new tests reuse the existing helpers.
---

# Test patterns in forskapd

All tests are inline `#[cfg(test)]` modules — no `tests/` dirs. There is **one**
GitLab mock; reuse the helpers below instead of inventing new scaffolding.

## The shared fake (`src/testing.rs`)

`FakeGitlab` implements `GitlabApi` for every suite (handlers, sync, queue,
reconnect).
- **Reads** are routed by `route(&Listing)`: the listing's `path()`, except for the
  two recent issue lists. They request `issues` like the assigned list, so the fake
  routes them by `RECENT_AUTHORED_PATH` (`"issues?authored"`) and
  `RECENT_ASSIGNED_PATH` (`"issues?assigned"`), and a test serving or counting
  `"issues"` still means the assigned list (or `AllIssues`). A new listing that
  shares a path with an existing one gets its own route there too.
  `serve(path, rows)` sets the JSON rows
  every call to that path returns (empty by default), `serve_next(path, rows)`
  answers only the next call (e.g. a page walk that differs from its follow-up
  delta); `fail_next(path, FakeErr)` queues one-shot failures; `gate(path)` holds the next call until the returned
  `Notify` fires (`fake.gated` signals that the call started). `serve_timelogs(..)`
  covers the GraphQL timelog read. A read reports to the `Progress` it is handed like
  GitLab's first page would: the rows it is about to serve as the total before its
  gate, the rows themselves after, so a gated fetch is at `0/n` in the snapshot. A
  call outside a sync run passes `&Progress::default()`.
- **Writes** succeed unless `fail_next_write(FakeErr)` queued a failure; `writes()`
  logs `(op, kind, project_id, iid)`.
  `gate_writes()` holds every write until the returned gate's `release()` (sticky);
  a held write is already in `writes()`, so `writes().len()` counts started attempts.
  A create is a write too, logged as `("create_issue", Issuable::Issue, project_id, 0)`:
  `serve_create(row)` sets the issue the next create answers with (without one it is
  `issue_json(project_id, 1, title)`), `created()` returns the `(project_id, NewIssue)`
  of every attempt, failed ones included.
- **Token calls**: `serve_token(info)` sets what `token_info` returns (a token
  without expiry by default); the n-th `rotate_token` yields the token `rotated-n`.
  Both fail through `fail_next(TOKEN_PATH | ROTATE_PATH, err)`; `rotations()` logs
  the `expires_at` of every attempt, `token_info_calls()` counts the reads.
- **Avatars**: `serve_avatar(project_id, bytes)` sets a project's image (`PNG` is
  a minimal one), a project without one answers like GitLab's 404; failures go
  through `fail_next("projects/<id>/avatar", err)`, `avatar_calls()` logs the
  downloads. `project_json_with_avatar(id, file)` is a member project with one.
- Assert on traffic with `calls()`, `calls_to(path)`, `timelog_calls()`,
  `read_calls()` — e.g. "a read never touches GitLab" is `read_calls() == 0`.
- `FakeErr::{Transient, Throttled(status), Rejected, RejectedWith(status),
  Unauthorized}` build the matching `Error` (`Unauthorized` is a 401: a dead token).
  `Rejected` is GitLab's 403 Forbidden, `RejectedWith(404 | 400 | 422 | …)` a
  rejection with another status; both are `Error::Rejected { status, .. }`, whose
  status the sync engine counts refusals (403/404) by. A failure without a status
  (an unreadable page) is a plain `Error::Gitlab`.
- `project_json(id)` is a member project with every feature on;
  `project_json_without(id, "issues" | "merge_requests" | "repository")` one with
  that feature switched off, for the planner's feature levels.
- JSON builders `issue_json`, `event_json`; `eventually(what, || cond)` polls up to 2 s.

## Handler tests (`src/handlers/tests.rs`)

**Scaffolding**: `handlers_with(state) -> (Handlers, TempDir)` opens a real fjall DB
in a tempdir and spawns the sync worker with `SyncHandle::spawn_on_demand` — it runs
only demanded jobs, so the test decides when GitLab is read. Wrappers:
`dormant_handlers()` (NoCredentials; also used by `service.rs` tests),
`unreachable_handlers()`, `connected_handlers(&fake)`. Keep the `TempDir` alive.

**Seeding the store directly**: `seed(&h, &rows)` upserts mirror rows,
`seed_view(&h, name, keys, fetched_at)` writes a view (assigned or recent),
`mark_synced(&h, &[Job::…])` makes the cold-cache guards treat data as warm.
Composite seeds: `seed_assigned_issues`, `seed_assigned_mrs`, `seed_recent_issues`,
`seed_corpus`.

**Driving a varlink method**: helpers wrap `AsyncCall` — `assigned_issues`,
`assigned_mrs`, `list_issues`, `run_search`, `history`, `post_time`, `close`,
`unassign`, `clear_cache`,
`run_record_open`, `create_issue` / `create_issue_with` (they return the call, for
either of the next two; `created_json(iid, title, assignees)` is a row to
`serve_create`); `reply::<T_Reply>(&mut call)` parses success,
`reply_error(&mut call)` returns the error name (`NOT_AUTHENTICATED`,
`GITLAB_ERROR` constants).

**Reconnect-signal assertions**: demotion woke the supervisor →
`tokio::time::timeout(Duration::from_millis(200), h.reconnect_signal.notified())`
is `Ok`; "did not fire" → `.is_err()`.

## Sync tests (`src/sync/*`)

- `schedule.rs` is pure (`now` passed in): proptests on jitter bands and backoff.
- `store.rs`/`planner.rs`: a tempdir `SyncStore`, rows committed through
  `store.begin()` … `commit()`.
- `jobs.rs`: call `fetch(job, ctx)` with a `FetchCtx` built around the fake, apply
  the `Staged` result to a commit, assert on the store and `fake.calls_to(..)`.
- `engine.rs`: `start(state)` / `start_on(store, state)` spawn the real scheduled
  worker with `instant_config()` (no job gap, no startup spread); drive it with
  `refresh_now`, `clear`, and `eventually`. `start_with_avatars(store, &dir, state)`
  keeps the avatar files in `dir`, for tests that restart or look at them.
  `start_on_demand(store, state)` runs only demanded jobs, for tests of what reaches
  the store beside the fetches (`land_issue`); `seed_views` writes their views.
  A failed job retries only after its backoff (a minute and up): to drive several
  attempts, queue that many `fail_next`s, wait for the scheduled one
  (`first_failure`) and demand the rest with `rerun(&env, job)`, which runs it ahead
  of its backoff and waits for the outcome. `serve_tracked_project` plans project 7's
  jobs. To assert on what the worker *logs* (a warning or only a debug line),
  `let (logs, _guard) = Logs::capture();` before starting it, then
  `logs.count("WARN", &job.key())` / `logs.said(level, key, "message")`: a
  `#[tokio::test]` runs the worker on its own thread, where the capture applies.

## Dry-run tests (`src/daemon.rs`, `src/demo.rs`)

- `demo.rs` tests call `DemoGitlab` (the dry run's in-memory GitLab, not the shared
  fake) directly: listings, writes, refusals.
- `daemon.rs` tests start whole dry runs and drive them over their socket with the
  generated `VarlinkClient`: `DryRun::start(&Scratch::create()?)`; `synced(&sync,
  limit)` waits until every planned job ran (what the dry run announces its socket
  after); `until(what, async || -> Option<T>)` polls an async probe (5 s);
  `run.stop()` asserts `keychain.refused() == 0` and the socket's removal. A test
  that must see the temp dir go drops the runtime first, as `main` does.
- Handler and bench scaffolding holds `Keychain::disabled()`: no test reaches the OS
  keychain.

## Queue and reconnect tests

- `queue.rs` spawns `worker(..)` directly on a channel; `run_worker_one_task[_with]`
  runs one task to completion, `instant_retry_config()` zeroes the backoff, and
  `calls(&fake, "close")` counts writes.
- `reconnect.rs` injects the connect attempt as a closure into
  `reconnect_loop(session, config, || async { .. })` returning `Attempt::*`.
- `rotate.rs` injects clock, keychain and connect through its `Env` trait:
  `rig(info, now)` builds a supervisor around a `FakeEnv` with zeroed waits, and a
  test calls `engage()` once per evaluation. The sync worker's keychain look on a
  401 is the `KeychainProbe` closure passed to `SyncHandle::spawn`.

## Timing rules

- **No `tokio::time::pause()` anywhere.** Backoff and scheduling use
  `SystemTime::now()`, which a paused tokio clock does not affect. Tests use short
  real sleeps and `tokio::time::timeout` with small budgets (≤ a few hundred ms), or
  `eventually`.
- Prefer defeating delays via config (`instant_config`, `instant_retry_config`) over
  sleeping through them; keep schedule logic in pure functions so it is testable
  without a clock.
