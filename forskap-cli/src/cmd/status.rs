//! `forskap status` — is anything not working?
//!
//! One look at what can break without the user noticing: the daemon away or
//! stuck, its GitLab session down, sync jobs failing or hanging, writes that
//! failed for good. Each check ends in a [`Level`]; an `error` anywhere makes
//! the exit status non-zero, so a script or a shell prompt can ask too.
//!
//! Asking the daemon ([`gather`]) and judging its answers ([`evaluate`], pure
//! over the answers and a `now`) are kept apart, so the judging is tested
//! without a daemon.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
#[cfg(test)]
use forskap_api::QueuedWrite;
use forskap_api::admin::{
    self, GetSyncJobs_Reply, SyncJob, SyncJobStatus, VarlinkClientInterface as _,
};
use forskap_api::{
    API_VERSION, ErrorKind as ApiErrorKind, FailedTask, GetQueue_Reply, NotAuthReason,
    VarlinkClient, VarlinkClientInterface, WhoAmI_Reply,
};
use serde::Serialize;

use crate::cli::{OutputFormat, WatchArgs};
use crate::cmd::auth::status::{EXPIRY_WARN_SECS, expiry, token_line};
use crate::cmd::sync::jobs::{self, failure, kind, pause, span};
use crate::friendly::DaemonError;
use crate::{client, config, friendly, output, style, watch};

/// The CLI's own version. The daemon's may differ: what has to fit is the
/// interface ([`forskap_api::compatible`]).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the daemon gets for each answer. It answers from memory and its
/// local store within milliseconds; one that takes this long is stuck.
const ANSWER_SECS: u64 = 10;

/// A job running this long hangs. The slowest fetch seen, the whole
/// member-projects walk over a bad link, took about four minutes; this is
/// well beyond it, so a slow network alone does not get there.
const HANG_SECS: i64 = 15 * 60;

/// A job due this long is overdue. The assigned lists are due every five
/// minutes, so a sync worker that works starts something well within this;
/// it only warns when nothing at all started meanwhile (see [`sync`]).
const OVERDUE_SECS: i64 = 30 * 60;

/// How many failing jobs are named; `forskap sync jobs` has them all.
const FAILING_NAMED: usize = 3;

pub async fn run(format: OutputFormat, watch: WatchArgs) -> Result<()> {
    let every = watch::interval(watch, format)?;
    let socket = client::socket(&config::load()?)?;
    let socket = socket.as_str();
    match every {
        Some(every) => {
            watch::run(
                every,
                move || async move { Ok(render(&check(socket).await)) },
            )
            .await
        }
        None => {
            let report = check(socket).await;
            output::emit(format, &report, |report| out!("{}", render(report)))?;
            outcome(&report)
        }
    }
}

async fn check(socket: &str) -> Report {
    evaluate(&gather(socket).await, Utc::now())
}

/// `forskap status` found an error and printed it: `main` exits non-zero
/// without another word.
#[derive(Debug, thiserror::Error)]
#[error("a status check found an error")]
pub struct Unhealthy;

/// What the exit status says: only an error is a failure, so a warning
/// (a sync job GitLab keeps failing) doesn't fail a prompt.
fn outcome(report: &Report) -> Result<()> {
    match report.level {
        Level::Error => Err(Unhealthy.into()),
        Level::Ok | Level::Skipped | Level::Warning => Ok(()),
    }
}

/// What the daemon answered, call by call.
struct Answers {
    socket: String,
    /// Whether the socket took the connection, else why not.
    connected: Result<(), String>,
    info: Answer<Info>,
    /// The interface version it speaks; `None` from a daemon older than
    /// `GetStatus`.
    interface: Answer<Option<String>>,
    who: Answer<Login>,
    jobs: Answer<GetSyncJobs_Reply>,
    /// The writes still waiting; `None` from a daemon too old to list them.
    queued: Answer<Option<GetQueue_Reply>>,
    failures: Answer<Vec<FailedTask>>,
}

enum Answer<T> {
    Got(T),
    /// The call failed: why.
    Failed(String),
    /// No answer within [`ANSWER_SECS`].
    TimedOut,
    /// Not asked: the daemon was out of reach, or stopped answering.
    Unasked,
}

impl<T> Answer<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Answer<U> {
        match self {
            Answer::Got(value) => Answer::Got(f(value)),
            Answer::Failed(why) => Answer::Failed(why),
            Answer::TimedOut => Answer::TimedOut,
            Answer::Unasked => Answer::Unasked,
        }
    }
}

/// The daemon's `org.varlink.service.GetInfo`.
struct Info {
    product: String,
    version: String,
}

enum Login {
    Connected(WhoAmI_Reply),
    Dormant {
        reason: Option<NotAuthReason>,
        detail: Option<String>,
    },
}

/// Ask the daemon at `socket` everything the checks judge.
///
/// One call at a time on one connection. After a timeout the next answer on
/// it could be the late one, so nothing more is asked.
async fn gather(socket: &str) -> Answers {
    let mut answers = Answers {
        socket: socket.to_string(),
        connected: Ok(()),
        info: Answer::Unasked,
        interface: Answer::Unasked,
        who: Answer::Unasked,
        jobs: Answer::Unasked,
        queued: Answer::Unasked,
        failures: Answer::Unasked,
    };
    let conn = match ask(client::open(socket)).await {
        Some(Ok(conn)) => conn,
        Some(Err(e)) => {
            answers.connected = Err(unreachable_because(&e));
            return answers;
        }
        None => {
            answers.connected = Err(format!("No connection within {ANSWER_SECS}s."));
            return answers;
        }
    };
    answers.info = match ask(client::service_info(&conn)).await {
        Some(Ok(info)) => Answer::Got(Info {
            product: info.product.into_owned(),
            version: info.version.into_owned(),
        }),
        Some(Err(e)) => Answer::Failed(varlink_words(e.kind())),
        None => Answer::TimedOut,
    };
    if matches!(answers.info, Answer::TimedOut) {
        return answers;
    }
    let api = VarlinkClient::new(Arc::clone(&conn));
    let admin = admin::VarlinkClient::new(conn);
    answers.interface = match ask(api.get_status().call()).await {
        Some(Err(e)) if friendly::is_method_not_found(&e) => Answer::Got(None),
        other => answer(other).map(|status| Some(status.api_version)),
    };
    if matches!(answers.interface, Answer::TimedOut) {
        return answers;
    }
    answers.who = match ask(api.who_am_i().call()).await {
        Some(Err(e)) => match e.kind() {
            ApiErrorKind::NotAuthenticated(args) => Answer::Got(Login::Dormant {
                reason: args.as_ref().and_then(|a| a.reason.clone()),
                detail: args.as_ref().and_then(|a| a.detail.clone()),
            }),
            _ => Answer::Failed(describe(&e)),
        },
        other => answer(other).map(Login::Connected),
    };
    if matches!(answers.who, Answer::TimedOut) {
        return answers;
    }
    answers.jobs = answer(ask(admin.get_sync_jobs().call()).await);
    if matches!(answers.jobs, Answer::TimedOut) {
        return answers;
    }
    answers.queued = match ask(api.get_queue().call()).await {
        Some(Err(e)) if friendly::is_method_not_found(&e) => Answer::Got(None),
        other => answer(other).map(Some),
    };
    if matches!(answers.queued, Answer::TimedOut) {
        return answers;
    }
    answers.failures = answer(ask(api.get_failures().call()).await).map(|r| r.failures);
    answers
}

/// `call` under the timeout: `None` when it ran out.
async fn ask<T, E>(call: impl Future<Output = Result<T, E>>) -> Option<Result<T, E>> {
    tokio::time::timeout(Duration::from_secs(ANSWER_SECS), call)
        .await
        .ok()
}

fn answer<T>(asked: Option<Result<T, impl DaemonError>>) -> Answer<T> {
    match asked {
        Some(Ok(reply)) => Answer::Got(reply),
        Some(Err(e)) => Answer::Failed(describe(&e)),
        None => Answer::TimedOut,
    }
}

/// A failed call in a few words, rather than the generated client's dump.
fn describe(e: &impl DaemonError) -> String {
    match (e.message(), e.varlink_kind()) {
        (Some(message), _) => message,
        (None, Some(kind)) => varlink_words(kind),
        (None, None) => e.to_string(),
    }
}

fn varlink_words(kind: &varlink::ErrorKind) -> String {
    match kind {
        varlink::ErrorKind::VarlinkErrorReply(reply) => {
            let name = reply.error.as_deref().unwrap_or("an error");
            match &reply.parameters {
                Some(parameters) => format!("{name} {parameters}"),
                None => name.to_string(),
            }
        }
        varlink::ErrorKind::Io(io) => std::io::Error::from(*io).to_string(),
        // A reply that doesn't decode: a field this CLI needs is missing.
        varlink::ErrorKind::SerdeJsonSer(_) | varlink::ErrorKind::SerdeJsonDe(_) => {
            "Its answer does not fit this forskap: the daemon and the CLI are from different \
             builds."
                .to_string()
        }
        kind => kind.to_string(),
    }
}

/// Why connecting failed, in words that say what to look at.
fn unreachable_because(e: &varlink::Error) -> String {
    use std::io::ErrorKind as Io;
    match e.kind() {
        varlink::ErrorKind::Io(Io::NotFound) => "No socket exists there.".to_string(),
        varlink::ErrorKind::Io(Io::ConnectionRefused) => {
            "The socket exists, but nothing listens on it.".to_string()
        }
        // What a unix socket path longer than the ~100 bytes it may have gets.
        varlink::ErrorKind::Io(Io::InvalidInput) => {
            "That path can't be a unix socket: it is too long.".to_string()
        }
        varlink::ErrorKind::InvalidAddress => {
            "That is no varlink address (`unix:PATH` or `tcp:HOST:PORT`).".to_string()
        }
        kind => format!("Connecting failed: {}.", varlink_words(kind)),
    }
}

