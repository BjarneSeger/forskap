//! The account a dry run (`forskapd --dry-run`) serves: an in-memory
//! [`GitlabApi`] over a small fixture, so the real sync engine fills the dry
//! run's store from it and every read and write runs the production code.
//!
//! Writes change the fixture and nothing else. Every host and URL lies in
//! [`HOST`], under the `.invalid` top-level domain, which never resolves: a
//! client opening a link can't land anywhere real. The rows are the sync
//! model's own types serialized, which the sync reads back like GitLab's.
//! Times are relative to the moment the dry run started, so every sync
//! window holds something.
//!
//! Unlike the tests' fake (`testing.rs`), it answers each listing the way
//! GitLab would for this one account, filters included, and fails only where
//! GitLab would refuse: an unknown item, an unreadable duration, a write to
//! the archived project, a token rotation.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::{Config, RotatePolicy, SearchPopulation};
use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Issuable, Listing, NewIssue, RotatedToken, TokenInfo};
use crate::handlers::Session;
use crate::secrets::Token;
use crate::sync::model::{
    Board, BoardList, Epic, EpicRef, Event, Group, Issue, LabelRef, MergeRequest, NoteRef, Project,
    PushData, TimeStats, Timelog, UserRef,
};
use crate::sync::now_secs;

/// The host the dry run is logged in to; `WhoAmI` answers with it.
pub const HOST: &str = "dry-run.invalid";
const BASE: &str = "https://dry-run.invalid";

/// The demo user.
pub const USER_ID: i64 = 4242;
pub const USERNAME: &str = "demo";
const ALEX: i64 = 4243;
const SAM: i64 = 4244;

/// The project with an avatar, and the archived one.
pub const AVATAR_PROJECT: i64 = 101;
pub const ARCHIVED_PROJECT: i64 = 104;

/// The avatar of [`AVATAR_PROJECT`]: a 32×32 PNG.
pub const AVATAR: &[u8] = include_bytes!("demo/avatar.png");

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

/// The config of a dry run: the baked-in defaults, never the user's file,
/// with a fixture this small synced quickly and often.
pub fn config() -> Config {
    let mut c = crate::config::defaults();
    // Every demo project gets its issues, MRs and epics synced.
    c.search.population = SearchPopulation::Member;
    c.sync.startup_spread_secs = 0;
    c.sync.job_gap_ms = 0;
    c.sync.max_in_flight = 4;
    c.refresh.quick.interval_secs = 60;
    c.refresh.slow.interval_secs = 300;
    c.search.partial_interval_secs = 60;
    c.search.full_interval_secs = 300;
    // Nothing to reconnect to and no token to rotate; neither supervisor
    // runs in a dry run anyway.
    c.reconnect.enabled = false;
    c.auth.rotate = RotatePolicy::Never;
    c
}

/// An issue with who opened it, which GitLab knows and the sync's row
/// doesn't keep.
#[derive(Debug, Clone)]
struct DemoIssue {
    row: Issue,
    author: i64,
}

#[derive(Debug, Default)]
struct State {
    groups: Vec<Group>,
    projects: Vec<Project>,
    issues: Vec<DemoIssue>,
    merge_requests: Vec<MergeRequest>,
    epics: Vec<Epic>,
    boards: Vec<Board>,
    events: Vec<Event>,
    timelogs: Vec<Timelog>,
    /// The next id of a timelog or event made by a write.
    next_id: u64,
}

/// The dry run's GitLab: the fixture, changed by the writes.
pub struct DemoGitlab {
    state: Mutex<State>,
    created_at: u64,
}

impl DemoGitlab {
    /// The fixture, dated relative to `now` (unix seconds).
    pub fn new(now: u64) -> Self {
        Self {
            state: Mutex::new(fixture(now)),
            created_at: now,
        }
    }

    /// A session logged in to the demo account through `self`.
    pub fn session(self: Arc<Self>) -> Session {
        Session {
            gitlab: self,
            host: HOST.into(),
            user_id: USER_ID,
            username: USERNAME.into(),
            token: Token::new("dry-run"),
        }
    }
}

fn user(id: i64) -> UserRef {
    let username = match id {
        USER_ID => USERNAME,
        ALEX => "alex",
        SAM => "sam",
        _ => "someone",
    };
    UserRef {
        id,
        username: username.into(),
    }
}

fn project_path(state: &State, project_id: i64) -> String {
    state
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map_or_else(String::new, |p| p.path_with_namespace.clone())
}

fn issue_url(path: &str, iid: i64) -> String {
    format!("{BASE}/{path}/-/issues/{iid}")
}

fn merge_request_url(path: &str, iid: i64) -> String {
    format!("{BASE}/{path}/-/merge_requests/{iid}")
}

fn epic_url(group_path: &str, iid: i64) -> String {
    format!("{BASE}/groups/{group_path}/-/epics/{iid}")
}

/// GitLab's spelling of a time spent: `1h 30m`, `1d 2h` (8-hour days,
/// 5-day weeks).
fn human(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mut left = secs;
    let mut parts = Vec::new();
    for (unit, size) in [("w", 5 * 8 * HOUR), ("d", 8 * HOUR), ("h", HOUR), ("m", 60)] {
        let n = left / size;
        if n > 0 {
            parts.push(format!("{n}{unit}"));
            left -= n * size;
        }
    }
    parts.join(" ")
}

