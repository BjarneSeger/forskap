//! The remaining pure store-reader handlers: the assigned-MR view and the
//! history merge.

mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use gitlab_trackr_api::{
    AsyncCall, Call_GetAssignedMergeRequests, Call_GetHistory, VarlinkInterface,
};

use support::{dormant_env, now_secs, seed_history, seed_mr_corpus};

const SIZES: [u64; 3] = [1_000, 10_000, 50_000];

fn assigned_mrs(c: &mut Criterion) {
    let mut group = c.benchmark_group("assigned_mrs");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));
    for n in SIZES {
        let env = dormant_env();
        // The view lists a realistic handful; the corpus size shows the read
        // no longer scales with it.
        seed_mr_corpus(&env, n, 20);
        group.throughput(Throughput::Elements(n));
        for (variant, groups) in [
            ("all", None),
            // Adds the per-MR namespace_of + in_group pass.
            ("group_filter", Some(vec!["team".to_string()])),
        ] {
            group.bench_with_input(BenchmarkId::new(variant, n), &n, |b, _| {
                b.to_async(&env.rt).iter(|| {
                    let groups = groups.clone();
                    let h = &env.h;
                    async move {
                        let mut call = AsyncCall::default();
                        h.get_assigned_merge_requests(
                            &mut call as &mut dyn Call_GetAssignedMergeRequests,
                            groups,
                        )
                        .await
                        .unwrap();
                        black_box(call.take_reply())
                    }
                });
            });
        }
    }
    group.finish();
}

fn get_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_history");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));
    let now = now_secs();
    for n in SIZES {
        let env = dormant_env();
        seed_history(&env, n, now);
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.to_async(&env.rt).iter(|| {
                let h = &env.h;
                async move {
                    let mut call = AsyncCall::default();
                    h.get_history(&mut call as &mut dyn Call_GetHistory, Some(7))
                        .await
                        .unwrap();
                    black_box(call.take_reply())
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, assigned_mrs, get_history);
criterion_main!(benches);