/// How a check came out, from best to worst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Ok,
    /// Not judged: an earlier check makes it moot.
    Skipped,
    Warning,
    Error,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Skipped => "skipped",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// Everything `forskap status` found, as printed by `--output json`.
#[derive(Serialize)]
struct Report {
    /// The worst level of any check.
    level: Level,
    checks: Checks,
}

#[derive(Serialize)]
struct Checks {
    daemon: Check<DaemonFacts>,
    session: Check<SessionFacts>,
    sync: Check<SyncFacts>,
    queue: Check<QueueFacts>,
}

/// One check: its level, a line saying why, lines saying more. The facts
/// behind it sit next to them, where the check got that far.
#[derive(Serialize)]
struct Check<F> {
    name: &'static str,
    level: Level,
    summary: String,
    details: Vec<String>,
    #[serde(flatten)]
    facts: Option<F>,
}

impl<F> Check<F> {
    fn new(name: &'static str, level: Level, summary: impl Into<String>) -> Self {
        Check {
            name,
            level,
            summary: summary.into(),
            details: Vec::new(),
            facts: None,
        }
    }

    fn details(self, details: Vec<String>) -> Self {
        Check { details, ..self }
    }

    fn facts(self, facts: F) -> Self {
        Check {
            facts: Some(facts),
            ..self
        }
    }
}

#[derive(Serialize)]
struct DaemonFacts {
    socket: String,
    /// What the daemon says it is; absent when it didn't say.
    version: Option<String>,
    cli_version: &'static str,
    /// The interface version it speaks; absent when it didn't say.
    api_version: Option<String>,
    cli_api_version: &'static str,
}

#[derive(Serialize)]
struct SessionFacts {
    connected: bool,
    host: Option<String>,
    username: Option<String>,
    user_id: Option<i64>,
    token_expires_at: Option<i64>,
    token_rotates: Option<bool>,
    /// Why there is no session.
    reason: Option<NotAuthReason>,
    detail: Option<String>,
}

#[derive(Serialize)]
struct SyncFacts {
    jobs: JobCounts,
    paused_until: Option<i64>,
    /// Jobs whose last runs failed, or that back off after failing.
    failing: Vec<String>,
    /// Jobs running for longer than a fetch takes.
    hanging: Vec<String>,
    /// Due jobs left waiting for a while.
    overdue: Vec<String>,
    /// Instance-wide lists with no successful run yet.
    never_synced: Vec<String>,
    /// Jobs GitLab refuses for good (a feature switched off in a project,
    /// epics without GitLab Premium); the daemon asks again once a day.
    unavailable: Vec<String>,
}

#[derive(Default, Serialize)]
struct JobCounts {
    total: usize,
    running: usize,
    demanded: usize,
    due: usize,
    waiting: usize,
    backing_off: usize,
}

#[derive(Serialize)]
struct QueueFacts {
    failed_writes: usize,
    /// The writes still waiting to be sent; absent from a daemon too old to
    /// say.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_writes: Option<usize>,
}

fn evaluate(a: &Answers, now: DateTime<Utc>) -> Report {
    let checks = Checks {
        daemon: daemon(a),
        session: session(a, now),
        sync: sync(a, link(&a.who), now.timestamp()),
        queue: queue(a, link(&a.who), now.timestamp()),
    };
    let level = checks.rows().iter().map(|row| row.level).max();
    Report {
        level: level.unwrap_or(Level::Ok),
        checks,
    }
}

/// The reply a check judges, or the check telling why there is none.
fn reply<'a, T, F>(
    name: &'static str,
    method: &str,
    answer: &'a Answer<T>,
    a: &Answers,
) -> Result<&'a T, Check<F>> {
    match answer {
        Answer::Got(reply) => Ok(reply),
        Answer::Failed(why) => {
            Err(Check::new(name, Level::Error, format!("{method} failed"))
                .details(vec![why.clone()]))
        }
        Answer::TimedOut => Err(Check::new(
            name,
            Level::Error,
            format!("no answer to {method} within {ANSWER_SECS}s"),
        )
        .details(vec![format!(
            "The daemon stopped answering; restart it: {}.",
            restart_command()
        )])),
        Answer::Unasked if a.connected.is_err() => Err(Check::new(
            name,
            Level::Skipped,
            "the daemon is not reachable",
        )),
        Answer::Unasked => Err(Check::new(
            name,
            Level::Skipped,
            "not asked: the daemon stopped answering",
        )),
    }
}

/// How to get the daemon running, the way it is installed here.
fn start_hint() -> String {
    let how = if cfg!(target_os = "macos") {
        "`brew services start forskap`"
    } else {
        "`systemctl --user enable --now forskapd.service`"
    };
    format!("Start it with {how}.")
}

fn restart_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "`brew services restart forskap`"
    } else {
        "`systemctl --user restart forskapd.service`"
    }
}

/// Judged by the interface it speaks: a daemon of another package version
/// that speaks this forskap's interface is fine.
fn daemon(a: &Answers) -> Check<DaemonFacts> {
    let socket = &a.socket;
    let facts = DaemonFacts {
        socket: socket.clone(),
        version: match &a.info {
            Answer::Got(info) => Some(info.version.clone()),
            _ => None,
        },
        cli_version: VERSION,
        api_version: match &a.interface {
            Answer::Got(api) => api.clone(),
            _ => None,
        },
        cli_api_version: API_VERSION,
    };
    if let Err(why) = &a.connected {
        return Check::new("daemon", Level::Error, format!("not reachable on {socket}"))
            .details(vec![why.clone(), start_hint()])
            .facts(facts);
    }
    let restart = || {
        vec![format!(
            "After an upgrade the daemon runs the old version until it is restarted: {}.",
            restart_command()
        )]
    };
    let check = match (&a.info, &a.interface) {
        (Answer::TimedOut, _) | (_, Answer::TimedOut) => Check::new(
            "daemon",
            Level::Error,
            format!("no answer on {socket} within {ANSWER_SECS}s"),
        )
        .details(vec![
            "It takes the connection but does not answer: it is stuck.".to_string(),
            format!("Restart it: {}.", restart_command()),
        ]),
        (Answer::Failed(why), _) => Check::new(
            "daemon",
            Level::Warning,
            format!("answers on {socket}, but not with its version"),
        )
        .details(vec![format!("GetInfo failed: {why}")]),
        (Answer::Unasked, _) | (_, Answer::Unasked) => {
            Check::new("daemon", Level::Skipped, format!("on {socket}"))
        }
        (Answer::Got(Info { product, version }), Answer::Got(api)) => {
            let daemon = format!("{product} {version} on {socket}");
            match api {
                Some(api) if forskap_api::compatible(api) => {
                    Check::new("daemon", Level::Ok, daemon)
                }
                Some(api) => Check::new(
                    "daemon",
                    Level::Warning,
                    format!("{daemon} speaks interface {api}, forskap {API_VERSION}"),
                )
                .details(restart()),
                None => Check::new(
                    "daemon",
                    Level::Warning,
                    format!("{daemon} speaks an interface older than forskap's {API_VERSION}"),
                )
                .details(restart()),
            }
        }
        (Answer::Got(Info { product, version }), Answer::Failed(why)) => Check::new(
            "daemon",
            Level::Warning,
            format!("{product} {version} on {socket}, but not saying its interface"),
        )
        .details(vec![format!("GetStatus failed: {why}")]),
    };
    check.facts(facts)
}

fn session(a: &Answers, now: DateTime<Utc>) -> Check<SessionFacts> {
    match reply("session", "WhoAmI", &a.who, a) {
        Ok(Login::Connected(me)) => connected(me, now),
        Ok(Login::Dormant { reason, detail }) => dormant(reason.as_ref(), detail.as_deref()),
        Err(check) => check,
    }
}

fn connected(me: &WhoAmI_Reply, now: DateTime<Utc>) -> Check<SessionFacts> {
    let who = format!("@{} on {}", me.username, me.host);
    let expires = me
        .token_expires_at
        .and_then(|secs| DateTime::from_timestamp(secs, 0));
    let check = match expires {
        Some(at) if at <= now => Check::new(
            "session",
            Level::Error,
            format!("{who}, but the token {}", expiry(at, now)),
        )
        .details(vec![
            friendly::remedy(Some(&NotAuthReason::token_rejected)).to_string(),
        ]),
        Some(at) if !me.token_rotates && (at - now).num_seconds() < EXPIRY_WARN_SECS => Check::new(
            "session",
            Level::Warning,
            format!("{who}; the token {}", expiry(at, now)),
        )
        .details(vec![
            "Automatic rotation is off: create a new token and run `forskap auth login` \
                 before then."
                .to_string(),
        ]),
        _ => Check::new("session", Level::Ok, who).details(vec![token_line(
            me.token_expires_at,
            me.token_rotates,
            now,
        )]),
    };
    check.facts(SessionFacts {
        connected: true,
        host: Some(me.host.clone()),
        username: Some(me.username.clone()),
        user_id: Some(me.user_id),
        token_expires_at: me.token_expires_at,
        token_rotates: Some(me.token_rotates),
        reason: None,
        detail: None,
    })
}

