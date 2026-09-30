//! Daemon configuration, loaded from a TOML file.
//!
//! Layered with [`confique`]: the user file at
//! `$XDG_CONFIG_HOME/forskapd/config.toml` wins, then the
//! package-provided default at [`SYSTEM_CONFIG`], then the `#[config(default)]`
//! values baked into the structs below. Every field is optional in the file.
//!
//! The config is grouped into nested sections, one per concern, so the TOML
//! reads as `[server]` / `[refresh]` / `[history]` / `[queue]` tables instead
//! of a flat list of keys. Each [`Config`] field is a sub-struct owned by the
//! module that consumes it, alongside the helpers that turn raw values into the
//! runtime types those modules expect.
//!
//! Credentials are deliberately **not** here — the GitLab host/token live in the
//! OS keychain and are set through the varlink API (`forskap auth login`), never via this
//! file or the environment.
//!
//! Run the `gen-config-template` binary to print an annotated TOML template with
//! the current defaults and doc comments inline (used to generate the shipped
//! default config).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use confique::Config as ConfiqueConfig;
use tracing::warn;

/// Shared, swappable config read by every consumer at the moment of use, so a
/// hot reload (see `reload`) takes effect without a restart.
///
/// Reads must stay momentary — extract the `Copy` value you need in a single
/// statement so the guard drops before any `.await`; never hold it across one.
pub type SharedConfig = Arc<RwLock<Config>>;

/// Default install path for the package-provided config, layered under the
/// user's own file.
const SYSTEM_CONFIG: &str = "/usr/share/forskapd/config.toml";

/// `forskapd` configuration.
///
/// Each section is a nested sub-struct; missing keys fall back to the
/// `#[config(default = ...)]` value on the corresponding field.
#[derive(Debug, ConfiqueConfig)]
pub struct Config {
    /// Varlink server / listening socket.
    #[config(nested)]
    pub server: ServerConfig,

    /// Sync cadences for the foreground (`quick`) and bulk (`slow`) data.
    #[config(nested)]
    pub refresh: RefreshConfig,

    /// Timelog history retention.
    #[config(nested)]
    pub history: HistoryConfig,

    /// Retry-queue backoff and lifetime tuning.
    #[config(nested)]
    pub queue: QueueConfig,

    /// Background auto-reconnect backoff (re-establishing a dormant session).
    #[config(nested)]
    pub reconnect: ReconnectConfig,

    /// Search-corpus population and per-project sync cadence.
    #[config(nested)]
    pub search: SearchConfig,

    /// Open-statistics retention (what `Search` ranks by).
    #[config(nested)]
    pub usage: UsageConfig,

    /// Pacing of the background sync, so its requests trickle out instead of
    /// hitting GitLab in bursts.
    #[config(nested)]
    pub sync: SyncConfig,

    /// Automatic rotation of the GitLab token before it expires.
    #[config(nested)]
    pub auth: AuthConfig,
}

/// Varlink server settings (see `server.rs`).
#[derive(Debug, ConfiqueConfig)]
pub struct ServerConfig {
    /// Unix socket the daemon listens on. If unset, defaults to
    /// `$XDG_RUNTIME_DIR/forskapd.socket` (then `/tmp/...` as a last
    /// resort). Ignored under systemd socket activation.
    pub socket: Option<String>,
}

impl ServerConfig {
    /// The configured socket, or the `$XDG_RUNTIME_DIR` -> `/tmp` fallback chain
    /// when unset.
    pub fn resolved_socket(&self) -> String {
        if let Some(socket) = &self.socket {
            return socket.clone();
        }
        dirs::runtime_dir()
            .map(|d| d.join("forskapd.socket").to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp/forskapd.socket".to_string())
    }
}

/// Sync cadences, consumed by the `sync` jobs. Work is split by cost and
/// volatility:
///
/// * `quick` — fast-changing and cheap to fetch: the assigned issue/MR lists
///   and your most recent timelogs. Synced frequently.
/// * `slow` — the large, slow-moving rest: the full timelog history, the
///   board columns and the memberships. Synced rarely.
#[derive(Debug, ConfiqueConfig)]
pub struct RefreshConfig {
    /// Quick tier: assigned issues/MRs and recent timelogs.
    #[config(nested)]
    pub quick: QuickRefreshConfig,