/// Seconds in a GitLab duration (`1h30m`, `1.5h`, `2d`, `45`): months of 4
/// weeks, weeks of 5 days, days of 8 hours; a bare number is hours. `None`
/// for anything GitLab would refuse, nothing included.
fn parse_duration(text: &str) -> Option<u64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = rest[..unit_end].to_ascii_lowercase();
        rest = rest[unit_end..].trim_start();
        let size = match unit.as_str() {
            "mo" => 4 * 5 * 8 * HOUR,
            "w" => 5 * 8 * HOUR,
            "d" => 8 * HOUR,
            "h" | "" => HOUR,
            "m" => 60,
            "s" => 1,
            _ => return None,
        };
        total += number * size as f64;
    }
    let secs = total.round() as u64;
    (secs > 0).then_some(secs)
}

fn not_found() -> Error {
    refused(404, "404 Not Found".into())
}

/// GitLab's answer with `status`, as the real client classifies it.
fn refused(status: u16, detail: String) -> Error {
    Error::Rejected { status, detail }
}

/// `updated_after` as unix seconds; everything without one.
fn since(after: &Option<DateTime<Utc>>) -> u64 {
    after.map_or(0, |t| t.timestamp().max(0) as u64)
}

fn rows<T: serde::Serialize>(items: impl IntoIterator<Item = T>) -> Vec<Value> {
    items
        .into_iter()
        .map(|item| serde_json::to_value(item).expect("a model row serializes"))
        .collect()
}

impl State {
    /// An issue as GitLab lists it: its time spent summed up.
    fn issue_row(&self, issue: &DemoIssue) -> Issue {
        let spent: u64 = self
            .timelogs
            .iter()
            .filter(|t| {
                t.kind == Issuable::Issue
                    && t.project_id == issue.row.project_id
                    && t.iid == issue.row.iid
            })
            .map(|t| t.time_spent)
            .sum();
        let mut row = issue.row.clone();
        row.time_stats = Some(TimeStats {
            human_total_time_spent: if spent > 0 {
                human(spent)
            } else {
                String::new()
            },
        });
        row
    }

