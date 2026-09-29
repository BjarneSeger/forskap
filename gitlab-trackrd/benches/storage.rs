//! The storage substrate: raw `KvStore` scans (fjall iteration + per-entry
//! JSON decode), the sync store's table scans and batch upserts, the
//! timelog window scan, and the full-run reconcile.
//!
//! No `RetryQueue` benches on purpose: its stores fsync after every mutation
//! (`open_durable`), so a bench would measure the disk, not the code.

mod support;

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use gitlab_trackrd::db::KvStore;
use gitlab_trackrd::sync::model::{Issue, Timelog};
use gitlab_trackrd::sync::store::RowScope;

use support::{dormant_env, issue, now_secs, put, seed_history, seed_search_corpus, timelog};

const SIZES: [u64; 3] = [1_000, 10_000, 50_000];

fn kvstore_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("kvstore_scan");
    group.sample_size(30);
    let now = now_secs();
    for n in SIZES {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        let store: KvStore<u64, Timelog> = KvStore::open(&db, "bench_scan").unwrap();
        for i in 0..n {
            store.put(i, &timelog(i, now)).unwrap();
        }
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(store.scan(|_, v| Ok(v)).unwrap()));
        });
    }
    group.finish();
}

fn table(c: &mut Criterion) {
    let mut group = c.benchmark_group("table");
    group.sample_size(30);
    for n in SIZES {
        let env = dormant_env();
        let rows: Vec<Issue> = (0..n).map(issue).collect();
        put(&env, &rows);
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new("scan", n), &n, |b, _| {
            b.iter(|| black_box(env.store().issues.scan(RowScope::All).unwrap()));
        });
        // Steady state: a sync run re-upserting rows already stored.
        group.bench_with_input(BenchmarkId::new("upsert", n), &n, |b, _| {
            b.iter(|| put(&env, black_box(&rows)));
        });
    }
    group.finish();
}

fn timelogs(c: &mut Criterion) {
    let mut group = c.benchmark_group("timelogs");
    group.sample_size(30);
    let now = now_secs();
    for n in SIZES {
        let env = dormant_env();
        seed_history(&env, n, now);
        // spent_at is uniform over 30 days; a 9-day cutoff selects ~30%.
        let cutoff = now - 9 * 86_400;
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new("since", n), &n, |b, _| {
            b.iter(|| black_box(env.store().timelogs.scan(RowScope::Since(cutoff)).unwrap()));
        });
    }
    for n in [1_000u64, 10_000] {
        let env = dormant_env();
        seed_history(&env, n, now);
        // The oldest ~10% band; each iteration clears it, setup reseeds only
        // that band.
        let (band_min, band_max) = (now - 30 * 86_400, now - 27 * 86_400);
        let band: Vec<_> = (0..n)
            .map(|i| timelog(i, now))
            .filter(|t| t.spent_at >= band_min && t.spent_at < band_max)
            .collect();
        group.throughput(Throughput::Elements(band.len() as u64));
        group.bench_with_input(BenchmarkId::new("clear_band", n), &n, |b, _| {
            b.iter_batched(
                || put(&env, &band),
                |()| {
                    let mut c = env.store().begin();
                    let removed = c
                        .remove_where::<Timelog>(RowScope::Since(band_min), |k| k.0 >= band_max)
                        .unwrap();
                    c.commit().unwrap();
                    black_box(removed)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn reconcile(c: &mut Criterion) {
    let mut group = c.benchmark_group("reconcile");
    group.sample_size(30);
    for n in [1_000u64, 10_000] {
        let env = dormant_env();
        seed_search_corpus(&env, n);
        // A full `all` run: keep 90%, reseed the stale 10% tail each
        // iteration so the timed scan always sees n rows.
        let rows: Vec<Issue> = (0..n).map(issue).collect();
        let keep_n = (n * 9 / 10) as usize;
        let (kept, stale) = rows.split_at(keep_n);
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new("all", n), &n, |b, _| {
            b.iter_batched(
                || put(&env, stale),
                |()| {
                    let mut c = env.store().begin();
                    let removed = c.reconcile(RowScope::All, kept).unwrap();
                    c.commit().unwrap();
                    black_box(removed)
                },
                BatchSize::PerIteration,
            );
        });
        // One tracked project's full run: a key-range scan, not the table.
        let project: Vec<Issue> = rows.iter().filter(|i| i.project_id == 1).cloned().collect();
        group.bench_with_input(BenchmarkId::new("project", n), &n, |b, _| {
            b.iter(|| {
                let mut c = env.store().begin();
                let removed = c.reconcile(RowScope::Prefix(1), &project).unwrap();
                c.commit().unwrap();
                black_box(removed)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, kvstore_scan, table, timelogs, reconcile);
criterion_main!(benches);