    /// Slow tier: the full timelog history and board columns.
    #[config(nested)]
    pub slow: SlowRefreshConfig,
}

/// The `quick` refresh tier — see [`RefreshConfig`]. Its own struct (rather than
/// a shared tier type) because confique bakes `#[config(default)]` per type, and
/// the two tiers ship different defaults.
#[derive(Debug, ConfiqueConfig)]
pub struct QuickRefreshConfig {
    /// Seconds between quick syncs (assigned issues/MRs and the most recent
    /// timelogs). Five minutes by default.
    #[config(default = 300)]
    pub interval_secs: u64,

    /// How far back, in hours, the quick timelog sync reaches. Timelogs
    /// deleted in GitLab inside this window disappear at the next quick sync;
    /// older ones at the next slow sync. (24h by default.)
    #[config(default = 24)]
    pub window_hours: u64,
}

impl QuickRefreshConfig {
    /// Cadence between refreshes.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    /// Timelog look-back span.
    pub fn window(&self) -> Duration {
        hours(self.window_hours)
    }
}

/// The `slow` refresh tier — see [`RefreshConfig`]. Distinct from
/// [`QuickRefreshConfig`] only in its defaults (see that type's note).
#[derive(Debug, ConfiqueConfig)]
pub struct SlowRefreshConfig {
    /// Seconds between slow syncs of the full timelog history (the whole
    /// `history.retention_hours`), the board columns, and your project and
    /// group memberships. Once a day by default.
    #[config(default = 86400)]
    pub interval_secs: u64,
}

impl SlowRefreshConfig {
    /// Cadence between refreshes.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

/// Timelog history retention, consumed by the timelog sync jobs.
#[derive(Debug, ConfiqueConfig)]
pub struct HistoryConfig {
    /// Total timelog history to keep, in hours: synced in full on the slow
    /// cadence, and anything older is pruned. (90 days by default.)
    #[config(default = 2160)]
    pub retention_hours: u64,
}

impl HistoryConfig {
    /// Retention horizon: the oldest timelog kept on disk.
    pub fn retention(&self) -> Duration {
        hours(self.retention_hours)
    }
}

/// Open-statistics retention, consumed by the `RecordOpen` handler via
/// `usage.rs`.
#[derive(Debug, ConfiqueConfig)]
pub struct UsageConfig {
    /// How long an issue/MR keeps its "frequently opened" ranking after its
    /// last open, in hours; older entries are dropped on the next recorded
    /// open. (90 days by default.)
    #[config(default = 2160)]
    pub retention_hours: u64,
}

impl UsageConfig {
    /// Retention horizon: the oldest last-open kept.
    pub fn retention(&self) -> Duration {
        hours(self.retention_hours)
    }
}

/// Retry-queue timing, consumed by `queue.rs`. The `*_secs` fields come from the
/// TOML; the accessors hand `queue.rs` the [`Duration`]s its worker uses.
#[derive(Debug, Clone, Copy, ConfiqueConfig)]
pub struct QueueConfig {
    /// Retry-queue exponential backoff: initial delay, in seconds.
    #[config(default = 1)]
    pub base_delay_secs: u64,

    /// Retry-queue exponential backoff: maximum delay, in seconds. (30 min.)
    #[config(default = 1800)]
    pub max_delay_secs: u64,

    /// How long, in seconds, a queued task keeps retrying before it is
    /// dead-lettered. (7 days by default.)
    #[config(default = 604800)]
    pub max_lifetime_secs: u64,

    /// How long, in seconds, the retry worker sleeps while the daemon is
    /// dormant (no GitLab session) before checking again.
    #[config(default = 30)]
    pub session_wait_secs: u64,

    /// Most queued writes the retry worker sends to GitLab at once. Writes to
    /// the same issue or MR always go one at a time, in the order they were
    /// queued. (4 by default; at least 1.)
    #[config(default = 4)]
    pub max_in_flight: u64,
}

impl QueueConfig {
    /// Initial exponential-backoff delay.
    pub fn base_delay(&self) -> Duration {
        Duration::from_secs(self.base_delay_secs)
    }