    /// `issues`, most recently updated first, as GitLab rows.
    fn issue_rows<'a>(&self, issues: impl Iterator<Item = &'a DemoIssue>) -> Vec<Value> {
        let mut issues: Vec<&DemoIssue> = issues.collect();
        issues.sort_by_key(|i| std::cmp::Reverse((i.row.updated_at, i.row.id)));
        rows(issues.into_iter().map(|i| self.issue_row(i)))
    }

    fn merge_request_rows<'a>(&self, mrs: impl Iterator<Item = &'a MergeRequest>) -> Vec<Value> {
        let mut mrs: Vec<&MergeRequest> = mrs.collect();
        mrs.sort_by_key(|m| std::cmp::Reverse((m.updated_at, m.id)));
        rows(mrs)
    }

    fn list(&self, listing: &Listing) -> Vec<Value> {
        let mine = |assignees: &[UserRef]| assignees.iter().any(|a| a.id == USER_ID);
        let issues = self.issues.iter();
        let mrs = self.merge_requests.iter();
        match listing {
            Listing::AssignedIssues => self
                .issue_rows(issues.filter(|i| i.row.state == "opened" && mine(&i.row.assignees))),
            Listing::AssignedMergeRequests => {
                self.merge_request_rows(mrs.filter(|m| m.state == "opened" && mine(&m.assignees)))
            }
            Listing::ProjectIssues {
                project_id,
                updated_after,
            } => self.issue_rows(issues.filter(|i| {
                i.row.project_id == *project_id && i.row.updated_at >= since(updated_after)
            })),
            Listing::ProjectMergeRequests {
                project_id,
                updated_after,
            } => {
                self.merge_request_rows(mrs.filter(|m| {
                    m.project_id == *project_id && m.updated_at >= since(updated_after)
                }))
            }
            Listing::AllIssues { updated_after } => {
                self.issue_rows(issues.filter(|i| i.row.updated_at >= since(updated_after)))
            }
            Listing::AllMergeRequests { updated_after } => {
                self.merge_request_rows(mrs.filter(|m| m.updated_at >= since(updated_after)))
            }
            Listing::RecentAuthoredIssues { updated_after } => {
                let since = since(&Some(*updated_after));
                self.issue_rows(issues.filter(|i| i.author == USER_ID && i.row.updated_at >= since))
            }
            Listing::RecentAssignedIssues { updated_after } => {
                let since = since(&Some(*updated_after));
                self.issue_rows(
                    issues.filter(|i| mine(&i.row.assignees) && i.row.updated_at >= since),
                )
            }
            Listing::MemberProjects => rows(&self.projects),
            Listing::MemberGroups => rows(&self.groups),
            Listing::GroupEpics {
                group_id,
                updated_after,
            } => {
                let mut epics: Vec<&Epic> = self
                    .epics
                    .iter()
                    .filter(|e| e.group_id == *group_id && e.updated_at >= since(updated_after))
                    .collect();
                epics.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
                rows(epics)
            }
            Listing::ProjectBoards { project_id } => {
                rows(self.boards.iter().filter(|b| b.project_id == *project_id))
            }
            // Oldest first; `after` is a date, compared exclusively.
            Listing::Events { after } => {
                let mut events: Vec<&Event> = self
                    .events
                    .iter()
                    .filter(|e| {
                        let day = DateTime::from_timestamp(e.created_at as i64, 0)
                            .map(|t| t.date_naive());
                        after.is_none_or(|after| day.is_some_and(|day| day > after))
                    })
                    .collect();
                events.sort_by_key(|e| (e.created_at, e.id));
                rows(events)
            }
            Listing::Issuable {
                kind: Issuable::Issue,
                project_id,
                iid,
            } => self.issue_rows(
                issues.filter(|i| i.row.project_id == *project_id && i.row.iid == *iid),
            ),
            Listing::Issuable {
                kind: Issuable::MergeRequest,
                project_id,
                iid,
            } => self
                .merge_request_rows(mrs.filter(|m| m.project_id == *project_id && m.iid == *iid)),
        }
    }

    /// Refuse a write to a project GitLab would refuse it to.
    fn writable(&self, project_id: i64) -> Result<()> {
        match self.projects.iter().find(|p| p.id == project_id) {
            None => Err(not_found()),
            Some(p) if p.archived => Err(refused(
                403,
                "403 Forbidden: the project is archived and read-only".into(),
            )),
            Some(_) => Ok(()),
        }
    }

    fn issue_mut(&mut self, project_id: i64, iid: i64) -> Result<&mut DemoIssue> {
        self.issues
            .iter_mut()
            .find(|i| i.row.project_id == project_id && i.row.iid == iid)
            .ok_or_else(not_found)
    }

    fn merge_request_mut(&mut self, project_id: i64, iid: i64) -> Result<&mut MergeRequest> {
        self.merge_requests
            .iter_mut()
            .find(|m| m.project_id == project_id && m.iid == iid)
            .ok_or_else(not_found)
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Change the issue or MR in place, as of `now`; returns its title, for
    /// an event.
    fn update(
        &mut self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        now: u64,
        change: impl FnOnce(&mut String, &mut Vec<UserRef>),
    ) -> Result<String> {
        self.writable(project_id)?;
        let (state, assignees, updated_at, title) = match kind {
            Issuable::Issue => {
                let i = &mut self.issue_mut(project_id, iid)?.row;
                (&mut i.state, &mut i.assignees, &mut i.updated_at, &i.title)
            }
            Issuable::MergeRequest => {
                let m = self.merge_request_mut(project_id, iid)?;
                (&mut m.state, &mut m.assignees, &mut m.updated_at, &m.title)
            }
        };
        change(state, assignees);
        *updated_at = now;
        Ok(title.clone())
    }

    fn event(&mut self, project_id: i64, action: &str, kind: Issuable, iid: i64, title: String) {
        let id = self.next_id();
        self.events.push(Event {
            id: id as i64,
            project_id,
            action_name: action.into(),
            target_type: kind.gid_type().into(),
            target_iid: iid,
            target_title: title,
            created_at: now_secs(),
            ..Default::default()
        });
    }

    /// Log `duration` on the issue or MR, spent at `spent_at`.
    fn spend(
        &mut self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        duration: &str,
        summary: &str,
        spent_at: u64,
    ) -> Result<()> {
        let time_spent = parse_duration(duration)
            .ok_or_else(|| refused(400, format!("400 Bad Request: invalid time {duration:?}")))?;
        let now = now_secs();
        let title = self.update(kind, project_id, iid, now, |_, _| {})?;
        let path = project_path(self, project_id);
        let web_url = match kind {
            Issuable::Issue => issue_url(&path, iid),
            Issuable::MergeRequest => merge_request_url(&path, iid),
        };
        let id = self.next_id();
        self.timelogs.push(Timelog {
            id,
            spent_at,
            kind,
            project_id,
            iid,
            title,
            web_url,
            time_spent,
            summary: summary.into(),
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl GitlabApi for DemoGitlab {
    async fn add_spent_time(
        &self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        duration: &str,
        summary: Option<&str>,
    ) -> Result<()> {
        let summary = summary.unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        state.spend(kind, project_id, iid, duration, summary, now_secs())
    }

    async fn create_timelog(
        &self,
        kind: Issuable,
        issuable_id: i64,
        duration: &str,
        summary: &str,
        spent_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let (project_id, iid) = match kind {
            Issuable::Issue => state
                .issues
                .iter()
                .find(|i| i.row.id == issuable_id)
                .map(|i| (i.row.project_id, i.row.iid)),
            Issuable::MergeRequest => state
                .merge_requests
                .iter()
                .find(|m| m.id == issuable_id)
                .map(|m| (m.project_id, m.iid)),
        }
        .ok_or_else(not_found)?;
        let spent_at = spent_at.timestamp().max(0) as u64;
        state.spend(kind, project_id, iid, duration, summary, spent_at)
    }

    async fn close(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let title = state.update(kind, project_id, iid, now_secs(), |s, _| {
            *s = "closed".into();
        })?;
        state.event(project_id, "closed", kind, iid, title);
        Ok(())
    }

    async fn assign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.update(kind, project_id, iid, now_secs(), |_, assignees| {
            if !assignees.iter().any(|a| a.id == USER_ID) {
                assignees.push(user(USER_ID));
            }
        })?;
        Ok(())
    }

    async fn unassign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.update(kind, project_id, iid, now_secs(), |_, assignees| {
            assignees.retain(|a| a.id != USER_ID);
        })?;
        Ok(())
    }

    async fn create_issue(&self, project_id: i64, new: &NewIssue) -> Result<Value> {
        let mut state = self.state.lock().unwrap();
        state.writable(project_id)?;
        let epic = match new.epic_id {
            None => None,
            Some(id) => {
                let epic = state
                    .epics
                    .iter()
                    .find(|e| e.id == id)
                    .ok_or_else(not_found)?;
                Some(EpicRef {
                    url: epic.web_url.clone(),
                })
            }
        };
        let iid = 1 + state
            .issues
            .iter()
            .filter(|i| i.row.project_id == project_id)
            .map(|i| i.row.iid)
            .max()
            .unwrap_or(0);
        let path = project_path(&state, project_id);
        let issue = DemoIssue {
            row: Issue {
                id: project_id * 1000 + iid,
                iid,
                project_id,
                title: new.title.clone(),
                web_url: issue_url(&path, iid),
                state: "opened".into(),
                labels: new.labels.clone(),
                assignees: if new.assign_self {
                    vec![user(USER_ID)]
                } else {
                    Vec::new()
                },
                epic,
                time_stats: None,
                updated_at: now_secs(),
            },
            author: USER_ID,
        };
        let row = state.issue_row(&issue);
        state.issues.push(issue);
        state.event(
            project_id,
            "opened",
            Issuable::Issue,
            iid,
            new.title.clone(),
        );
        Ok(serde_json::to_value(row)?)
    }

    async fn list(&self, listing: &Listing, limit: Option<usize>) -> Result<Vec<Value>> {
        let mut rows = self.state.lock().unwrap().list(listing);
        rows.truncate(limit.unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn list_timelogs(&self, since: DateTime<Utc>) -> Result<Vec<Timelog>> {
        let since = since.timestamp().max(0) as u64;
        let state = self.state.lock().unwrap();
        let mut logs: Vec<Timelog> = state
            .timelogs
            .iter()
            .filter(|t| t.spent_at >= since)
            .cloned()
            .collect();
        logs.sort_by_key(|t| std::cmp::Reverse((t.spent_at, t.id)));
        Ok(logs)
    }

    async fn project_avatar(&self, project_id: i64) -> Result<Option<Vec<u8>>> {
        Ok((project_id == AVATAR_PROJECT).then(|| AVATAR.to_vec()))
    }

    /// A token that never expires, so nothing would rotate it.
    async fn token_info(&self) -> Result<TokenInfo> {
        Ok(TokenInfo {
            scopes: vec!["api".into(), "read_user".into()],
            created_at: DateTime::from_timestamp(
                self.created_at.saturating_sub(30 * DAY) as i64,
                0,
            ),
            expires_at: None,
        })
    }

    async fn rotate_token(&self, _expires_at: Option<chrono::NaiveDate>) -> Result<RotatedToken> {
        Err(Error::Gitlab("a dry run's token is not rotated".into()))
    }
}

/// An issue of the fixture: project, iid, title, state, labels, assignees,
/// author, and how long ago it was last updated.
type IssueRow = (
    i64,
    i64,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [i64],
    i64,
    u64,
);

#[rustfmt::skip]
const ISSUES: [IssueRow; 13] = [
    (101, 12, "Rate-limit the token endpoint", "opened", &["bug", "backend", "Doing"], &[USER_ID], ALEX, 3 * HOUR),
    (101, 14, "Paginate the audit log export", "opened", &["feature", "backend"], &[USER_ID], USER_ID, DAY),
    (101, 15, "OAuth refresh fails after a password change", "opened", &["bug", "Review"], &[USER_ID], SAM, 30 * HOUR),
    (101, 9, "Remove the v1 projects endpoint", "closed", &["backend"], &[USER_ID], USER_ID, 4 * DAY),
    (101, 16, "Document the webhook retry policy", "opened", &["docs"], &[ALEX], USER_ID, 2 * DAY),
    (102, 7, "Invoice PDF shows the wrong VAT rate", "opened", &["bug", "billing", "Doing"], &[USER_ID], ALEX, 5 * HOUR),
    (102, 8, "Self-service plan downgrade", "opened", &["feature", "billing"], &[USER_ID], USER_ID, 2 * DAY),
    (102, 3, "Move the Stripe webhooks to the new signing secret", "closed", &["billing"], &[USER_ID], USER_ID, 6 * DAY),
    (102, 10, "Dunning e-mails go out twice", "opened", &["bug"], &[SAM], SAM, 3 * DAY),
    (103, 21, "Dark mode: contrast of disabled buttons", "opened", &["frontend", "a11y"], &[USER_ID], ALEX, 8 * HOUR),
    (103, 22, "Keyboard navigation in the date picker", "opened", &["frontend", "a11y", "Review"], &[], USER_ID, 2 * DAY),
    (103, 19, "Flaky end-to-end test: checkout flow", "closed", &["ci"], &[USER_ID], ALEX, 9 * DAY),
    (ARCHIVED_PROJECT, 2, "Sunset the legacy portal", "closed", &[], &[USER_ID], USER_ID, 40 * DAY),
];

/// A merge request of the fixture: project, iid, title, state, labels,
/// assignees, and how long ago it was last updated.
type MergeRequestRow = (
    i64,
    i64,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [i64],
    u64,
);

#[rustfmt::skip]
const MERGE_REQUESTS: [MergeRequestRow; 6] = [
    (101, 31, "Rate-limit /oauth/token per client", "opened", &["backend"], &[USER_ID], 2 * HOUR),
    (101, 29, "Drop the v1 projects endpoint", "merged", &["backend"], &[USER_ID], 4 * DAY),
    (102, 12, "Fix the VAT rate on invoice PDFs", "opened", &["billing"], &[USER_ID], 6 * HOUR),
    (102, 11, "Rotate the Stripe signing secret", "merged", &[], &[ALEX], 6 * DAY),
    (103, 44, "Raise the contrast of disabled buttons", "opened", &["a11y"], &[USER_ID], DAY),
    (103, 40, "Retry the checkout end-to-end test once", "closed", &["ci"], &[ALEX], 9 * DAY),
];

/// A timelog of the fixture: what it was logged on, how long ago, the
/// seconds spent and the summary.
#[rustfmt::skip]
const TIMELOGS: [(Issuable, i64, i64, u64, u64, &str); 9] = [
    (Issuable::Issue, 101, 12, 2 * HOUR, 5400, "Reproduced the burst, sketched the limiter"),
    (Issuable::Issue, 102, 7, 5 * HOUR, 3600, "Traced the rate lookup"),
    (Issuable::MergeRequest, 102, 12, 6 * HOUR, 1800, "Review fixes"),
    (Issuable::Issue, 103, 21, 26 * HOUR, 2700, "Contrast measurements"),
    (Issuable::Issue, 102, 8, 2 * DAY, 7200, "Proration rules"),
    (Issuable::Issue, 101, 14, 3 * DAY, 3600, ""),
    (Issuable::MergeRequest, 101, 29, 4 * DAY, 1800, "Review"),
    (Issuable::Issue, 102, 3, 6 * DAY, 5400, "Signing secret rollout"),
    (Issuable::Issue, 103, 19, 12 * DAY, 3600, "Bisected the flake"),
];

/// What a contribution event of the fixture is about.
#[derive(Clone, Copy)]
enum About {
    Nothing,
    /// An issue or merge request: its kind, iid and title.
    Item(&'static str, i64, &'static str),
    /// A push: the branch, the number of commits, the newest one's title.
    Push(&'static str, i64, &'static str),
    /// A comment: on what kind of item, its iid, the comment.
    Comment(&'static str, i64, &'static str),
}

/// The contribution events of the fixture, oldest first: project, action,
/// how long ago, and what about.
#[rustfmt::skip]
const EVENTS: [(i64, &str, u64, About); 12] = [
    (103, "joined", 20 * DAY, About::Nothing),
    (103, "commented on", 9 * DAY, About::Comment("MergeRequest", 40, "One retry hides the flake, it doesn't fix it.")),
    (101, "accepted", 4 * DAY, About::Item("MergeRequest", 29, "Drop the v1 projects endpoint")),
    (101, "closed", 4 * DAY, About::Item("Issue", 9, "Remove the v1 projects endpoint")),
    (103, "opened", 2 * DAY, About::Item("Issue", 22, "Keyboard navigation in the date picker")),
    (103, "pushed new", DAY, About::Push("contrast-buttons", 2, "Raise the contrast of disabled buttons")),
    (103, "opened", DAY, About::Item("MergeRequest", 44, "Raise the contrast of disabled buttons")),
    (102, "pushed to", 6 * HOUR, About::Push("fix-vat-rate", 1, "Take the VAT rate from the customer's country")),
    (102, "opened", 6 * HOUR, About::Item("MergeRequest", 12, "Fix the VAT rate on invoice PDFs")),
    (102, "commented on", 5 * HOUR, About::Comment("Issue", 7, "The rate comes from the customer's country, not ours.")),
    (101, "pushed to", 2 * HOUR, About::Push("rate-limit-token", 3, "Rate-limit /oauth/token per client")),
    (101, "opened", 2 * HOUR, About::Item("MergeRequest", 31, "Rate-limit /oauth/token per client")),
];

/// The demo account as of `now`.
fn fixture(now: u64) -> State {
    let ago = |secs: u64| now.saturating_sub(secs);

    let group = |id: i64, name: &str, full_path: &str| Group {
        id,
        name: name.into(),
        full_path: full_path.into(),
        web_url: format!("{BASE}/groups/{full_path}"),
    };
    let groups = vec![
        group(10, "Acme", "acme"),
        group(11, "Backend", "acme/backend"),
        group(12, "Frontend", "acme/frontend"),
    ];

    let project = |id: i64, name: &str, path: &str| Project {
        id,
        name: name.into(),
        path_with_namespace: path.into(),
        web_url: format!("{BASE}/{path}"),
        avatar_url: String::new(),
        archived: false,
        issues_access_level: "enabled".into(),
        merge_requests_access_level: "enabled".into(),
        repository_access_level: "enabled".into(),
        issues_enabled: Some(true),
        merge_requests_enabled: Some(true),
    };
    let mut api = project(AVATAR_PROJECT, "API", "acme/backend/api");
    api.avatar_url = format!("{BASE}/uploads/-/system/project/avatar/{AVATAR_PROJECT}/api.png");
    let mut legacy = project(ARCHIVED_PROJECT, "Legacy Portal", "acme/legacy-portal");
    legacy.archived = true;
    let projects = vec![
        api,
        project(102, "Billing", "acme/backend/billing"),
        project(103, "Web", "acme/frontend/web"),
        legacy,
    ];
    let path_of = |project_id: i64| {
        projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.path_with_namespace.clone())
            .unwrap_or_default()
    };

    let epic =
        |id: i64, group_id: i64, group_path: &str, iid: i64, title: &str, state: &str| Epic {
            id,
            iid,
            group_id,
            title: title.into(),
            web_url: epic_url(group_path, iid),
            state: state.into(),
            labels: vec!["roadmap".into()],
            updated_at: ago(2 * DAY),
        };
    let epics = vec![
        epic(3001, 10, "acme", 1, "Self-service billing", "opened"),
        epic(
            3002,
            12,
            "acme/frontend",
            1,
            "Accessibility audit",
            "closed",
        ),
    ];
    let billing_epic = EpicRef {
        url: epics[0].web_url.clone(),
    };

    let issues = ISSUES
        .iter()
        .map(
            |&(project_id, iid, title, state, labels, assignees, author, age)| DemoIssue {
                row: Issue {
                    id: project_id * 1000 + iid,
                    iid,
                    project_id,
                    title: title.into(),
                    web_url: issue_url(&path_of(project_id), iid),
                    state: state.into(),
                    labels: labels.iter().map(|l| l.to_string()).collect(),
                    assignees: assignees.iter().map(|&a| user(a)).collect(),
                    // The billing epic holds the billing features.
                    epic: (project_id == 102 && labels.contains(&"feature"))
                        .then(|| billing_epic.clone()),
                    time_stats: None,
                    updated_at: ago(age),
                },
                author,
            },
        )
        .collect();

    let merge_requests = MERGE_REQUESTS
        .iter()
        .map(
            |&(project_id, iid, title, state, labels, assignees, age)| MergeRequest {
                id: 1_000_000 + project_id * 1000 + iid,
                iid,
                project_id,
                title: title.into(),
                web_url: merge_request_url(&path_of(project_id), iid),
                state: state.into(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                assignees: assignees.iter().map(|&a| user(a)).collect(),
                updated_at: ago(age),
            },
        )
        .collect();

    let board = |id: i64, project_id: i64, labels: &[Option<&str>]| Board {
        id,
        project_id,
        lists: labels
            .iter()
            .map(|l| BoardList {
                label: l.map(|name| LabelRef { name: name.into() }),
            })
            .collect(),
    };
    let boards = vec![
        // With GitLab's label-less backlog and closed lists.
        board(501, 101, &[None, Some("Doing"), Some("Review"), None]),
        board(502, 102, &[Some("Doing"), Some("Review")]),
        board(503, 103, &[Some("Doing"), Some("Review")]),
    ];

    let events = EVENTS
        .iter()
        .enumerate()
        .map(|(n, &(project_id, action, age, what))| {
            let mut e = Event {
                id: 9001 + n as i64,
                project_id,
                action_name: action.into(),
                created_at: ago(age),
                ..Default::default()
            };
            match what {
                About::Nothing => {}
                About::Item(kind, iid, title) => {
                    e.target_type = kind.into();
                    e.target_iid = iid;
                    e.target_title = title.into();
                }
                About::Push(git_ref, commit_count, commit_title) => {
                    e.push_data = PushData {
                        git_ref: git_ref.into(),
                        commit_count,
                        commit_title: commit_title.into(),
                    };
                }
                About::Comment(noteable_type, noteable_iid, body) => {
                    e.target_type = "Note".into();
                    e.note = NoteRef {
                        body: body.into(),
                        noteable_type: noteable_type.into(),
                        noteable_iid,
                    };
                }
            }
            e
        })
        .collect();

    let timelogs = TIMELOGS
        .iter()
        .enumerate()
        .map(|(n, &(kind, project_id, iid, age, time_spent, summary))| {
            let path = path_of(project_id);
            let (title, web_url) = match kind {
                Issuable::Issue => {
                    let row = ISSUES.iter().find(|r| r.0 == project_id && r.1 == iid);
                    (row.map(|r| r.2), issue_url(&path, iid))
                }
                Issuable::MergeRequest => {
                    let row = MERGE_REQUESTS
                        .iter()
                        .find(|r| r.0 == project_id && r.1 == iid);
                    (row.map(|r| r.2), merge_request_url(&path, iid))
                }
            };
            Timelog {
                id: 7001 + n as u64,
                spent_at: ago(age),
                kind,
                project_id,
                iid,
                title: title.unwrap_or_default().into(),
                web_url,
                time_spent,
                summary: summary.into(),
            }
        })
        .collect();

    State {
        groups,
        projects,
        issues,
        merge_requests,
        epics,
        boards,
        events,
        timelogs,
        // Above every fixture id: new timelogs and events follow it.
        next_id: 20_000,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::sync::model::{self, Resource};

    /// The fixture as of now: the writes date their changes by the clock.
    fn demo() -> (DemoGitlab, u64) {
        let now = now_secs();
        (DemoGitlab::new(now), now)
    }

    async fn issues(demo: &DemoGitlab, listing: Listing) -> Vec<model::Issue> {
        let rows = demo.list(&listing, None).await.unwrap();
        rows.into_iter()
            .map(|r| serde_json::from_value(r).unwrap())
            .collect()
    }

    /// The `(project, iid)` of `issues`, in their order.
    fn keys(issues: &[model::Issue]) -> Vec<(i64, i64)> {
        issues.iter().map(|i| (i.project_id, i.iid)).collect()
    }

    fn key_set(issues: &[model::Issue]) -> BTreeSet<(i64, i64)> {
        keys(issues).into_iter().collect()
    }

    fn at(secs: u64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs as i64, 0).unwrap()
    }

    #[test]
    fn durations_read_like_gitlab_reads_them() {
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1h 30m"), Some(5400));
        assert_eq!(parse_duration("1.5h"), Some(5400));
        assert_eq!(parse_duration("2d"), Some(16 * HOUR));
        assert_eq!(parse_duration("1w"), Some(40 * HOUR));
        assert_eq!(parse_duration("1mo"), Some(160 * HOUR));
        assert_eq!(
            parse_duration("2"),
            Some(2 * HOUR),
            "a bare number is hours"
        );
        for refused in ["", "abc", "1x", "0m", "h"] {
            assert_eq!(parse_duration(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn time_spent_is_spelled_like_gitlab_spells_it() {
        assert_eq!(human(5400), "1h 30m");
        assert_eq!(human(9 * HOUR), "1d 1h");
        assert_eq!(human(40 * HOUR + 60), "1w 1m");
        assert_eq!(human(30), "30s");
    }

    #[test]
    fn every_link_is_on_the_invalid_host() {
        let state = fixture(now_secs());
        let urls = state
            .issues
            .iter()
            .map(|i| &i.row.web_url)
            .chain(state.merge_requests.iter().map(|m| &m.web_url))
            .chain(
                state
                    .projects
                    .iter()
                    .flat_map(|p| [&p.web_url, &p.avatar_url]),
            )
            .chain(state.groups.iter().map(|g| &g.web_url))
            .chain(state.epics.iter().map(|e| &e.web_url))
            .chain(state.timelogs.iter().map(|t| &t.web_url))
            .filter(|u| !u.is_empty());
        for url in urls {
            assert!(url.starts_with("https://dry-run.invalid/"), "{url}");
        }
    }

    #[test]
    fn the_fixture_rows_are_all_valid() {
        let state = fixture(now_secs());
        assert!(state.issues.iter().all(|i| i.row.is_valid()));
        assert!(state.merge_requests.iter().all(Resource::is_valid));
        assert!(state.projects.iter().all(Resource::is_valid));
        assert!(state.groups.iter().all(Resource::is_valid));
        assert!(state.epics.iter().all(Resource::is_valid));
        assert!(state.boards.iter().all(Resource::is_valid));
        assert!(state.events.iter().all(Resource::is_valid));
        assert!(state.timelogs.iter().all(Resource::is_valid));
        assert!(state.timelogs.iter().all(|t| !t.title.is_empty()));
        let ids: std::collections::HashSet<i64> = state.events.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), state.events.len(), "event ids are unique");
    }

    #[tokio::test]
    async fn the_assigned_lists_hold_my_open_items_only() {
        let (demo, _) = demo();
        let assigned = issues(&demo, Listing::AssignedIssues).await;
        assert_eq!(assigned.len(), 6);
        for i in &assigned {
            assert_eq!(i.state, "opened");
            assert!(i.assignees.iter().any(|a| a.id == USER_ID));
        }
        let mrs = demo
            .list(&Listing::AssignedMergeRequests, None)
            .await
            .unwrap();
        let mrs: Vec<model::MergeRequest> = mrs
            .into_iter()
            .map(|r| serde_json::from_value(r).unwrap())
            .collect();
        assert_eq!(
            mrs.iter().map(|m| m.iid).collect::<Vec<_>>(),
            [31, 12, 44],
            "newest first"
        );
    }

    #[tokio::test]
    async fn listings_filter_by_project_time_and_role() {
        let (demo, now) = demo();
        let recent = Listing::ProjectIssues {
            project_id: 101,
            updated_after: Some(at(now - 2 * DAY)),
        };
        let recent = issues(&demo, recent).await;
        assert_eq!(
            keys(&recent),
            [(101, 12), (101, 14), (101, 15), (101, 16)],
            "newest first"
        );

        let window = at(now - 5 * DAY);
        let authored = Listing::RecentAuthoredIssues {
            updated_after: window,
        };
        let authored = issues(&demo, authored).await;
        let expected = [(101, 9), (101, 14), (101, 16), (102, 8), (103, 22)];
        assert_eq!(key_set(&authored), BTreeSet::from(expected));

        let assigned = Listing::RecentAssignedIssues {
            updated_after: window,
        };
        let assigned = key_set(&issues(&demo, assigned).await);
        assert!(assigned.contains(&(101, 9)), "closed ones too");
        assert!(!assigned.contains(&(103, 22)), "unassigned");
        assert!(!assigned.contains(&(102, 3)), "updated before the window");

        let one = Listing::Issuable {
            kind: Issuable::Issue,
            project_id: 102,
            iid: 7,
        };
        let one = issues(&demo, one).await;
        assert_eq!(one[0].id, 102_007);
        assert_eq!(one[0].parent_url(), "", "a bug is in no epic");
        let featured = issues(
            &demo,
            Listing::Issuable {
                kind: Issuable::Issue,
                project_id: 102,
                iid: 8,
            },
        )
        .await;
        assert_eq!(
            featured[0].parent_url(),
            "https://dry-run.invalid/groups/acme/-/epics/1"
        );
        assert_eq!(featured[0].total_time(), "2h");

        let limited = demo
            .list(
                &Listing::AllIssues {
                    updated_after: None,
                },
                Some(3),
            )
            .await
            .unwrap();
        assert_eq!(limited.len(), 3);
    }

    #[tokio::test]
    async fn events_come_oldest_first_after_a_date() {
        let (demo, now) = demo();
        let all = demo
            .list(&Listing::Events { after: None }, None)
            .await
            .unwrap();
        let all: Vec<Event> = all
            .into_iter()
            .map(|r| serde_json::from_value(r).unwrap())
            .collect();
        assert!(all.windows(2).all(|w| w[0].created_at <= w[1].created_at));
        let after = at(now - 3 * DAY).date_naive();
        let recent = demo
            .list(&Listing::Events { after: Some(after) }, None)
            .await
            .unwrap();
        assert!(recent.len() < all.len() && !recent.is_empty());
    }

    #[tokio::test]
    async fn writes_change_the_fixture() {
        let (demo, now) = demo();
        demo.close(Issuable::Issue, 101, 12).await.unwrap();
        demo.unassign_self(Issuable::MergeRequest, 101, 31)
            .await
            .unwrap();
        demo.assign_self(Issuable::Issue, 103, 22).await.unwrap();
        let assigned = keys(&issues(&demo, Listing::AssignedIssues).await);
        assert!(!assigned.contains(&(101, 12)), "closed");
        assert!(assigned.contains(&(103, 22)), "assigned");
        let mrs = demo
            .list(&Listing::AssignedMergeRequests, None)
            .await
            .unwrap();
        assert_eq!(mrs.len(), 2);

        demo.add_spent_time(Issuable::Issue, 103, 22, "1h30m", Some("Pairing"))
            .await
            .unwrap();
        let logs = demo.list_timelogs(at(now - HOUR)).await.unwrap();
        assert_eq!(logs[0].iid, 22);
        assert_eq!(logs[0].time_spent, 5400);
        assert_eq!(logs[0].summary, "Pairing");
        let spent = Listing::Issuable {
            kind: Issuable::Issue,
            project_id: 103,
            iid: 22,
        };
        assert_eq!(issues(&demo, spent).await[0].total_time(), "1h 30m");

        let new = NewIssue {
            title: "Try the dry run".into(),
            labels: vec!["demo".into()],
            assign_self: true,
            epic_id: Some(3001),
            ..Default::default()
        };
        let created: model::Issue =
            serde_json::from_value(demo.create_issue(102, &new).await.unwrap()).unwrap();
        assert_eq!((created.project_id, created.iid), (102, 11));
        assert_eq!(
            created.web_url,
            "https://dry-run.invalid/acme/backend/billing/-/issues/11"
        );
        assert_eq!(
            created.parent_url(),
            "https://dry-run.invalid/groups/acme/-/epics/1"
        );
        let authored = Listing::RecentAuthoredIssues {
            updated_after: at(now),
        };
        let authored = issues(&demo, authored).await;
        assert!(keys(&authored).contains(&(102, 11)));
        let events = demo
            .list(&Listing::Events { after: None }, None)
            .await
            .unwrap();
        let actions: Vec<String> = events
            .iter()
            .rev()
            .take(2)
            .map(|e| e["action_name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions, ["opened", "closed"]);
    }

    #[tokio::test]
    async fn gitlab_refusals_are_kept() {
        let (demo, _) = demo();
        let archived = demo.close(Issuable::Issue, ARCHIVED_PROJECT, 2).await;
        assert!(
            matches!(archived, Err(Error::Rejected { status: 403, detail }) if detail.starts_with("403"))
        );
        assert!(matches!(
            demo.close(Issuable::Issue, 101, 999).await,
            Err(Error::Rejected { status: 404, detail }) if detail.starts_with("404")
        ));
        let bad = demo
            .add_spent_time(Issuable::Issue, 101, 12, "soon", None)
            .await;
        assert!(
            matches!(bad, Err(Error::Rejected { status: 400, detail }) if detail.starts_with("400"))
        );
        assert!(demo.rotate_token(None).await.is_err());
        let token = demo.token_info().await.unwrap();
        assert_eq!(token.expires_at, None);
    }

    #[tokio::test]
    async fn only_the_avatar_project_has_an_avatar() {
        let (demo, _) = demo();
        let image = demo.project_avatar(AVATAR_PROJECT).await.unwrap().unwrap();
        assert_eq!(crate::sync::avatars::extension(&image), Some("png"));
        assert_eq!(demo.project_avatar(102).await.unwrap(), None);
    }

    #[test]
    fn the_config_never_comes_from_a_file_and_turns_the_supervisors_off() {
        let c = config();
        assert_eq!(c.server.socket, None);
        assert_eq!(c.auth.rotate, RotatePolicy::Never);
        assert!(!c.reconnect.enabled);
        assert_eq!(c.sync.startup_spread_secs, 0);
    }
}
