//! When sync jobs run: pure functions of persisted job state, cadence and
//! `now`.
//!
//! Every due time is jittered by a hash of the job key and the run it follows,
//! so jobs sharing an interval drift apart instead of hitting GitLab in
//! lockstep, and a restart recomputes the same schedule instead of re-rolling
//! it.

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

/// Per-job bookkeeping, persisted so a restart inside an interval costs
/// GitLab nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobState {
    /// Start of the last successful run; 0 means never.
    pub last_ok: u64,
    /// Start of the last successful full run.
    pub last_full: u64,
    /// Consecutive failures, driving the backoff.
    pub failures: u32,
    /// The job must not run before this (backoff); 0 means no backoff.
    pub retry_at: u64,
    /// What the last success synced under (resource schema, windows); a
    /// mismatch forces the next run to be full.
    pub fingerprint: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// Seconds between runs.
    pub every: u64,
    /// Seconds between full runs; `None` when every run is full.
    pub full_every: Option<u64>,
}

/// Backoff cap after a 5xx.
pub const SERVER_BACKOFF_CAP: u64 = 3600;
/// Backoff cap after a permanent rejection (403 on a lost project, …).
pub const REJECTED_BACKOFF_CAP: u64 = 6 * 3600;
/// How long a job rests after GitLab rejected a feature the instance may
/// simply not have (epics without Premium).
pub const UNAVAILABLE_REST_SECS: u64 = 24 * 3600;
/// Cap on the worker-wide pause after a 429, `Retry-After` included.
pub const RATE_LIMIT_PAUSE_CAP: u64 = 3600;
const BACKOFF_BASE: u64 = 60;

/// When the job is next due. Never-run jobs and jobs whose fingerprint
/// changed are due at once (backoff permitting).
pub fn due_at(key: &str, state: &JobState, cadence: Cadence, fingerprint: u64, jitter: f64) -> u64 {
    if state.last_ok == 0 || state.fingerprint != fingerprint {
        return state.retry_at;
    }
    let next = state
        .last_ok
        .saturating_add(jittered(cadence.every, key, state.last_ok, jitter));
    next.max(state.retry_at)
}

/// Whether a run starting at `now` must be full rather than a delta.
pub fn run_is_full(
    key: &str,
    state: &JobState,
    cadence: Cadence,
    fingerprint: u64,
    jitter: f64,
    now: u64,
) -> bool {
    let Some(full_every) = cadence.full_every else {
        return true;
    };
    state.last_full == 0
        || state.fingerprint != fingerprint
        || now
            >= state
                .last_full
                .saturating_add(jittered(full_every, key, !state.last_full, jitter))
}

/// `secs` scaled by a factor in `[1 - jitter, 1 + jitter]`, deterministic in
/// `key` and `seed`. `u64::MAX` means never and stays that.
pub fn jittered(secs: u64, key: &str, seed: u64, jitter: f64) -> u64 {
    if secs == u64::MAX {
        return secs;
    }
    let offset = (2.0 * unit(key, seed) - 1.0) * jitter.clamp(0.0, 0.5);
    (secs as f64 * (1.0 + offset)).round() as u64
}

/// A deterministic value in `[0, 1)` from `key` and `seed`.
pub fn unit(key: &str, seed: u64) -> f64 {
    (xxh3_64_with_seed(key.as_bytes(), seed) >> 11) as f64 / (1u64 << 53) as f64
}

/// Seconds to wait before retrying after `failures` consecutive failures:
/// 1 min doubling, capped.
pub fn backoff(failures: u32, cap: u64) -> u64 {
    let doublings = failures.saturating_sub(1).min(32);
    BACKOFF_BASE.saturating_mul(1u64 << doublings).min(cap)
}

/// The priority class a due job competes in: its own, but past class 1 a
/// job overdue by more than its interval ranks with the events, so a
/// saturated corpus can't starve it.
pub fn aged(priority: u8, overdue: u64, every: u64) -> u8 {
    if priority > 1 && overdue > every {
        1
    } else {
        priority
    }
}

/// How long after boot an overdue background job waits, spread over
/// `window` so a restart doesn't fire every job at once.
pub fn startup_offset(key: &str, window: u64) -> u64 {
    (unit(key, 0) * window as f64) as u64
}

/// Hash a job's sync parameters into a [`JobState::fingerprint`].
pub fn fingerprint(parts: &[u64]) -> u64 {
    let bytes: Vec<u8> = parts.iter().flat_map(|p| p.to_le_bytes()).collect();
    xxh3_64(&bytes)
}