    /// Exponential-backoff cap.
    pub fn max_delay(&self) -> Duration {
        Duration::from_secs(self.max_delay_secs)
    }

    /// How long a task keeps retrying before it is dead-lettered.
    pub fn max_lifetime(&self) -> Duration {
        Duration::from_secs(self.max_lifetime_secs)
    }

    /// How long the worker sleeps while dormant (no session) before retrying.
    pub fn session_wait(&self) -> Duration {
        Duration::from_secs(self.session_wait_secs)
    }

    /// Most attempts in flight at once; never below one.
    pub fn max_in_flight(&self) -> usize {
        usize::try_from(self.max_in_flight)
            .unwrap_or(usize::MAX)
            .max(1)
    }
}

/// Background auto-reconnect tuning, consumed by `reconnect.rs`.
///
/// When the daemon is dormant because GitLab was *unreachable* (the stored
/// credentials are known-good), a background task retries the connection with
/// exponential backoff using these values. Unlike the queue there is nothing to
/// dead-letter, so retries continue indefinitely — the delay is merely capped —
/// until the connection succeeds or the session state changes. The cap is kept
/// short (a minute) so recovery is noticed promptly, versus the queue's 30-min
/// cap tuned for long-lived write retries.
#[derive(Debug, Clone, Copy, ConfiqueConfig)]
pub struct ReconnectConfig {
    /// Whether the daemon auto-reconnects after an unreachable-GitLab dormancy
    /// (whether GitLab was down at boot or the connection dropped mid-run). When
    /// `false`, recovery is manual (`forskap auth login` or a restart) — the session still
    /// honestly reports `unreachable`, it just isn't retried. Re-read on every
    /// retry, so disabling it via a hot config reload stops an in-flight reconnect
    /// on the next iteration. The supervisor task is long-lived (parked between
    /// outages) and re-checks the dormant slot on a periodic tick (≤ `max_delay`),
    /// so a `false`→`true` reload is picked up at the next tick — no disconnect,
    /// `forskap auth login`, or restart required.
    #[config(default = true)]
    pub enabled: bool,

    /// Auto-reconnect exponential backoff: initial delay, in seconds.
    #[config(default = 2)]
    pub base_delay_secs: u64,

    /// Auto-reconnect exponential backoff: maximum delay between attempts, in
    /// seconds. (1 min.)
    #[config(default = 60)]
    pub max_delay_secs: u64,
}

impl ReconnectConfig {
    /// Initial exponential-backoff delay.
    pub fn base_delay(&self) -> Duration {
        Duration::from_secs(self.base_delay_secs)
    }

    /// Exponential-backoff cap.
    pub fn max_delay(&self) -> Duration {
        Duration::from_secs(self.max_delay_secs)
    }
}

/// Search-corpus sync tuning, consumed by the `sync` planner and jobs.
///
/// The corpus holds issues, merge requests, projects, and groups. Each
/// project's issues and MRs are synced incrementally (`updated_after` deltas)
/// on the partial cadence and fully resynced — which also reconciles
/// deletions — on the full cadence. Job states persist across restarts, so
/// restarting the daemon inside the partial interval does not re-poll GitLab.
#[derive(Debug, ConfiqueConfig)]
pub struct SearchConfig {
    /// What the search corpus holds for issues and merge requests. The
    /// default `"tracked"` covers the member projects you are active in:
    /// assigned issues/MRs, your pushes, issues, MRs and comments, and your
    /// timelogs (see `tracked_retention_hours`). Activity in a project you
    /// aren't a member of (an upstream you contribute to) only keeps your
    /// assigned items there. `"member"` covers every project you
    /// are a member of, `"all"` everything your token can see (GitLab
    /// `scope=all`; huge on large instances, and rejected by gitlab.com).
    /// Projects and groups themselves are always membership-scoped. `"auto"`
    /// is accepted as an alias of `"tracked"`.
    #[config(default = "tracked")]
    pub population: SearchPopulation,

    /// Minimum seconds between incremental search-cache syncs. (30 min by
    /// default.)
    #[config(default = 1800)]
    pub partial_interval_secs: u64,

    /// Seconds between full search-cache resyncs, which also remove deleted
    /// items. (7 days by default.)
    #[config(default = 604800)]
    pub full_interval_secs: u64,