fn dormant(reason: Option<&NotAuthReason>, detail: Option<&str>) -> Check<SessionFacts> {
    // Only `unreachable` heals by itself; the rest wait for the user.
    let (level, summary) = match reason {
        Some(NotAuthReason::unreachable) => (Level::Warning, "GitLab not reachable"),
        Some(NotAuthReason::no_credentials) => (Level::Error, "not logged in"),
        Some(NotAuthReason::logged_out) => (Level::Error, "logged out"),
        Some(NotAuthReason::token_rejected) => (Level::Error, "GitLab rejected the token"),
        Some(NotAuthReason::keychain_error) => {
            (Level::Error, "can't read the credentials from the keychain")
        }
        None => (Level::Error, "not connected to GitLab"),
    };
    let detail = detail.filter(|d| !d.is_empty());
    let next = match reason {
        Some(NotAuthReason::unreachable) => {
            "The daemon reconnects by itself unless `reconnect.enabled` is off; meanwhile \
             cached reads keep working and writes are queued."
        }
        _ => friendly::remedy(reason),
    };
    let details = detail
        .map(str::to_string)
        .into_iter()
        .chain([next.to_string()]);
    Check::new("session", level, summary)
        .details(details.collect())
        .facts(SessionFacts {
            connected: false,
            host: None,
            username: None,
            user_id: None,
            token_expires_at: None,
            token_rotates: None,
            reason: reason.cloned(),
            detail: detail.map(str::to_string),
        })
}

/// Whether the daemon has a GitLab session, as far as `WhoAmI` told.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Connected,
    Dormant,
    Unknown,
}

fn link(who: &Answer<Login>) -> Link {
    match who {
        Answer::Got(Login::Connected(_)) => Link::Connected,
        Answer::Got(Login::Dormant { .. }) => Link::Dormant,
        _ => Link::Unknown,
    }
}

/// Something a check found; `ok` ones are notes.
struct Finding {
    level: Level,
    line: String,
    details: Vec<String>,
}

impl Finding {
    fn new(level: Level, line: impl Into<String>) -> Self {
        Finding {
            level,
            line: line.into(),
            details: Vec::new(),
        }
    }

    fn details(self, details: Vec<String>) -> Self {
        Finding { details, ..self }
    }
}

/// Whether the job's last runs failed, or it backs off after failing.
fn troubled(job: &SyncJob) -> bool {
    job.status == SyncJobStatus::backing_off || (job.failures > 0 && job.last_error.is_some())
}

/// Whether GitLab refuses the job for good, as the daemon says. A daemon too
/// old to say (no `unavailable`) is judged as it always was: a failing
/// epics listing is one, they need GitLab Premium.
fn unavailable(job: &SyncJob) -> bool {
    match job.unavailable {
        Some(_) => jobs::unavailable(job),
        None => kind(&job.key) == "group/*/epics" && troubled(job),
    }
}

/// The unavailable jobs in a few words: a kind with several under its
/// count, a lone one by its key.
fn refusals(unavailable: &[&SyncJob]) -> String {
    let mut kinds: Vec<(String, &str, usize)> = Vec::new();
    for job in unavailable {
        let kind = kind(&job.key);
        match kinds.iter_mut().find(|k| k.0 == kind) {
            Some(k) => k.2 += 1,
            None => kinds.push((kind, &job.key, 1)),
        }
    }
    let named = kinds.iter().map(|(kind, key, n)| match n {
        1 => key.to_string(),
        n => format!("{kind} ({n})"),
    });
    named.collect::<Vec<_>>().join(", ")
}

fn sync(a: &Answers, link: Link, now: i64) -> Check<SyncFacts> {
    let reply = match reply("sync", "GetSyncJobs", &a.jobs, a) {
        Ok(reply) => reply,
        Err(check) => return check,
    };
    let jobs = &reply.jobs;
    let running = |j: &&SyncJob| j.status == SyncJobStatus::running;

    let unavailable: Vec<&SyncJob> = jobs.iter().filter(|j| unavailable(j)).collect();
    let failing: Vec<&SyncJob> = jobs
        .iter()
        .filter(|j| troubled(j) && !unavailable.iter().any(|u| u.key == j.key))
        .collect();
    let hanging: Vec<(&SyncJob, i64)> = jobs
        .iter()
        .filter(running)
        .filter_map(|j| Some((j, now - j.running_since?)))
        .filter(|&(_, secs)| secs >= HANG_SECS)
        .collect();
    let overdue: Vec<(&SyncJob, i64)> = jobs
        .iter()
        .filter(|j| j.status == SyncJobStatus::due)
        .filter_map(|j| Some((j, now - j.next_due?)))
        .filter(|&(_, late)| late >= OVERDUE_SECS)
        .collect();
    // Something started lately: the worker gets through its backlog.
    let busy = jobs
        .iter()
        .any(|j| running(&j) || j.last_ok.is_some_and(|at| now - at < OVERDUE_SECS));
    let never_synced: Vec<&SyncJob> = jobs
        .iter()
        .filter(|j| j.last_ok.is_none() && kind(&j.key) == j.key)
        .filter(|j| !failing.iter().chain(&unavailable).any(|f| f.key == j.key))
        .collect();
    let paused = pause(reply.paused_until, now);

    let mut findings = Vec::new();
    // A hung fetch holds its slot whatever the session does.
    let free = format!(
        "A fetch this long hangs; restarting the daemon frees it: {}.",
        restart_command()
    );
    match hanging[..] {
        [] => {}
        [(job, secs)] => findings.push(
            Finding::new(
                Level::Error,
                format!("{} running for {}", job.key, span(secs)),
            )
            .details(vec![free]),
        ),
        _ => {
            let line = format!(
                "{} jobs running for over {}",
                hanging.len(),
                span(HANG_SECS)
            );
            let each = hanging
                .iter()
                .map(|(job, secs)| format!("{}: running for {}", job.key, span(*secs)));
            let details = each.chain([free]).collect();
            findings.push(Finding::new(Level::Error, line).details(details));
        }
    }
    // The worker's own word where it gives one: it is what its jobs wait for.
    let on_hold = reply.connected.map_or(link == Link::Dormant, |c| !c);
    if on_hold {
        // Nothing runs without a session: the session check says why.
        findings.push(Finding::new(
            Level::Skipped,
            "on hold while there is no GitLab session",
        ));
    } else {
        let paused_now = paused.is_some();
        if let Some(paused) = paused {
            findings.push(Finding::new(Level::Warning, paused));
        }
        if !failing.is_empty() {
            let line = format!("{} of {} jobs failing", failing.len(), jobs.len());
            let mut named: Vec<String> = failing
                .iter()
                .take(FAILING_NAMED)
                .map(|j| {
                    format!(
                        "{}: {}",
                        j.key,
                        failure(j.failures, j.last_error.as_deref())
                    )
                })
                .collect();
            if failing.len() > FAILING_NAMED {
                named.push(format!(
                    "… and {} more; `forskap sync jobs` lists them all",
                    failing.len() - FAILING_NAMED
                ));
            }
            findings.push(Finding::new(Level::Warning, line).details(named));
        }
        if let Some(&(oldest, late)) = overdue.iter().max_by_key(|&&(_, late)| late) {
            let n = overdue.len();
            // While the worker may run (connected, not paused) and runs
            // nothing, it is not getting to them.
            if link == Link::Connected && !paused_now && !busy {
                let line = format!(
                    "{n} jobs overdue, none started in the last {}",
                    span(OVERDUE_SECS)
                );
                // Where the daemon says what keeps it: a lane or every slot
                // taken, by fetches that don't end.
                let held = jobs::held(oldest).map_or(String::new(), |h| format!(": it runs {h}"));
                findings.push(Finding::new(Level::Warning, line).details(vec![
                    format!("the oldest, {}, by {}{held}", oldest.key, span(late)),
                    format!(
                        "The sync worker is not getting to them; restarting the daemon may \
                         help: {}.",
                        restart_command()
                    ),
                ]));
            } else {
                let catching_up = if busy { "catching up: " } else { "" };
                findings.push(Finding::new(
                    Level::Ok,
                    format!(
                        "{catching_up}{n} jobs overdue, the oldest by {}",
                        span(late)
                    ),
                ));
            }
        }
        if !unavailable.is_empty() {
            let n = unavailable.len();
            let (jobs, them) = if n == 1 {
                ("job", "it")
            } else {
                ("jobs", "them")
            };
            let line = format!(
                "{n} {jobs} unavailable, GitLab refuses {them}: {}",
                refusals(&unavailable)
            );
            let epics = unavailable.iter().any(|j| kind(&j.key) == "group/*/epics");
            let why = epics.then(|| "Epics need GitLab Premium or Ultimate.".to_string());
            findings.push(Finding::new(Level::Ok, line).details(why.into_iter().collect()));
        }
        if !never_synced.is_empty() {
            let keys: Vec<&str> = never_synced.iter().map(|j| j.key.as_str()).collect();
            findings.push(Finding::new(
                Level::Ok,
                format!("not synced yet: {}", keys.join(", ")),
            ));
        }
    }

    let fine = match jobs.len() {
        0 => "no sync jobs planned".to_string(),
        n => format!("{n} jobs, none failing"),
    };
    let keys = |jobs: &[&SyncJob]| jobs.iter().map(|j| j.key.clone()).collect();
    let facts = SyncFacts {
        jobs: JobCounts::of(jobs),
        paused_until: reply.paused_until,
        failing: keys(&failing),
        hanging: hanging.iter().map(|(j, _)| j.key.clone()).collect(),
        overdue: overdue.iter().map(|(j, _)| j.key.clone()).collect(),
        never_synced: keys(&never_synced),
        unavailable: keys(&unavailable),
    };
    conclude("sync", fine, findings).facts(facts)
}

impl JobCounts {
    fn of(jobs: &[SyncJob]) -> Self {
        let mut counts = JobCounts {
            total: jobs.len(),
            ..JobCounts::default()
        };
        for job in jobs {
            *match job.status {
                SyncJobStatus::running => &mut counts.running,
                SyncJobStatus::demanded => &mut counts.demanded,
                SyncJobStatus::due => &mut counts.due,
                SyncJobStatus::waiting => &mut counts.waiting,
                SyncJobStatus::backing_off => &mut counts.backing_off,
            } += 1;
        }
        counts
    }
}