/// Hash a text a job syncs by (an avatar's URL), as a [`fingerprint`] part.
pub fn text_hash(text: &str) -> u64 {
    xxh3_64(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HOURLY: Cadence = Cadence {
        every: 3600,
        full_every: Some(86_400),
    };

    fn synced(at: u64, fp: u64) -> JobState {
        JobState {
            last_ok: at,
            last_full: at,
            fingerprint: fp,
            ..Default::default()
        }
    }

    proptest! {
        #[test]
        fn due_time_stays_inside_the_jitter_band(
            key in "[a-z/0-9]{1,20}",
            last_ok in 1u64..2_000_000_000,
            every in 60u64..1_000_000,
            jitter in 0.0f64..0.5,
        ) {
            let cadence = Cadence { every, full_every: None };
            let gap = due_at(&key, &synced(last_ok, 1), cadence, 1, jitter) - last_ok;
            let lo = (every as f64 * (1.0 - jitter)).floor() as u64;
            let hi = (every as f64 * (1.0 + jitter)).ceil() as u64;
            prop_assert!((lo..=hi).contains(&gap), "{gap} outside {lo}..={hi}");
        }

        #[test]
        fn backoff_is_monotonic_and_capped(failures in 0u32..100, cap in 60u64..100_000) {
            prop_assert!(backoff(failures, cap) <= cap);
            prop_assert!(backoff(failures + 1, cap) >= backoff(failures, cap));
        }

        #[test]
        fn startup_offset_stays_inside_its_window(key in ".{0,30}", window in 1u64..10_000) {
            prop_assert!(startup_offset(&key, window) < window);
        }
    }

    #[test]
    fn jitter_spreads_jobs_sharing_an_interval() {
        let dues: std::collections::HashSet<u64> = (0..20)
            .map(|p| {
                due_at(
                    &format!("project/{p}/issues"),
                    &synced(1_000, 1),
                    HOURLY,
                    1,
                    0.15,
                )
            })
            .collect();
        assert!(
            dues.len() > 15,
            "20 jobs landed on {} distinct times",
            dues.len()
        );
    }

    #[test]
    fn never_run_or_changed_jobs_are_due_now_and_full() {
        let fresh = JobState::default();
        assert_eq!(due_at("k", &fresh, HOURLY, 1, 0.15), 0);
        assert!(run_is_full("k", &fresh, HOURLY, 1, 0.15, 10));

        let stale_schema = synced(1_000, 1);
        assert_eq!(due_at("k", &stale_schema, HOURLY, 2, 0.15), 0);
        assert!(run_is_full("k", &stale_schema, HOURLY, 2, 0.15, 1_001));
    }

    #[test]
    fn delta_runs_until_the_full_interval_passes() {
        let s = synced(1_000, 1);
        assert!(!run_is_full("k", &s, HOURLY, 1, 0.0, 1_000 + 3_600));
        assert!(run_is_full("k", &s, HOURLY, 1, 0.0, 1_000 + 86_400));
        let full_only = Cadence {
            every: 3600,
            full_every: None,
        };
        assert!(run_is_full("k", &s, full_only, 1, 0.0, 1_001));
    }

    proptest! {
        /// Events use `u64::MAX` for "never full again", avatars for "never
        /// again": jitter must not turn that into a time.
        #[test]
        fn a_never_cadence_stays_never(
            key in "[a-z]{1,8}",
            at in 1u64..4_000_000_000,
            jitter in 0.0f64..0.5,
        ) {
            let never = Cadence { every: u64::MAX, full_every: Some(u64::MAX) };
            prop_assert!(!run_is_full(&key, &synced(at, 1), never, 1, jitter, at + 1));
            prop_assert_eq!(due_at(&key, &synced(at, 1), never, 1, jitter), u64::MAX);
        }
    }

    #[test]
    fn a_job_overdue_past_its_interval_ranks_with_the_events() {
        assert_eq!(aged(3, 86_400, 86_400), 3);
        assert_eq!(aged(3, 86_401, 86_400), 1);
        assert_eq!(aged(2, 1_801, 1_800), 1);
        assert_eq!(aged(0, u64::MAX, 60), 0, "never demoted");
        assert_eq!(aged(1, u64::MAX, 60), 1);
    }

    /// Fingerprints are stored and name the avatar files: a hash that
    /// changed would resync everything once more.
    #[test]
    fn the_hash_is_pinned() {
        assert_eq!(text_hash(""), 0x2d06_8005_38d3_94c2, "the XXH3 spec's");
        assert_eq!(fingerprint(&[1, 2]), 571_542_372_673_154_031);
        assert_eq!(unit("k", 7).to_bits(), 4_595_725_774_607_267_048);
    }

    #[test]
    fn backoff_holds_the_job_back() {
        let s = JobState {
            retry_at: 50_000,
            ..synced(1_000, 1)
        };
        assert_eq!(due_at("k", &s, HOURLY, 1, 0.0), 50_000);
        assert_eq!(backoff(1, 3600), 60);
        assert_eq!(backoff(3, 3600), 240);
        assert_eq!(backoff(99, 3600), 3600);
    }
}