    /// How long, in hours, your activity in a project keeps it in the
    /// `"tracked"` population after the last time you touched it (an
    /// assignment, a push, an issue, MR or comment, a timelog). Your
    /// contribution events are kept for the same time, so this is also how
    /// far back `forskap activity` reaches. (90 days by default.)
    #[config(default = 2160)]
    pub tracked_retention_hours: u64,

    /// Most issues and most merge requests kept per corpus project: the most
    /// recently updated ones. Bounds the sync of very large projects.
    /// (1000 by default, at least 100.)
    #[config(default = 1000)]
    pub max_items_per_project: u64,
}

/// Which issues/MRs the search cache is populated with — see
/// [`SearchConfig::population`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchPopulation {
    /// Everything the token can see (`scope=all` on the global endpoints).
    All,
    /// Only projects the user is a member of (one fetch per project).
    Member,
    /// Only projects with recent activity of the user's (see
    /// [`SearchConfig::tracked_retention_hours`]).
    #[serde(alias = "auto")]
    Tracked,
}

impl SearchConfig {
    /// Minimum gap between incremental syncs; also the sync loop's tick.
    pub fn partial_interval(&self) -> Duration {
        Duration::from_secs(self.partial_interval_secs)
    }

    /// Gap between full resyncs.
    pub fn full_interval(&self) -> Duration {
        Duration::from_secs(self.full_interval_secs)
    }

    /// How long activity keeps a project tracked.
    pub fn tracked_retention(&self) -> Duration {
        hours(self.tracked_retention_hours)
    }
}

/// Background sync pacing, consumed by `sync::engine`.
#[derive(Debug, ConfiqueConfig)]
pub struct SyncConfig {
    /// Random spread applied to every sync interval, as a fraction: 0.15 runs
    /// a 5-minute job anywhere from 4¼ to 5¾ minutes after the last one, so
    /// jobs sharing an interval don't hit GitLab together. (0 to 0.5; 0.15 by
    /// default.)
    #[config(default = 0.15)]
    pub jitter: f64,

    /// Pause between two sync jobs, in milliseconds (jittered like the
    /// intervals), so a backlog of due jobs trickles out. (250 ms by default.)
    #[config(default = 250)]
    pub job_gap_ms: u64,

    /// Seconds over which jobs already overdue at startup are spread out. The
    /// assigned issue/MR lists and recent timelogs always run at once. (1 min
    /// by default.)
    #[config(default = 60)]
    pub startup_spread_secs: u64,
}

impl SyncConfig {
    pub fn job_gap(&self) -> Duration {
        Duration::from_millis(self.job_gap_ms)
    }
}

/// Token rotation, consumed by `rotate.rs`.
#[derive(Debug, Clone, Copy, ConfiqueConfig)]
pub struct AuthConfig {
    /// Which tokens the daemon replaces by a fresh one shortly before they
    /// expire. Rotating revokes the token you pasted, which breaks every
    /// other tool using it, so the default `"scoped"` only rotates tokens
    /// created for it: those with the `self_rotate` scope. `"always"` also
    /// rotates tokens that can rotate through the `api` scope, `"never"`
    /// turns rotation off. A token without an expiry date is never rotated.
    #[config(default = "scoped")]
    pub rotate: RotatePolicy,

    /// How many days before it expires a token is rotated. A token living
    /// less than three times as long is rotated once a third of its lifetime
    /// is left. (7 days by default, at least 1.)
    #[config(default = 7)]
    pub rotate_before_days: u64,
}

/// Which tokens are rotated — see [`AuthConfig::rotate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotatePolicy {
    /// Only tokens carrying the `self_rotate` scope.
    Scoped,
    /// Every token that is able to rotate itself.
    Always,
    Never,
}

impl AuthConfig {
    /// How long before its expiry a token is rotated.
    pub fn rotate_before(&self) -> Duration {
        hours(self.rotate_before_days.saturating_mul(24))
    }
}

/// `h` hours, saturating: `Duration::from_hours` panics past `u64::MAX`
/// seconds, and a huge window must mean "everything", not a dead task.
fn hours(h: u64) -> Duration {
    Duration::from_secs(h.saturating_mul(3600))
}