/// A check from its findings: the first of the worst sums it up and the
/// others follow as details. With nothing worse than a note, `fine` does.
fn conclude<F>(name: &'static str, fine: String, findings: Vec<Finding>) -> Check<F> {
    let worst = findings.iter().map(|f| f.level).max().unwrap_or(Level::Ok);
    let top = findings
        .iter()
        .position(|f| f.level == worst)
        .filter(|_| worst > Level::Ok);
    let mut check = Check::new(name, worst, fine);
    let mut rest = Vec::new();
    for (i, finding) in findings.into_iter().enumerate() {
        if Some(i) == top {
            check.summary = finding.line;
            check.details = finding.details;
        } else {
            rest.push(finding.line);
            rest.extend(finding.details);
        }
    }
    check.details.extend(rest);
    check
}

fn queue(a: &Answers, link: Link, now: i64) -> Check<QueueFacts> {
    let failures = match reply("queue", "GetFailures", &a.failures, a) {
        Ok(failures) => failures,
        Err(check) => return check,
    };
    // What waits is a note beside the failures, never a failure of the
    // check: its answer missing leaves the failures to judge.
    let queued = match &a.queued {
        Answer::Got(queued) => queued.as_ref(),
        _ => None,
    };
    let waiting = queued.map_or(0, |q| q.writes.len());
    let facts = QueueFacts {
        failed_writes: failures.len(),
        queued_writes: queued.map(|q| q.writes.len()),
    };
    let writes = |n: usize| if n == 1 { "write" } else { "writes" };
    let mut findings = Vec::new();
    if !failures.is_empty() {
        let n = failures.len();
        findings.push(
            Finding::new(Level::Warning, format!("{n} failed {}", writes(n))).details(vec![
                "GitLab rejected them, or they outlived the retry window: `forskap queue list` \
                 shows them, to retry or dismiss."
                    .to_string(),
            ]),
        );
    }
    if let Some(queued) = queued.filter(|_| waiting > 0) {
        let why = match (link, pause(queued.paused_until, now)) {
            (Link::Dormant, _) => "They are sent once there is a GitLab session.".to_string(),
            (_, Some(paused)) => format!("They are {paused}."),
            _ => "The daemon is sending them; `forskap queue list` shows how far each is."
                .to_string(),
        };
        let line = format!("{waiting} {} queued", writes(waiting));
        findings.push(Finding::new(Level::Ok, line).details(vec![why]));
    }
    conclude("queue", "no failed writes".to_string(), findings).facts(facts)
}

/// One line of the table.
struct Row<'a> {
    name: &'static str,
    level: Level,
    summary: &'a str,
    details: &'a [String],
}

impl<F> Check<F> {
    fn row(&self) -> Row<'_> {
        Row {
            name: self.name,
            level: self.level,
            summary: &self.summary,
            details: &self.details,
        }
    }
}

impl Checks {
    fn rows(&self) -> [Row<'_>; 4] {
        [
            self.daemon.row(),
            self.session.row(),
            self.sync.row(),
            self.queue.row(),
        ]
    }
}

/// A line per check with its details beneath, then the verdict.
fn render(report: &Report) -> String {
    let rows = report.checks.rows();
    let w0 = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    // As wide as any level, not just the ones there: a `--watch` keeps its
    // columns when one changes.
    let levels = [Level::Ok, Level::Skipped, Level::Warning, Level::Error];
    let w1 = levels
        .map(|l| l.word().len())
        .into_iter()
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(w0 + 2 + w1 + 2);
    let mut out = String::new();
    for row in &rows {
        let name = style::strong(row.name);
        let level = style::state(row.level.word());
        out.push_str(&format!("{name:<w0$}  {level:<w1$}  {}\n", row.summary));
        for detail in row.details {
            out.push_str(&format!("{indent}{}\n", style::muted(detail)));
        }
    }
    let (state, tail) = verdict(&rows);
    out.push_str(&format!("\n{}{tail}\n", style::state(state)));
    out
}

