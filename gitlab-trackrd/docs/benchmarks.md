# Benchmarks

Criterion benches for the daemon's **local** operations — the pure
cache-reader handlers and the fjall storage layer under them. GitLab-bound
paths are deliberately not benchmarked (network latency and instance load
drown out any signal), and neither is the `RetryQueue` (its stores fsync
after every mutation, so a bench would measure the disk, not the code).

## Targets

| Target | Covers |
|---|---|
| `search` | The `Search` handler over a seeded corpus (1k/10k/50k entries): needle-miss scans, ~1% hit scans with the per-hit board lookup, all-kinds scans, `#iid` reference queries. Plus the pure matchers `text_matches`/`parse_iid_query`. |
| `storage` | Raw `KvStore` full scans (fjall iteration + per-entry JSON decode), the sync store's table scan and batch upsert, the timelog window scan and band clear, and full-run reconciles (whole table and one project's key range). |
| `handlers` | `GetAssignedMergeRequests` (view → point reads, with and without group filter) and `GetHistory` (timelog window scan). |

Handler benches drive the real varlink surface: the generated `AsyncCall`
driver against a dormant `Handlers` on a temp-dir fjall database. Dormant is
deliberate — every benched path is a pure cache read/write, and dormancy
proves no network access is possible. Shared scaffolding lives in
`benches/support/mod.rs`; corpus generators shuffle timestamps so sorts never
see pre-ordered input.

## Workflow

```sh
cargo bench -p gitlab-trackrd                          # everything (minutes)
cargo bench -p gitlab-trackrd --bench search           # one target
cargo bench -p gitlab-trackrd -- --quick               # fast smoke pass
cargo test  -p gitlab-trackrd --benches                # run each bench once (correctness only)
```

Regression checking is baseline-driven:

```sh
cargo bench -p gitlab-trackrd -- --save-baseline main  # on the base commit
# ...apply your change...
cargo bench -p gitlab-trackrd -- --baseline main       # compare; criterion flags regressions
```

HTML reports land under `target/criterion/`.

## Measurement hygiene

Numbers are only comparable **same machine, same session**: run both sides of
a comparison back to back, on AC power, with the machine otherwise quiet. The
hot paths are allocation-heavy and fjall has block-cache warm-up, so expect a
few percent of noise — treat single-digit deltas as suspect, re-run before
believing them.

## Non-goals

- No CI timing job — shared runners are far too noisy; benches are a local
  tool. (A `cargo bench --no-run` compile check is the most CI should ever do.)
- No network-path or queue benchmarks, per above.