/// `$XDG_CONFIG_HOME/forskapd/config.toml` (falls back to `./`).
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("forskapd/config.toml")
}

/// Double `current`, capped at `max`, saturating instead of panicking on
/// overflow — the exponential-backoff step shared by the retry queue and the
/// auto-reconnect loop. `Duration * u32` panics on overflow, so an absurd
/// configured cap would crash the loop without the `checked_mul` guard.
pub fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(max).min(max)
}

/// Clamp a section's exponential-backoff `*_secs` pair into a sane range and
/// warn on any change. A `base` of 0 would busy-spin the retry loop (a zero
/// sleep between attempts); a `max` below `base` would collapse the schedule
/// back to that zero after the first step. Values are otherwise left as-is —
/// `next_backoff` handles the overflow ceiling.
fn normalize_backoff(base_secs: &mut u64, max_secs: &mut u64, section: &str) {
    if *base_secs == 0 {
        warn!(
            section,
            "base_delay_secs of 0 would busy-spin the backoff loop; flooring to 1"
        );
        *base_secs = 1;
    }
    if *max_secs < *base_secs {
        warn!(
            section,
            base = *base_secs,
            max = *max_secs,
            "max_delay_secs is below base_delay_secs; raising it to the base"
        );
        *max_secs = *base_secs;
    }
}

/// Floor the queue's in-flight bound: 0 would never send a queued write.
fn normalize_queue(queue: &mut QueueConfig) {
    if queue.max_in_flight == 0 {
        warn!("queue.max_in_flight of 0 would never send a queued write; flooring to 1");
        queue.max_in_flight = 1;
    }
}

/// Floor the rotation lead: with 0 days a token would expire unrotated.
fn normalize_auth(auth: &mut AuthConfig) {
    if auth.rotate_before_days == 0 {
        warn!("auth.rotate_before_days of 0 would let the token expire; flooring to 1");
        auth.rotate_before_days = 1;
    }
}

/// Load the layered config: user file → system default → built-in defaults.
///
/// Missing files are treated as empty layers; parse errors propagate. Backoff
/// delays are normalized (see [`normalize_backoff`]) so a hand-edited config
/// can't busy-spin or overflow the retry loops.
pub fn load() -> Result<Config, confique::Error> {
    let mut config = Config::builder()
        .file(config_path())
        .file(Path::new(SYSTEM_CONFIG))
        .load()?;
    normalize_backoff(
        &mut config.reconnect.base_delay_secs,
        &mut config.reconnect.max_delay_secs,
        "reconnect",
    );
    normalize_backoff(
        &mut config.queue.base_delay_secs,
        &mut config.queue.max_delay_secs,
        "queue",
    );
    normalize_queue(&mut config.queue);
    normalize_search(&mut config.search);
    normalize_refresh(&mut config.refresh);
    normalize_sync(&mut config.sync);
    normalize_auth(&mut config.auth);
    Ok(config)
}

/// Floor both refresh cadences: an interval of 0 would poll GitLab
/// back-to-back.
fn normalize_refresh(refresh: &mut RefreshConfig) {
    for (tier, secs) in [
        ("quick", &mut refresh.quick.interval_secs),
        ("slow", &mut refresh.slow.interval_secs),
    ] {
        if *secs < 60 {
            warn!(
                tier,
                configured = *secs,
                "refresh interval_secs below 60 would hammer GitLab; flooring to 60"
            );
            *secs = 60;
        }
    }
}

/// Keep the jitter a fraction that can't push an interval to zero or double
/// it, and the job gap short enough not to stall the queue.
fn normalize_sync(sync: &mut SyncConfig) {
    if !(0.0..=0.5).contains(&sync.jitter) {
        let clamped = if sync.jitter.is_nan() {
            0.0
        } else {
            sync.jitter.clamp(0.0, 0.5)
        };
        warn!(
            configured = sync.jitter,
            clamped, "sync.jitter must be between 0 and 0.5; clamping"
        );
        sync.jitter = clamped;
    }
    if sync.job_gap_ms > 60_000 {
        warn!(
            configured = sync.job_gap_ms,
            "sync.job_gap_ms above 60000 would stall the sync queue; capping to 60000"
        );
        sync.job_gap_ms = 60_000;
    }
}

