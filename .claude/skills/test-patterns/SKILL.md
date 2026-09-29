---
name: test-patterns
description: The established mock and test conventions in this workspace (the shared FakeGitlab, handler and sync-engine scaffolding, varlink call driving, timing rules) — read before writing or extending daemon tests so new tests reuse the existing helpers.
---

# Test patterns in gitlab-trackrd

All tests are inline `#[cfg(test)]` modules — no `tests/` dirs. There is **one**
GitLab mock; reuse the helpers below instead of inventing new scaffolding.

## The shared fake (`src/testing.rs`)

`FakeGitlab` implements `GitlabApi` for every suite (handlers, sync, queue,
reconnect).
- **Reads** are routed by `Listing::path()`: `serve(path, rows)` sets the JSON rows
  every call to that path returns (empty by default), `serve_next(path, rows)`
  answers only the next call (e.g. a page walk that differs from its follow-up
  delta); `fail_next(path, FakeErr)` queues one-shot failures; `gate(path)` holds the next call until the returned
  `Notify` fires (`fake.gated` signals that the call started). `serve_timelogs(..)`
  covers the GraphQL timelog read.
- **Writes** succeed unless `fail_next_write(FakeErr)` queued a failure; `writes()`
  logs `(op, kind, project_id, iid)`.
  `gate_writes()` holds every write until the returned gate's `release()` (sticky);
  a held write is already in `writes()`, so `writes().len()` counts started attempts.
- Assert on traffic with `calls()`, `calls_to(path)`, `timelog_calls()`,
  `read_calls()` — e.g. "a read never touches GitLab" is `read_calls() == 0`.
- `FakeErr::{Transient, Throttled(status), Rejected, Unauthorized}` build the
  matching `Error` (`Unauthorized` is a 401: a dead token).
- JSON builders `issue_json`, `event_json`; `eventually(what, || cond)` polls up to 2 s.

## Handler tests (`src/handlers/tests.rs`)

**Scaffolding**: `handlers_with(state) -> (Handlers, TempDir)` opens a real fjall DB
in a tempdir and spawns the sync worker with `SyncHandle::spawn_on_demand` — it runs
only demanded jobs, so the test decides when GitLab is read. Wrappers:
`dormant_handlers()` (NoCredentials; also used by `service.rs` tests),
`unreachable_handlers()`, `connected_handlers(&fake)`. Keep the `TempDir` alive.

**Seeding the store directly**: `seed(&h, &rows)` upserts mirror rows,
`seed_view(&h, name, keys, fetched_at)` writes an assigned view,
`mark_synced(&h, &[Job::…])` makes the cold-cache guards treat data as warm.
Composite seeds: `seed_assigned_issues`, `seed_assigned_mrs`, `seed_corpus`.

**Driving a varlink method**: helpers wrap `AsyncCall` — `assigned_issues`,
`assigned_mrs`, `run_search`, `history`, `post_time`, `close`, `clear_cache`,
`run_record_open`; `reply::<T_Reply>(&mut call)` parses success,
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
  `refresh_now`, `clear`, and `eventually`.

## Queue and reconnect tests

- `queue.rs` spawns `worker(..)` directly on a channel; `run_worker_one_task[_with]`
  runs one task to completion, `instant_retry_config()` zeroes the backoff, and
  `calls(&fake, "close")` counts writes.
- `reconnect.rs` injects the connect attempt as a closure into
  `reconnect_loop(session, config, || async { .. })` returning `Attempt::*`.

## Timing rules

- **No `tokio::time::pause()` anywhere.** Backoff and scheduling use
  `SystemTime::now()`, which a paused tokio clock does not affect. Tests use short
  real sleeps and `tokio::time::timeout` with small budgets (≤ a few hundred ms), or
  `eventually`.
- Prefer defeating delays via config (`instant_config`, `instant_retry_config`) over
  sleeping through them; keep schedule logic in pure functions so it is testable
  without a clock.