/// `healthy`, `healthy: 1 warning`, `unhealthy: 1 error, 3 skipped`, as the
/// state and what follows it.
fn verdict(rows: &[Row]) -> (&'static str, String) {
    let count = |level| rows.iter().filter(|r| r.level == level).count();
    let parts: Vec<String> = [
        (Level::Error, "errors"),
        (Level::Warning, "warnings"),
        (Level::Skipped, "skipped"),
    ]
    .into_iter()
    .filter_map(|(level, many)| match count(level) {
        0 => None,
        1 => Some(format!("1 {}", level.word())),
        n => Some(format!("{n} {many}")),
    })
    .collect();
    let state = if count(Level::Error) > 0 {
        "unhealthy"
    } else {
        "healthy"
    };
    if parts.is_empty() {
        (state, String::new())
    } else {
        (state, format!(": {}", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use forskap_api::Error as ApiError;
    use forskap_api::admin::SyncJobHold;

    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;
    const SOCKET: &str = "unix:/run/user/1000/forskapd.socket";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW, 0).unwrap()
    }

    fn me() -> WhoAmI_Reply {
        WhoAmI_Reply {
            host: "gitlab.example.com".to_string(),
            user_id: 7,
            username: "ada".to_string(),
            token_expires_at: Some(NOW + 90 * DAY),
            token_rotates: true,
            rotation: None,
        }
    }

    fn job(key: &str, status: SyncJobStatus) -> SyncJob {
        SyncJob {
            key: key.to_string(),
            status,
            last_ok: None,
            next_due: None,
            running_since: None,
            failures: 0,
            last_error: None,
            // What a daemon too old to say sends; the same as `false` for
            // anything but the epics.
            unavailable: None,
            full: None,
            fetched: None,
            expected: None,
            held_by: None,
            behind: None,
        }
    }

    /// A job GitLab refuses for good, as a daemon that says so reports it:
    /// resting for most of a day, its failures and error kept.
    fn refused(key: &str, error: &str) -> SyncJob {
        SyncJob {
            next_due: Some(NOW + 80_000),
            failures: 3,
            last_error: Some(error.to_string()),
            unavailable: Some(true),
            ..job(key, SyncJobStatus::waiting)
        }
    }

    /// Synced a minute ago, due again in four.
    fn fresh(key: &str) -> SyncJob {
        SyncJob {
            last_ok: Some(NOW - 60),
            next_due: Some(NOW + 240),
            ..job(key, SyncJobStatus::waiting)
        }
    }

    fn failing(key: &str, failures: i64, error: &str) -> SyncJob {
        SyncJob {
            next_due: Some(NOW + 3600),
            failures,
            last_error: Some(error.to_string()),
            ..job(key, SyncJobStatus::backing_off)
        }
    }

    fn jobs(jobs: Vec<SyncJob>) -> Answer<GetSyncJobs_Reply> {
        Answer::Got(GetSyncJobs_Reply {
            jobs,
            paused_until: None,
            connected: None,
        })
    }

    /// A daemon of 1.3 with nothing waiting.
    fn no_queue() -> GetQueue_Reply {
        GetQueue_Reply {
            writes: Vec::new(),
            paused_until: None,
        }
    }

    fn healthy() -> Answers {
        Answers {
            socket: SOCKET.to_string(),
            connected: Ok(()),
            info: Answer::Got(Info {
                product: "forskapd".to_string(),
                version: VERSION.to_string(),
            }),
            interface: Answer::Got(Some(API_VERSION.to_string())),
            who: Answer::Got(Login::Connected(me())),
            jobs: jobs(vec![
                fresh("assigned/issues"),
                fresh("events"),
                fresh("project/7/issues"),
            ]),
            queued: Answer::Got(Some(no_queue())),
            failures: Answer::Got(Vec::new()),
        }
    }

    fn unreachable(socket: &str) -> Answers {
        Answers {
            socket: socket.to_string(),
            connected: Err("No socket exists there.".to_string()),
            info: Answer::Unasked,
            interface: Answer::Unasked,
            who: Answer::Unasked,
            jobs: Answer::Unasked,
            queued: Answer::Unasked,
            failures: Answer::Unasked,
        }
    }

    fn dormant_with(reason: Option<NotAuthReason>, detail: Option<&str>) -> Answers {
        Answers {
            who: Answer::Got(Login::Dormant {
                reason,
                detail: detail.map(str::to_string),
            }),
            ..healthy()
        }
    }

    fn report(answers: &Answers) -> Report {
        evaluate(answers, now())
    }

    fn levels(report: &Report) -> [Level; 4] {
        report.checks.rows().map(|row| row.level)
    }

    #[test]
    fn everything_healthy_is_ok_and_exits_zero() {
        let report = report(&healthy());
        assert_eq!(levels(&report), [Level::Ok; 4]);
        assert_eq!(report.level, Level::Ok);
        assert!(outcome(&report).is_ok());
        let c = &report.checks;
        assert_eq!(c.daemon.summary, format!("forskapd {VERSION} on {SOCKET}"));
        assert_eq!(c.session.summary, "@ada on gitlab.example.com");
        assert_eq!(
            c.session.details,
            ["The token expires on 2027-04-15 (in 90 days); the daemon rotates it before that."]
        );
        assert_eq!(c.sync.summary, "3 jobs, none failing");
        assert!(c.sync.details.is_empty(), "{:?}", c.sync.details);
        assert_eq!(c.queue.summary, "no failed writes");
    }

    #[test]
    fn an_unreachable_daemon_is_an_error_and_the_rest_is_skipped() {
        let report = report(&unreachable("unix:/tmp/does-not-exist.socket"));
        assert_eq!(
            levels(&report),
            [Level::Error, Level::Skipped, Level::Skipped, Level::Skipped]
        );
        let daemon = &report.checks.daemon;
        assert_eq!(
            daemon.summary,
            "not reachable on unix:/tmp/does-not-exist.socket"
        );
        assert_eq!(daemon.details[0], "No socket exists there.");
        assert!(
            daemon.details[1].starts_with("Start it with `"),
            "{:?}",
            daemon.details
        );
        for row in &report.checks.rows()[1..] {
            assert_eq!(row.summary, "the daemon is not reachable");
        }
        assert!(outcome(&report).unwrap_err().is::<Unhealthy>());
    }

    #[test]
    fn a_daemon_that_stops_answering_is_an_error() {
        // It took the connection, then went silent on the first call.
        let answers = Answers {
            info: Answer::TimedOut,
            interface: Answer::Unasked,
            who: Answer::Unasked,
            jobs: Answer::Unasked,
            queued: Answer::Unasked,
            failures: Answer::Unasked,
            ..healthy()
        };
        let report = report(&answers);
        assert_eq!(
            levels(&report),
            [Level::Error, Level::Skipped, Level::Skipped, Level::Skipped]
        );
        assert_eq!(
            report.checks.daemon.summary,
            format!("no answer on {SOCKET} within 10s")
        );
        assert_eq!(
            report.checks.queue.summary,
            "not asked: the daemon stopped answering"
        );

        // Later: the check that asked is the error.
        let answers = Answers {
            jobs: Answer::TimedOut,
            queued: Answer::Unasked,
            failures: Answer::Unasked,
            ..healthy()
        };
        let report = evaluate(&answers, now());
        assert_eq!(
            levels(&report),
            [Level::Ok, Level::Ok, Level::Error, Level::Skipped]
        );
        assert_eq!(
            report.checks.sync.summary,
            "no answer to GetSyncJobs within 10s"
        );
    }

    #[test]
    fn each_dormancy_reason_has_its_level_and_next_step() {
        for (reason, level, summary) in [
            (
                Some(NotAuthReason::unreachable),
                Level::Warning,
                "GitLab not reachable",
            ),
            (
                Some(NotAuthReason::no_credentials),
                Level::Error,
                "not logged in",
            ),
            (Some(NotAuthReason::logged_out), Level::Error, "logged out"),
            (
                Some(NotAuthReason::token_rejected),
                Level::Error,
                "GitLab rejected the token",
            ),
            (
                Some(NotAuthReason::keychain_error),
                Level::Error,
                "can't read the credentials from the keychain",
            ),
            // A daemon too old to say why.
            (None, Level::Error, "not connected to GitLab"),
        ] {
            let healing = reason == Some(NotAuthReason::unreachable);
            let report = report(&dormant_with(
                reason.clone(),
                Some("gitlab.example.com: 401"),
            ));
            let session = &report.checks.session;
            assert_eq!((session.level, session.summary.as_str()), (level, summary));
            assert_eq!(session.details[0], "gitlab.example.com: 401");
            let next = &session.details[1];
            if healing {
                assert!(next.contains("reconnects by itself"), "{next}");
            } else {
                assert!(next.contains("forskap auth login"), "{next}");
            }
            assert_eq!(report.level, level, "{reason:?}");
            assert_eq!(outcome(&report).is_ok(), healing, "{reason:?}");
        }
        // No detail, no empty line for it.
        let report = report(&dormant_with(Some(NotAuthReason::logged_out), Some("")));
        assert_eq!(
            report.checks.session.details,
            ["Run `forskap auth login` to authenticate."]
        );
    }

    #[test]
    fn a_token_expiring_soon_warns_only_when_it_is_not_rotated() {
        let session = |expires_in: i64, rotates: bool| {
            let me = WhoAmI_Reply {
                token_expires_at: Some(NOW + expires_in),
                token_rotates: rotates,
                ..me()
            };
            let answers = Answers {
                who: Answer::Got(Login::Connected(me)),
                ..healthy()
            };
            report(&answers).checks.session
        };
        let soon = session(2 * DAY + 3600, false);
        assert_eq!(soon.level, Level::Warning);
        assert_eq!(
            soon.summary,
            "@ada on gitlab.example.com; the token expires on 2027-01-17 (in 2 days)"
        );
        assert!(
            soon.details[0].contains("forskap auth login"),
            "{:?}",
            soon.details
        );

        // The daemon replaces it before then.
        let rotated = session(2 * DAY, true);
        assert_eq!(rotated.level, Level::Ok);
        assert_eq!(rotated.summary, "@ada on gitlab.example.com");
        // Far enough out.
        assert_eq!(session(30 * DAY, false).level, Level::Ok);
        // No known expiry.
        let me = WhoAmI_Reply {
            token_expires_at: None,
            token_rotates: false,
            ..me()
        };
        let answers = Answers {
            who: Answer::Got(Login::Connected(me)),
            ..healthy()
        };
        let session = report(&answers).checks.session;
        assert_eq!(session.level, Level::Ok);
        assert_eq!(session.details, ["The token has no known expiry date."]);
    }

    #[test]
    fn an_expired_token_is_an_error() {
        for rotates in [false, true] {
            let me = WhoAmI_Reply {
                token_expires_at: Some(NOW - DAY),
                token_rotates: rotates,
                ..me()
            };
            let answers = Answers {
                who: Answer::Got(Login::Connected(me)),
                ..healthy()
            };
            let report = report(&answers);
            let session = &report.checks.session;
            assert_eq!(session.level, Level::Error);
            assert_eq!(
                session.summary,
                "@ada on gitlab.example.com, but the token expired on 2027-01-14"
            );
            assert!(outcome(&report).is_err());
        }
    }

    #[test]
    fn a_rate_limit_pause_warns_for_how_long() {
        let answers = Answers {
            jobs: Answer::Got(GetSyncJobs_Reply {
                jobs: vec![fresh("assigned/issues")],
                paused_until: Some(NOW + 240),
                connected: None,
            }),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(sync.summary, "paused by a GitLab rate limit for another 4m");

        // One that is over says nothing.
        let answers = Answers {
            jobs: Answer::Got(GetSyncJobs_Reply {
                jobs: vec![fresh("assigned/issues")],
                paused_until: Some(NOW - 1),
                connected: None,
            }),
            ..healthy()
        };
        assert_eq!(report(&answers).checks.sync.level, Level::Ok);
    }

    #[test]
    fn failing_jobs_warn_with_their_count_and_the_first_few_named() {
        let forbidden = "GitLab error: 403 Forbidden";
        let answers = Answers {
            jobs: jobs(vec![
                fresh("assigned/issues"),
                failing("project/7/merge_requests", 8, forbidden),
                failing("project/9/boards", 1, forbidden),
                // Failed and due again, not backing off: still failing.
                SyncJob {
                    last_ok: Some(NOW - DAY),
                    next_due: Some(NOW - 10),
                    failures: 2,
                    last_error: Some("GitLab unavailable (502)".to_string()),
                    ..job("events", SyncJobStatus::due)
                },
                // Backing off since before a restart: no error kept.
                SyncJob {
                    last_error: None,
                    ..failing("member/groups", 3, "")
                },
                failing("project/11/issues", 1, forbidden),
            ]),
            ..healthy()
        };
        let report = report(&answers);
        let sync = &report.checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(sync.summary, "5 of 6 jobs failing");
        assert_eq!(
            sync.details,
            [
                "project/7/merge_requests: failed 8 times: GitLab error: 403 Forbidden",
                "project/9/boards: failed once: GitLab error: 403 Forbidden",
                "events: failed 2 times: GitLab unavailable (502)",
                "… and 2 more; `forskap sync jobs` lists them all",
            ]
        );
        let facts = sync.facts.as_ref().unwrap();
        assert_eq!(facts.failing.len(), 5);
        assert_eq!(facts.failing[3], "member/groups");
        // Something to look at, nothing broken.
        assert_eq!(report.level, Level::Warning);
        assert!(outcome(&report).is_ok());
    }

    /// The owner's account: two projects with a feature switched off, which
    /// the daemon gave up on: a note under an `ok` sync check.
    #[test]
    fn features_switched_off_in_a_project_are_only_noted() {
        let forbidden = "GitLab error: gitlab server error (403 Forbidden): 403 Forbidden";
        let mut all = vec![fresh("assigned/issues"), fresh("assigned/merge_requests")];
        all.extend((1..=55).map(|p| fresh(&format!("project/{p}/avatar"))));
        all.push(refused("project/60/merge_requests", forbidden));
        all.push(refused("project/61/boards", forbidden));
        let answers = Answers {
            jobs: jobs(all),
            ..healthy()
        };
        let report = report(&answers);
        assert_eq!(levels(&report), [Level::Ok; 4]);
        let sync = &report.checks.sync;
        assert_eq!(sync.summary, "59 jobs, none failing");
        assert_eq!(
            sync.details,
            [
                "2 jobs unavailable, GitLab refuses them: project/60/merge_requests, \
                 project/61/boards"
            ]
        );
        let facts = sync.facts.as_ref().unwrap();
        assert!(facts.failing.is_empty());
        assert_eq!(
            facts.unavailable,
            ["project/60/merge_requests", "project/61/boards"]
        );
        assert_eq!(report.level, Level::Ok);
        assert!(outcome(&report).is_ok());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["level"], "ok");
        assert_eq!(json["checks"]["sync"]["failing"], serde_json::json!([]));
        assert_eq!(
            json["checks"]["sync"]["unavailable"],
            serde_json::json!(["project/60/merge_requests", "project/61/boards"])
        );
        assert!(render(&report).ends_with(
            "\
sync     ok       59 jobs, none failing
                  2 jobs unavailable, GitLab refuses them: project/60/merge_requests, project/61/boards
queue    ok       no failed writes

healthy
"
        ));
    }

    /// The same two jobs from a daemon too old to tell them apart: they fail
    /// as they always did.
    #[test]
    fn a_daemon_too_old_to_say_still_reports_them_failing() {
        let forbidden = "GitLab error: gitlab server error (403 Forbidden): 403 Forbidden";
        let mut all = vec![fresh("assigned/issues"), fresh("assigned/merge_requests")];
        all.extend((1..=55).map(|p| fresh(&format!("project/{p}/avatar"))));
        all.push(failing("project/60/merge_requests", 8, forbidden));
        all.push(failing("project/61/boards", 8, forbidden));
        let answers = Answers {
            jobs: jobs(all),
            ..healthy()
        };
        let report = report(&answers);
        assert_eq!(
            levels(&report),
            [Level::Ok, Level::Ok, Level::Warning, Level::Ok]
        );
        assert_eq!(report.checks.sync.summary, "2 of 59 jobs failing");
        let facts = report.checks.sync.facts.as_ref().unwrap();
        assert!(facts.unavailable.is_empty());
        assert!(outcome(&report).is_ok());
    }

    /// Several of a kind are counted, a lone one named; one whose error the
    /// daemon lost in a restart is still one; a job that fails for real
    /// next to them still warns.
    #[test]
    fn unavailable_jobs_are_noted_by_kind_beside_the_failing_ones() {
        let forbidden = "GitLab error: 403 Forbidden";
        let answers = Answers {
            jobs: jobs(vec![
                fresh("assigned/issues"),
                refused("project/61/boards", forbidden),
                SyncJob {
                    last_error: None,
                    ..refused("project/62/boards", forbidden)
                },
                refused("project/60/merge_requests", forbidden),
                failing("project/9/issues", 1, "GitLab error: 400 Bad Request"),
            ]),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(sync.summary, "1 of 5 jobs failing");
        assert_eq!(
            sync.details,
            [
                "project/9/issues: failed once: GitLab error: 400 Bad Request",
                "3 jobs unavailable, GitLab refuses them: project/*/boards (2), \
                 project/60/merge_requests",
            ]
        );
        let facts = sync.facts.unwrap();
        assert_eq!(facts.failing, ["project/9/issues"]);
        assert_eq!(
            facts.unavailable,
            [
                "project/61/boards",
                "project/62/boards",
                "project/60/merge_requests"
            ]
        );
    }

    #[test]
    fn epics_gitlab_does_not_serve_are_only_noted() {
        // From a daemon that says so, and from one too old to: by their key,
        // as before.
        for (three, four) in [
            (
                refused("group/3/epics", "GitLab error: 404 Not Found"),
                refused("group/4/epics", "GitLab error: 403 Forbidden"),
            ),
            (
                failing("group/3/epics", 1, "GitLab error: 404 Not Found"),
                failing("group/4/epics", 1, "GitLab error: 403 Forbidden"),
            ),
        ] {
            let answers = Answers {
                jobs: jobs(vec![fresh("assigned/issues"), three, four]),
                ..healthy()
            };
            let sync = report(&answers).checks.sync;
            assert_eq!(sync.level, Level::Ok);
            assert_eq!(sync.summary, "3 jobs, none failing");
            assert_eq!(
                sync.details,
                [
                    "2 jobs unavailable, GitLab refuses them: group/*/epics (2)",
                    "Epics need GitLab Premium or Ultimate.",
                ]
            );
            let facts = sync.facts.unwrap();
            assert!(facts.failing.is_empty());
            assert_eq!(facts.unavailable, ["group/3/epics", "group/4/epics"]);
        }

        // A daemon that says so decides: an epics job it still retries (a
        // 5xx) fails like any other.
        let answers = Answers {
            jobs: jobs(vec![SyncJob {
                unavailable: Some(false),
                ..failing("group/3/epics", 1, "GitLab unavailable (502)")
            }]),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(sync.facts.unwrap().failing, ["group/3/epics"]);
    }

    #[test]
    fn a_job_running_for_a_quarter_hour_hangs() {
        let running = |key: &str, secs: i64| SyncJob {
            last_ok: Some(NOW - DAY),
            running_since: Some(NOW - secs),
            ..job(key, SyncJobStatus::running)
        };
        // The slowest real fetch: about four minutes.
        let answers = Answers {
            jobs: jobs(vec![running("member/projects", 4 * 60), fresh("events")]),
            ..healthy()
        };
        assert_eq!(report(&answers).checks.sync.level, Level::Ok);

        let answers = Answers {
            jobs: jobs(vec![running("member/projects", 47 * 60), fresh("events")]),
            ..healthy()
        };
        let report = report(&answers);
        let sync = &report.checks.sync;
        assert_eq!(sync.level, Level::Error);
        assert_eq!(sync.summary, "member/projects running for 47m");
        assert!(sync.details[0].starts_with("A fetch this long hangs"));
        assert_eq!(sync.facts.as_ref().unwrap().hanging, ["member/projects"]);
        assert!(outcome(&report).is_err());

        let answers = Answers {
            jobs: jobs(vec![
                running("member/projects", 47 * 60),
                running("project/7/issues", 16 * 60),
            ]),
            ..healthy()
        };
        let sync = evaluate(&answers, now()).checks.sync;
        assert_eq!(sync.summary, "2 jobs running for over 15m");
        assert_eq!(
            sync.details[..2],
            [
                "member/projects: running for 47m",
                "project/7/issues: running for 16m"
            ]
        );
    }

    #[test]
    fn overdue_jobs_warn_only_while_the_worker_starts_nothing() {
        let overdue = |key: &str, late: i64| SyncJob {
            last_ok: Some(NOW - late - 1800),
            next_due: Some(NOW - late),
            ..job(key, SyncJobStatus::due)
        };
        let stale = |key: &str| SyncJob {
            last_ok: Some(NOW - 2 * 3600),
            next_due: Some(NOW + 600),
            ..job(key, SyncJobStatus::waiting)
        };
        let idle = vec![
            stale("assigned/issues"),
            overdue("project/7/issues", 2 * 3600),
            overdue("project/8/issues", 45 * 60),
        ];
        let answers = Answers {
            jobs: jobs(idle.clone()),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(sync.summary, "2 jobs overdue, none started in the last 30m");
        assert_eq!(sync.details[0], "the oldest, project/7/issues, by 2h");

        // Where the daemon says what keeps the oldest, so does the check.
        let mut held = idle.clone();
        held[1].held_by = Some(SyncJobHold::slots);
        let answers = Answers {
            jobs: jobs(held),
            ..healthy()
        };
        assert_eq!(
            report(&answers).checks.sync.details[0],
            "the oldest, project/7/issues, by 2h: it runs when a slot is free"
        );

        // Something runs: it is catching up, as after a suspend.
        let mut busy = idle.clone();
        busy[0] = SyncJob {
            last_ok: Some(NOW - 2 * 3600),
            running_since: Some(NOW - 5),
            ..job("assigned/issues", SyncJobStatus::running)
        };
        let answers = Answers {
            jobs: jobs(busy),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Ok);
        assert_eq!(
            sync.details,
            ["catching up: 2 jobs overdue, the oldest by 2h"]
        );

        // Paused: the rate limit holds them, and says so.
        let answers = Answers {
            jobs: Answer::Got(GetSyncJobs_Reply {
                jobs: idle.clone(),
                paused_until: Some(NOW + 60),
                connected: None,
            }),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.summary, "paused by a GitLab rate limit for another 1m");
        assert_eq!(sync.details, ["2 jobs overdue, the oldest by 2h"]);

        // Late, but not by much.
        let answers = Answers {
            jobs: jobs(vec![stale("events"), overdue("project/7/issues", 20 * 60)]),
            ..healthy()
        };
        assert_eq!(report(&answers).checks.sync.level, Level::Ok);
    }

    #[test]
    fn a_dormant_session_puts_the_sync_on_hold_without_more_warnings() {
        let paused_failing_overdue = Answer::Got(GetSyncJobs_Reply {
            jobs: vec![
                failing("project/7/boards", 3, "GitLab error: 403 Forbidden"),
                SyncJob {
                    last_ok: Some(NOW - DAY),
                    next_due: Some(NOW - DAY / 2),
                    ..job("assigned/issues", SyncJobStatus::due)
                },
            ],
            paused_until: Some(NOW + 600),
            connected: None,
        });
        let answers = Answers {
            jobs: paused_failing_overdue,
            ..dormant_with(Some(NotAuthReason::unreachable), None)
        };
        let report = report(&answers);
        let sync = &report.checks.sync;
        assert_eq!(sync.level, Level::Skipped);
        assert_eq!(sync.summary, "on hold while there is no GitLab session");
        assert!(sync.details.is_empty(), "{:?}", sync.details);
        // Only the session finding counts.
        assert_eq!(
            levels(&report),
            [Level::Ok, Level::Warning, Level::Skipped, Level::Ok]
        );

        // A hung fetch still is one.
        let answers = Answers {
            jobs: jobs(vec![SyncJob {
                running_since: Some(NOW - 3600),
                ..job("member/projects", SyncJobStatus::running)
            }]),
            ..dormant_with(Some(NotAuthReason::unreachable), None)
        };
        let sync = evaluate(&answers, now()).checks.sync;
        assert_eq!(sync.level, Level::Error);
        assert_eq!(sync.details[1], "on hold while there is no GitLab session");
    }

    /// The worker says itself whether it has a session to run its jobs with;
    /// what `WhoAmI` answered a moment earlier only stands in for a daemon
    /// too old to say.
    #[test]
    fn the_sync_is_on_hold_by_the_workers_own_word() {
        let overdue = SyncJob {
            last_ok: Some(NOW - DAY),
            next_due: Some(NOW - DAY / 2),
            ..job("assigned/issues", SyncJobStatus::due)
        };
        let said = |connected| {
            Answer::Got(GetSyncJobs_Reply {
                jobs: vec![overdue.clone()],
                paused_until: None,
                connected,
            })
        };
        // Lost between the two answers.
        let answers = Answers {
            jobs: said(Some(false)),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(
            (sync.level, sync.summary.as_str()),
            (Level::Skipped, "on hold while there is no GitLab session")
        );
        // Back between the two answers: the jobs are what the check is about.
        let answers = Answers {
            jobs: said(Some(true)),
            ..dormant_with(Some(NotAuthReason::unreachable), None)
        };
        let sync = report(&answers).checks.sync;
        assert_ne!(sync.level, Level::Skipped, "{}", sync.summary);
        assert!(sync.summary.contains("overdue") || sync.details[0].contains("overdue"));
    }

    #[test]
    fn lists_that_never_synced_are_noted() {
        let answers = Answers {
            jobs: jobs(vec![
                fresh("assigned/issues"),
                job("events", SyncJobStatus::due),
                job("timelogs/all", SyncJobStatus::waiting),
                // Per-project jobs are too many to name.
                job("project/7/avatar", SyncJobStatus::due),
                // Named as failing already.
                failing("member/groups", 1, "GitLab error: 500"),
            ]),
            ..healthy()
        };
        let sync = report(&answers).checks.sync;
        assert_eq!(sync.level, Level::Warning);
        assert_eq!(
            sync.details.last().unwrap(),
            "not synced yet: events, timelogs/all"
        );
        assert_eq!(sync.facts.unwrap().never_synced, ["events", "timelogs/all"]);
    }

    /// A write the daemon gave up.
    fn failed(id: i64) -> FailedTask {
        FailedTask {
            id,
            op: "post_time".to_string(),
            kind: forskap_api::IssuableKind::work_item,
            project_id: 7,
            iid: 42,
            detail: "1h".to_string(),
            error: "403 Forbidden".to_string(),
            queued_at: NOW - DAY,
            failed_at: NOW - 60,
        }
    }

    #[test]
    fn failed_writes_warn_with_the_count_and_where_to_look() {
        let answers = Answers {
            failures: Answer::Got(vec![failed(1), failed(2)]),
            ..healthy()
        };
        let report = report(&answers);
        let queue = &report.checks.queue;
        assert_eq!(queue.level, Level::Warning);
        assert_eq!(queue.summary, "2 failed writes");
        assert!(queue.details[0].contains("`forskap queue list`"));
        assert_eq!(queue.facts.as_ref().unwrap().failed_writes, 2);
        assert!(outcome(&report).is_ok());
        let answers = Answers {
            failures: Answer::Got(vec![failed(1)]),
            ..healthy()
        };
        assert_eq!(
            evaluate(&answers, now()).checks.queue.summary,
            "1 failed write"
        );
    }

    /// A write still to be sent.
    fn waiting(id: i64, iid: i64) -> QueuedWrite {
        QueuedWrite {
            id,
            op: "PostTime".to_string(),
            kind: forskap_api::IssuableKind::work_item,
            project_id: 7,
            iid,
            detail: "1h".to_string(),
            queued_at: NOW - 180,
            attempts: 0,
            running: false,
            blocked: false,
            next_attempt_at: None,
            last_error: None,
            expires_at: NOW + 7 * DAY,
        }
    }

    fn queue_of(
        writes: Vec<QueuedWrite>,
        paused_until: Option<i64>,
    ) -> Answer<Option<GetQueue_Reply>> {
        Answer::Got(Some(GetQueue_Reply {
            writes,
            paused_until,
        }))
    }

    /// Writes waiting to be sent are no trouble, only worth knowing: the
    /// check stays ok and says how many, and what they wait for.
    #[test]
    fn queued_writes_are_noted_with_what_they_wait_for() {
        let answers = Answers {
            queued: queue_of(vec![waiting(1, 42), waiting(2, 43)], None),
            ..healthy()
        };
        let queue = report(&answers).checks.queue;
        assert_eq!(queue.level, Level::Ok);
        assert_eq!(queue.summary, "no failed writes");
        assert_eq!(queue.details[0], "2 writes queued");
        assert!(queue.details[1].starts_with("The daemon is sending them"));
        let facts = queue.facts.unwrap();
        assert_eq!((facts.failed_writes, facts.queued_writes), (0, Some(2)));

        // Without a session they wait for one, however long.
        let answers = Answers {
            queued: queue_of(vec![waiting(1, 42)], None),
            ..dormant_with(Some(NotAuthReason::unreachable), None)
        };
        let queue = report(&answers).checks.queue;
        assert_eq!(queue.level, Level::Ok);
        assert_eq!(
            queue.details,
            [
                "1 write queued",
                "They are sent once there is a GitLab session."
            ]
        );

        let answers = Answers {
            queued: queue_of(vec![waiting(1, 42)], Some(NOW + 240)),
            ..healthy()
        };
        assert_eq!(
            report(&answers).checks.queue.details[1],
            "They are paused by a GitLab rate limit for another 4m."
        );
    }

    /// Failed writes stay what the check warns about; the waiting ones
    /// follow as a note.
    #[test]
    fn failed_writes_come_before_the_queued_ones() {
        let answers = Answers {
            queued: queue_of(vec![waiting(5, 42)], None),
            failures: Answer::Got(vec![failed(1)]),
            ..healthy()
        };
        let queue = report(&answers).checks.queue;
        assert_eq!(
            (queue.level, queue.summary.as_str()),
            (Level::Warning, "1 failed write")
        );
        assert!(
            queue.details.iter().any(|d| d == "1 write queued"),
            "{:?}",
            queue.details
        );
    }

    /// A daemon before 1.3 lists no queue: the check is what it was, and
    /// its facts say nothing of what waits.
    #[test]
    fn a_daemon_too_old_to_list_its_queue_is_judged_by_its_failures() {
        let answers = Answers {
            queued: Answer::Got(None),
            ..healthy()
        };
        let queue = report(&answers).checks.queue;
        assert_eq!(
            (queue.level, queue.summary.as_str()),
            (Level::Ok, "no failed writes")
        );
        assert!(queue.details.is_empty());
        assert_eq!(queue.facts.unwrap().queued_writes, None);

        // An answer that failed is no failure of the check either.
        let answers = Answers {
            queued: Answer::Failed("boom".to_string()),
            ..healthy()
        };
        assert_eq!(report(&answers).checks.queue.level, Level::Ok);
    }

    fn of_version(version: &str, interface: Answer<Option<String>>) -> Answers {
        Answers {
            info: Answer::Got(Info {
                product: "forskapd".to_string(),
                version: version.to_string(),
            }),
            interface,
            ..healthy()
        }
    }

    /// Another package version speaking this interface is fine.
    #[test]
    fn a_daemon_is_judged_by_its_interface() {
        let answers = of_version("0.0.1", Answer::Got(Some(API_VERSION.to_string())));
        let daemon = report(&answers).checks.daemon;
        assert_eq!(daemon.level, Level::Ok);
        assert_eq!(daemon.summary, format!("forskapd 0.0.1 on {SOCKET}"));
        let facts = daemon.facts.unwrap();
        assert_eq!(facts.version.as_deref(), Some("0.0.1"));
        assert_eq!(facts.api_version.as_deref(), Some(API_VERSION));

        // A patch apart: a fix to a binding alone, the same interface.
        let (minor, _) = API_VERSION.rsplit_once('.').unwrap();
        let patched = format!("{minor}.99");
        let answers = of_version(VERSION, Answer::Got(Some(patched.clone())));
        let daemon = report(&answers).checks.daemon;
        assert_eq!(daemon.level, Level::Ok, "{patched}");
        assert_eq!(daemon.facts.unwrap().api_version, Some(patched));
    }

    #[test]
    fn a_daemon_of_another_interface_warns_to_restart_it() {
        let answers = of_version(VERSION, Answer::Got(Some("0.0.1".to_string())));
        let report = report(&answers);
        let daemon = &report.checks.daemon;
        assert_eq!(daemon.level, Level::Warning);
        assert_eq!(
            daemon.summary,
            format!("forskapd {VERSION} on {SOCKET} speaks interface 0.0.1, forskap {API_VERSION}")
        );
        assert!(
            daemon.details[0].contains("restart"),
            "{:?}",
            daemon.details
        );
        assert_eq!(
            daemon.facts.as_ref().unwrap().api_version.as_deref(),
            Some("0.0.1")
        );
        assert!(outcome(&report).is_ok());

        // One from before `GetStatus`: the same.
        let answers = of_version("0.0.1", Answer::Got(None));
        let daemon = evaluate(&answers, now()).checks.daemon;
        assert_eq!(daemon.level, Level::Warning);
        assert_eq!(
            daemon.summary,
            format!(
                "forskapd 0.0.1 on {SOCKET} speaks an interface older than forskap's \
                 {API_VERSION}"
            )
        );
        assert!(daemon.details[0].contains("restart"));
        let facts = daemon.facts.unwrap();
        assert_eq!(facts.version.as_deref(), Some("0.0.1"));
        assert_eq!(facts.api_version, None);
    }

    /// How a daemon from before `GetStatus` answers it.
    #[test]
    fn a_method_the_daemon_lacks_is_told_from_other_failures() {
        let error = |kind| ApiError::from(varlink::Error::from(kind));
        let missing = varlink::ErrorKind::MethodNotFound("org.thehoster.forskapd.GetStatus".into());
        assert!(friendly::is_method_not_found(&error(missing)));
        assert!(!friendly::is_method_not_found(&error(
            varlink::ErrorKind::ConnectionClosed
        )));
    }

    /// Each of the daemon's errors is told by its message, as the other
    /// commands print it.
    #[test]
    fn a_daemon_error_reads_as_its_message() {
        use forskap_api::{
            GitlabError_Args, GitlabUnavailable_Args, Internal_Args, InvalidArgument_Args,
            NotFound_Args,
        };
        let message = || "it went wrong".to_string();
        for (kind, described) in [
            (
                ApiErrorKind::GitlabError(Some(GitlabError_Args {
                    message: message(),
                    status: Some(403),
                })),
                "it went wrong (HTTP 403)",
            ),
            (
                ApiErrorKind::GitlabUnavailable(Some(GitlabUnavailable_Args {
                    message: message(),
                })),
                "it went wrong",
            ),
            (
                ApiErrorKind::Internal(Some(Internal_Args { message: message() })),
                "it went wrong",
            ),
            (
                ApiErrorKind::InvalidArgument(Some(InvalidArgument_Args {
                    argument: "options.limit".into(),
                    message: message(),
                })),
                "it went wrong (options.limit)",
            ),
            (
                ApiErrorKind::NotFound(Some(NotFound_Args { message: message() })),
                "it went wrong",
            ),
        ] {
            assert_eq!(describe(&ApiError::from(kind)), described);
        }
        let internal =
            admin::ErrorKind::Internal(Some(admin::Internal_Args { message: message() }));
        assert_eq!(describe(&admin::Error::from(internal)), "it went wrong");
    }

    #[test]
    fn a_daemon_that_does_not_say_is_reachable_all_the_same() {
        let answers = Answers {
            info: Answer::Failed("org.varlink.service.MethodNotFound".to_string()),
            ..healthy()
        };
        let daemon = evaluate(&answers, now()).checks.daemon;
        assert_eq!(daemon.level, Level::Warning);
        assert_eq!(daemon.facts.unwrap().version, None);

        let answers = of_version(VERSION, Answer::Failed("Connection closed".to_string()));
        let daemon = evaluate(&answers, now()).checks.daemon;
        assert_eq!(daemon.level, Level::Warning);
        assert_eq!(daemon.details, ["GetStatus failed: Connection closed"]);

        // Silent on the second call: stuck as on the first.
        let answers = Answers {
            interface: Answer::TimedOut,
            who: Answer::Unasked,
            jobs: Answer::Unasked,
            queued: Answer::Unasked,
            failures: Answer::Unasked,
            ..healthy()
        };
        let report = report(&answers);
        assert_eq!(
            levels(&report),
            [Level::Error, Level::Skipped, Level::Skipped, Level::Skipped]
        );
        assert_eq!(
            report.checks.daemon.summary,
            format!("no answer on {SOCKET} within 10s")
        );
    }

    #[test]
    fn a_reply_this_cli_cannot_read_is_an_error_of_its_check() {
        let answers = Answers {
            who: Answer::Failed(varlink_words(&varlink::ErrorKind::SerdeJsonSer(
                serde_json::error::Category::Data,
            ))),
            ..healthy()
        };
        let report = report(&answers);
        let session = &report.checks.session;
        assert_eq!(
            (session.level, session.summary.as_str()),
            (Level::Error, "WhoAmI failed")
        );
        assert!(
            session.details[0].contains("different builds"),
            "{:?}",
            session.details
        );
        // The sync runs on regardless: nothing tells it is down.
        assert_eq!(report.checks.sync.level, Level::Ok);
    }

    #[test]
    fn only_an_error_exits_non_zero() {
        for (level, fails) in [
            (Level::Ok, false),
            (Level::Skipped, false),
            (Level::Warning, false),
            (Level::Error, true),
        ] {
            let report = Report {
                level,
                ..evaluate(&healthy(), now())
            };
            let outcome = outcome(&report);
            assert_eq!(outcome.is_err(), fails, "{level:?}");
            if let Err(e) = outcome {
                assert!(e.is::<Unhealthy>());
            }
        }
    }

    #[test]
    fn render_aligns_the_checks_and_closes_with_the_verdict() {
        let answers = Answers {
            jobs: jobs(vec![
                fresh("assigned/issues"),
                failing("project/9/boards", 8, "403 Forbidden"),
            ]),
            ..healthy()
        };
        assert_eq!(
            render(&report(&answers)),
            format!(
                "\
daemon   ok       forskapd {VERSION} on {SOCKET}
session  ok       @ada on gitlab.example.com
                  The token expires on 2027-04-15 (in 90 days); the daemon rotates it before that.
sync     warning  1 of 2 jobs failing
                  project/9/boards: failed 8 times: 403 Forbidden
queue    ok       no failed writes

healthy: 1 warning
"
            )
        );

        let text = render(&report(&unreachable(SOCKET)));
        assert!(
            text.starts_with(&format!(
                "\
daemon   error    not reachable on {SOCKET}
                  No socket exists there.
                  Start it with `"
            )),
            "{text}"
        );
        assert!(
            text.ends_with(
                "\
session  skipped  the daemon is not reachable
sync     skipped  the daemon is not reachable
queue    skipped  the daemon is not reachable

unhealthy: 1 error, 3 skipped
"
            ),
            "{text}"
        );
        assert_eq!(
            verdict(&report(&healthy()).checks.rows()),
            ("healthy", String::new())
        );
        // All `ok`: the columns stay where a warning would put them.
        let text = render(&report(&healthy()));
        assert!(text.starts_with("daemon   ok       forskapd"), "{text}");
        assert!(
            text.ends_with("queue    ok       no failed writes\n\nhealthy\n"),
            "{text}"
        );
    }

    #[test]
    fn render_colours_the_levels_and_keeps_the_columns() {
        style::force(true);
        let answers = Answers {
            queued: Answer::Got(Some(no_queue())),
            failures: Answer::Got(vec![]),
            jobs: Answer::TimedOut,
            ..healthy()
        };
        let text = render(&report(&answers));
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with("\x1b[1mdaemon \x1b[0m  \x1b[32mok     \x1b[0m  forskapd"),
            "{text}"
        );
        assert!(
            lines[2].starts_with("                  \x1b[2mThe token expires on"),
            "{text}"
        );
        assert!(
            lines[3].starts_with("\x1b[1msync   \x1b[0m  \x1b[31merror  \x1b[0m  no answer"),
            "{text}"
        );
        assert!(
            text.ends_with("\n\x1b[31munhealthy\x1b[0m: 1 error\n"),
            "{text}"
        );
    }

    #[test]
    fn json_carries_the_levels_and_the_numbers_behind_them() {
        let answers = Answers {
            jobs: jobs(vec![
                fresh("assigned/issues"),
                failing("project/9/boards", 8, "403 Forbidden"),
            ]),
            ..healthy()
        };
        let json = serde_json::to_value(report(&answers)).unwrap();
        assert_eq!(json["level"], "warning");
        let checks = &json["checks"];
        assert_eq!(checks["daemon"]["name"], "daemon");
        assert_eq!(checks["daemon"]["version"], VERSION);
        assert_eq!(checks["daemon"]["cli_version"], VERSION);
        assert_eq!(checks["daemon"]["api_version"], API_VERSION);
        assert_eq!(checks["daemon"]["cli_api_version"], API_VERSION);
        assert_eq!(checks["session"]["level"], "ok");
        assert_eq!(checks["session"]["username"], "ada");
        assert_eq!(checks["session"]["host"], "gitlab.example.com");
        assert_eq!(checks["session"]["reason"], serde_json::Value::Null);
        assert_eq!(checks["sync"]["level"], "warning");
        assert_eq!(checks["sync"]["jobs"]["backing_off"], 1);
        assert_eq!(checks["sync"]["jobs"]["total"], 2);
        assert_eq!(checks["sync"]["failing"][0], "project/9/boards");
        assert_eq!(checks["sync"]["unavailable"], serde_json::json!([]));
        assert_eq!(checks["queue"]["failed_writes"], 0);
        assert_eq!(checks["queue"]["details"], serde_json::json!([]));

        let dormant = dormant_with(Some(NotAuthReason::token_rejected), Some("401"));
        let json = serde_json::to_value(report(&dormant)).unwrap();
        assert_eq!(json["checks"]["session"]["connected"], false);
        assert_eq!(json["checks"]["session"]["reason"], "token_rejected");
        // Skipped for want of an answer: no numbers to give.
        let json = serde_json::to_value(report(&unreachable(SOCKET))).unwrap();
        assert_eq!(json["checks"]["sync"]["level"], "skipped");
        assert!(json["checks"]["sync"].get("jobs").is_none());
        assert_eq!(json["checks"]["daemon"]["version"], serde_json::Value::Null);

        // YAML takes the same document, the facts flattened in as well.
        let yaml = serde_saphyr::to_string(&report(&healthy())).unwrap();
        assert!(yaml.contains("\n    failed_writes: 0\n"), "{yaml}");
        assert!(yaml.contains("\n    username: ada\n"), "{yaml}");
    }

    #[test]
    fn connect_failures_say_what_to_look_at() {
        let because = |kind| unreachable_because(&varlink::Error::from(kind));
        assert_eq!(
            because(varlink::ErrorKind::Io(std::io::ErrorKind::NotFound)),
            "No socket exists there."
        );
        assert_eq!(
            because(varlink::ErrorKind::Io(
                std::io::ErrorKind::ConnectionRefused
            )),
            "The socket exists, but nothing listens on it."
        );
        assert_eq!(
            because(varlink::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)),
            "Connecting failed: permission denied."
        );
        assert_eq!(
            because(varlink::ErrorKind::Io(std::io::ErrorKind::InvalidInput)),
            "That path can't be a unix socket: it is too long."
        );
        assert!(because(varlink::ErrorKind::InvalidAddress).contains("`unix:PATH`"));
    }
}