/// Clamp the corpus sync cadences and the tracked retention into a sane
/// range and warn on any change.
/// A partial interval of 0 would busy-spin the sync loop; a full interval
/// below the partial one would make every sync a full resync.
fn normalize_search(search: &mut SearchConfig) {
    if search.partial_interval_secs < 60 {
        warn!(
            configured = search.partial_interval_secs,
            "search.partial_interval_secs below 60 would hammer GitLab; flooring to 60"
        );
        search.partial_interval_secs = 60;
    }
    if search.tracked_retention_hours < 24 {
        warn!(
            configured = search.tracked_retention_hours,
            "search.tracked_retention_hours below 24 would drop projects almost at once; flooring to 24"
        );
        search.tracked_retention_hours = 24;
    }
    if search.max_items_per_project < 100 {
        warn!(
            configured = search.max_items_per_project,
            "search.max_items_per_project below 100 would hide most of a project; flooring to 100"
        );
        search.max_items_per_project = 100;
    }
    if search.full_interval_secs < search.partial_interval_secs {
        warn!(
            partial = search.partial_interval_secs,
            full = search.full_interval_secs,
            "search.full_interval_secs is below partial_interval_secs; raising it to the partial interval"
        );
        search.full_interval_secs = search.partial_interval_secs;
    }
}

/// Load once and wrap for sharing across the daemon's tasks. Used at startup; a
/// parse error propagates so the caller can fail fast (no prior config exists).
pub fn load_shared() -> Result<SharedConfig, confique::Error> {
    Ok(Arc::new(RwLock::new(load()?)))
}

/// Re-run [`load`] and swap the contents in place.
///
/// On a parse error the existing config is left untouched and the error is
/// returned, so a malformed mid-edit save never disturbs the running daemon —
/// the caller logs and keeps serving the last-good values.
pub fn reload(shared: &SharedConfig) -> Result<(), confique::Error> {
    let fresh = load()?;
    *shared.write().unwrap() = fresh;
    Ok(())
}

/// A fully-defaulted config (no file layers), for tests and benches that need
/// a [`SharedConfig`] without touching the real XDG path.
pub fn defaults() -> Config {
    Config::builder()
        .load()
        .expect("built-in defaults are valid")
}

/// Render an annotated TOML template (current defaults + doc comments inline).
///
/// Used by the `gen-config-template` binary via the library target; the daemon
/// binary itself never calls it.
#[allow(dead_code)]
pub fn template() -> String {
    confique::toml::template::<Config>(confique::toml::FormatOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reconnect_defaults_are_short_and_enabled() {
        let c = defaults();
        assert!(c.reconnect.enabled);
        assert_eq!(c.reconnect.base_delay(), Duration::from_secs(2));
        assert_eq!(c.reconnect.max_delay(), Duration::from_secs(60));
    }

    proptest! {
        #[test]
        fn next_backoff_doubles_saturating_and_never_exceeds_the_cap(
            current_secs in any::<u64>(),
            max_secs in any::<u64>(),
        ) {
            let current = Duration::from_secs(current_secs);
            let max = Duration::from_secs(max_secs);
            let next = next_backoff(current, max);
            prop_assert!(next <= max);
            match current.checked_mul(2) {
                Some(doubled) => prop_assert_eq!(next, doubled.min(max)),
                // Doubling would overflow `Duration`: saturate, don't panic.
                None => prop_assert_eq!(next, max),
            }
        }
    }

    #[test]
    fn search_defaults() {
        let c = defaults();
        assert_eq!(c.search.population, SearchPopulation::Tracked);
        assert_eq!(c.search.partial_interval(), Duration::from_secs(1800));
        assert_eq!(c.search.full_interval(), Duration::from_secs(604800));
    }

    #[test]
    fn sync_defaults() {
        let c = defaults();
        assert_eq!(c.sync.jitter, 0.15);
        assert_eq!(c.sync.job_gap(), Duration::from_millis(250));
        assert_eq!(c.sync.startup_spread_secs, 60);
        assert_eq!(c.search.tracked_retention(), Duration::from_hours(2160));
        assert_eq!(c.search.max_items_per_project, 1000);
    }

    #[test]
    fn auth_defaults_rotate_only_scoped_tokens() {
        let c = defaults();
        assert_eq!(c.auth.rotate, RotatePolicy::Scoped);
        assert_eq!(c.auth.rotate_before(), Duration::from_hours(7 * 24));
    }

    #[test]
    fn rotate_policy_parses_its_three_names() {
        for (name, policy) in [
            ("\"scoped\"", RotatePolicy::Scoped),
            ("\"always\"", RotatePolicy::Always),
            ("\"never\"", RotatePolicy::Never),
        ] {
            let parsed: RotatePolicy = serde_json::from_str(name).unwrap();
            assert_eq!(parsed, policy);
        }
    }

    #[test]
    fn huge_hour_windows_saturate_instead_of_panicking() {
        let mut c = defaults();
        c.search.tracked_retention_hours = u64::MAX;
        assert_eq!(c.search.tracked_retention(), Duration::from_secs(u64::MAX));
    }

    #[test]
    fn search_population_parses_tracked_and_its_auto_alias() {
        for name in ["\"tracked\"", "\"auto\""] {
            let p: SearchPopulation = serde_json::from_str(name).unwrap();
            assert_eq!(p, SearchPopulation::Tracked);
        }
    }

    proptest! {
        #[test]
        fn normalize_sync_keeps_jitter_a_sane_fraction(jitter in any::<f64>(), gap in any::<u64>()) {
            let mut s = defaults().sync;
            s.jitter = jitter;
            s.job_gap_ms = gap;
            normalize_sync(&mut s);
            prop_assert!((0.0..=0.5).contains(&s.jitter));
            prop_assert!(s.job_gap_ms <= 60_000);
            if (0.0..=0.5).contains(&jitter) {
                prop_assert_eq!(s.jitter, jitter, "in-range values are left untouched");
            }
        }

        #[test]
        fn normalize_refresh_floors_both_intervals(quick in any::<u64>(), slow in any::<u64>()) {
            let mut r = defaults().refresh;
            r.quick.interval_secs = quick;
            r.slow.interval_secs = slow;
            normalize_refresh(&mut r);
            prop_assert_eq!(r.quick.interval_secs, quick.max(60));
            prop_assert_eq!(r.slow.interval_secs, slow.max(60));
        }

        #[test]
        fn normalize_backoff_floors_the_base_and_orders_the_pair(
            base_in in any::<u64>(),
            max_in in any::<u64>(),
        ) {
            let (mut base, mut max) = (base_in, max_in);
            normalize_backoff(&mut base, &mut max, "test");
            prop_assert!(base >= 1);
            prop_assert!(max >= base);
            if base_in >= 1 && max_in >= base_in {
                prop_assert_eq!(
                    (base, max),
                    (base_in, max_in),
                    "in-range values are left untouched"
                );
            }
        }

        #[test]
        fn normalize_auth_floors_the_rotation_lead(days in any::<u64>()) {
            let mut a = defaults().auth;
            a.rotate_before_days = days;
            normalize_auth(&mut a);
            prop_assert_eq!(a.rotate_before_days, days.max(1));
            // Saturates instead of panicking.
            let _ = a.rotate_before();
        }

        #[test]
        fn normalize_queue_floors_max_in_flight(n in any::<u64>()) {
            let mut q = defaults().queue;
            q.max_in_flight = n;
            normalize_queue(&mut q);
            prop_assert_eq!(q.max_in_flight, n.max(1));
            prop_assert!(q.max_in_flight() >= 1);
        }

        #[test]
        fn normalize_search_floors_the_partial_and_orders_the_pair(
            partial_in in any::<u64>(),
            full_in in any::<u64>(),
        ) {
            let mut s = defaults().search;
            s.partial_interval_secs = partial_in;
            s.full_interval_secs = full_in;
            normalize_search(&mut s);
            prop_assert!(s.partial_interval_secs >= 60);
            prop_assert!(s.full_interval_secs >= s.partial_interval_secs);
            if partial_in >= 60 && full_in >= partial_in {
                prop_assert_eq!(
                    (s.partial_interval_secs, s.full_interval_secs),
                    (partial_in, full_in),
                    "in-range values are left untouched"
                );
            }
        }
    }
}
