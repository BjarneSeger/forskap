//! GitLab API access — the only module that knows about the `gitlab` crate.
//!
//! Wraps `gitlab::AsyncGitlab`: one paginated endpoint for every read
//! ([`Listing`]), the GraphQL timelog query, and the write endpoints the crate
//! doesn't ship (`add_spent_time`, `close`, assignment).

use std::borrow::Cow;
use std::future::Future;
use std::time::Duration;

use gitlab::api::{AsyncQuery, UrlBase};
use tracing::{info, instrument, warn};

use crate::error::{Error, Result};
use crate::sync::model::Timelog;

/// Which GitLab issuable an operation targets. Internal counterpart of the
/// wire `IssuableKind`, kept separate so persisted queue/history records
/// don't couple the on-disk format to the api crate; `Default = Issue`
/// because every record written before MR support was an issue, which lets
/// them deserialize via `#[serde(default)]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Issuable {
    #[default]
    Issue,
    MergeRequest,
}

impl Issuable {
    /// REST URL path segment: `projects/{id}/<segment>/{iid}/…`.
    pub fn path_segment(self) -> &'static str {
        match self {
            Issuable::Issue => "issues",
            Issuable::MergeRequest => "merge_requests",
        }
    }

    /// GraphQL global-ID type: `gid://gitlab/<type>/{id}`.
    pub fn gid_type(self) -> &'static str {
        match self {
            Issuable::Issue => "Issue",
            Issuable::MergeRequest => "MergeRequest",
        }
    }
}

pub struct GitlabClient {
    inner: gitlab::AsyncGitlab,
    /// GitLab host (e.g. `"gitlab.com"`) used to build `inner`. Exposed so
    /// `WhoAmI` can return it.
    host: String,
    /// Numeric ID of the authenticated user, fetched once at `connect()`.
    /// Required so `assign_self`/`unassign_self` can mutate the issuable's
    /// `assignee_ids` list without an extra round-trip per call.
    current_user_id: i64,
}

impl GitlabClient {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn current_user_id(&self) -> i64 {
        self.current_user_id
    }
}

/// A paginated REST listing the sync layer fetches. Path and query are
/// rendered in one place ([`Listing::path`], [`Listing::params`]), so test
/// fakes match on the variant instead of on strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// Open issues assigned to the user.
    AssignedIssues,
    /// Open merge requests assigned to the user.
    AssignedMergeRequests,
    /// Every issue of a project, all states; `updated_after` for a delta.
    ProjectIssues {
        project_id: i64,
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
    },
    ProjectMergeRequests {
        project_id: i64,
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// Every issue the token can see (`scope=all`).
    AllIssues {
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
    },
    AllMergeRequests {
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// Projects the user is a member of.
    MemberProjects,
    /// Groups the user is a member of (a bare `GET /groups` would include
    /// public non-member groups).
    MemberGroups,
    /// A project's boards, lists embedded.
    ProjectBoards { project_id: i64 },
    /// The user's own contribution events created after `after` (a date;
    /// GitLab compares exclusively).
    Events { after: Option<chrono::NaiveDate> },
    /// One issue or MR by its iid, for its global id.
    Issuable {
        kind: Issuable,
        project_id: i64,
        iid: i64,
    },
}

impl Listing {
    pub fn path(&self) -> String {
        match self {
            Self::AssignedIssues | Self::AllIssues { .. } => "issues".into(),
            Self::AssignedMergeRequests | Self::AllMergeRequests { .. } => "merge_requests".into(),
            Self::ProjectIssues { project_id, .. } => format!("projects/{project_id}/issues"),
            Self::ProjectMergeRequests { project_id, .. } => {
                format!("projects/{project_id}/merge_requests")
            }
            Self::MemberProjects => "projects".into(),
            Self::MemberGroups => "groups".into(),
            Self::ProjectBoards { project_id } => format!("projects/{project_id}/boards"),
            Self::Events { .. } => "events".into(),
            Self::Issuable {
                kind, project_id, ..
            } => format!("projects/{project_id}/{}", kind.path_segment()),
        }
    }

    pub fn params(&self) -> Vec<(&'static str, String)> {
        let after = |t: &Option<chrono::DateTime<chrono::Utc>>| {
            t.map(|t| {
                (
                    "updated_after",
                    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                )
            })
        };
        let mut params = match self {
            Self::AssignedIssues | Self::AssignedMergeRequests => vec![
                ("scope", "assigned_to_me".into()),
                ("state", "opened".into()),
            ],
            // Newest first, so a capped fetch keeps the most recent items.
            Self::ProjectIssues { updated_after, .. }
            | Self::ProjectMergeRequests { updated_after, .. } => {
                let mut p = vec![("order_by", "updated_at".into()), ("sort", "desc".into())];
                p.extend(after(updated_after));
                p
            }
            Self::AllIssues { updated_after } | Self::AllMergeRequests { updated_after } => {
                let mut p = vec![("scope", "all".into())];
                p.extend(after(updated_after));
                p
            }
            Self::MemberProjects => vec![("membership", "true".into()), ("simple", "true".into())],
            // 10 = Guest, the lowest membership level.
            Self::MemberGroups => vec![("min_access_level", "10".into())],
            Self::ProjectBoards { .. } => Vec::new(),
            Self::Events { after } => after
                .map(|d| ("after", d.format("%Y-%m-%d").to_string()))
                .into_iter()
                .collect(),
            Self::Issuable { iid, .. } => vec![("iids[]", iid.to_string())],
        };
        params.sort();
        params
    }
}

/// Daemon-facing GitLab surface. Lets tests substitute a fake without touching
/// the real `gitlab` crate. Production code path goes through the impl on
/// [`GitlabClient`].
#[async_trait::async_trait]
pub trait GitlabApi: Send + Sync {
    async fn add_spent_time(
        &self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        duration: &str,
        summary: Option<&str>,
    ) -> Result<()>;

    async fn create_timelog(
        &self,
        kind: Issuable,
        issuable_id: i64,
        duration: &str,
        summary: &str,
        spent_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    async fn close(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()>;

    async fn assign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()>;

    async fn unassign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()>;

    /// The rows of a paginated REST listing, as raw JSON; paging stops once
    /// `limit` rows arrived.
    async fn list(&self, listing: &Listing, limit: Option<usize>)
    -> Result<Vec<serde_json::Value>>;

    /// The user's timelogs with `spent_at >= since`, newest first. GitLab has
    /// no REST listing for them, so this is the one GraphQL read.
    async fn list_timelogs(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<Timelog>>;
}

impl GitlabClient {
    /// Read the issuable's current `assignee_ids`, apply the add/remove, and
    /// PUT the new list back. Skips the PUT when the issuable is already in
    /// the target state (`add && already assigned` or `!add && not assigned`).
    async fn mutate_self_assignment(
        &self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        add: bool,
    ) -> Result<()> {
        let raw: serde_json::Value = GetIssuableEndpoint {
            kind,
            project_id,
            iid,
        }
        .query_async(&self.inner)
        .await
        .map_err(classify)?;

        let current: Vec<i64> = raw["assignees"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|a| a["id"].as_i64()).collect())
            .unwrap_or_default();

        let Some(new_ids) = compute_new_assignees(&current, self.current_user_id, add) else {
            return Ok(());
        };

        use gitlab::api::ignore;
        ignore(UpdateAssigneesEndpoint {
            kind,
            project_id,
            iid,
            assignee_ids: new_ids,
        })
        .query_async(&self.inner)
        .await
        .map_err(classify)?;
        Ok(())
    }

    pub async fn connect(host: &str, token: &str) -> Result<Self> {
        let inner = gitlab::GitlabBuilder::new(host.to_string(), token.to_string())
            .build_async()
            .await
            .map_err(classify_build)?;

        let user: serde_json::Value = CurrentUserEndpoint
            .query_async(&inner)
            .await
            .map_err(classify)?;
        let current_user_id = user["id"].as_i64().ok_or_else(|| {
            Error::Gitlab(format!("GET /user response missing numeric id: {user}"))
        })?;
        info!(current_user_id, "resolved authenticated GitLab user");

        Ok(Self {
            inner,
            host: host.to_string(),
            current_user_id,
        })
    }

    /// Like [`GitlabClient::connect`], but retries transient (network) failures
    /// with bounded exponential back-off (see [`retry_transient`]). A permanent
    /// rejection (a bad token) fails immediately without retrying. Used by the
    /// interactive `Login` handler so a momentary blip doesn't fail the command.
    pub async fn connect_with_retry(host: &str, token: &str) -> Result<Self> {
        retry_transient("login connect", || Self::connect(host, token)).await
    }
}

/// Run `op` with bounded exponential back-off on transient (network) failures:
/// up to four attempts, sleeping 1 s → 2 s → 4 s between them. A permanent error
/// returns immediately. Shared by `connect_with_retry` and `run_issues_query`.
///
/// Takes a future *factory* (`FnMut() -> Future`) rather than an async closure so
/// the produced future has a single concrete type — an `impl AsyncFnMut` here
/// yields a per-call future whose `Send`-ness isn't general enough for the
/// `#[instrument]` callers.
pub(crate) async fn retry_transient<T, Fut>(op: &str, mut f: impl FnMut() -> Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let mut delay = Duration::from_secs(1);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e @ Error::Transient(_)) if attempt < 4 => {
                warn!(attempt, error = %e, delay_secs = delay.as_secs(), op, "transient failure, retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(4));
            }
            Err(e) => return Err(e),
        }
    }
}

#[async_trait::async_trait]
impl GitlabApi for GitlabClient {
    /// Record time spent on a GitLab issue or merge request.
    #[instrument(skip(self))]
    async fn add_spent_time(
        &self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        duration: &str,
        summary: Option<&str>,
    ) -> Result<()> {
        use gitlab::api::ignore;

        ignore(AddSpentTime {
            kind,
            project_id,
            iid,
            duration,
            summary,
        })
        .query_async(&self.inner)
        .await
        .map_err(classify)?;
        Ok(())
    }

    /// Record time spent on an issuable via the GraphQL `timelogCreate` mutation,
    /// stamping it at `spent_at` instead of "now". Used by the retry queue so a
    /// task that was queued during an outage appears in GitLab at the time the
    /// user actually logged it, not the time we reconnected.
    #[instrument(skip(self))]
    async fn create_timelog(
        &self,
        kind: Issuable,
        issuable_id: i64,
        duration: &str,
        summary: &str,
        spent_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let endpoint = TimelogCreate {
            issuable_id: format!("gid://gitlab/{}/{issuable_id}", kind.gid_type()),
            time_spent: duration,
            summary,
            spent_at: spent_at.to_rfc3339(),
        };

        let raw: serde_json::Value = endpoint.query_async(&self.inner).await.map_err(classify)?;

        if let Some(errs) = raw["errors"].as_array()
            && !errs.is_empty()
        {
            let msg = errs
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Gitlab(format!("timelogCreate: {msg}")));
        }

        if let Some(errs) = raw["data"]["timelogCreate"]["errors"].as_array()
            && !errs.is_empty()
        {
            let msg = errs
                .iter()
                .filter_map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Gitlab(format!("timelogCreate: {msg}")));
        }

        Ok(())
    }

    /// Close a GitLab issuable (`PUT /projects/:id/<kind>/:iid` with
    /// `state_event=close`).
    #[instrument(skip(self))]
    async fn close(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        use gitlab::api::ignore;

        ignore(CloseEndpoint {
            kind,
            project_id,
            iid,
        })
        .query_async(&self.inner)
        .await
        .map_err(classify)?;
        Ok(())
    }

    /// Add the authenticated user to the issuable's `assignee_ids` list.
    #[instrument(skip(self))]
    async fn assign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        self.mutate_self_assignment(kind, project_id, iid, true)
            .await
    }

    /// Remove the authenticated user from the issuable's `assignee_ids` list.
    #[instrument(skip(self))]
    async fn unassign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        self.mutate_self_assignment(kind, project_id, iid, false)
            .await
    }

    #[instrument(skip(self))]
    async fn list(
        &self,
        listing: &Listing,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>> {
        use gitlab::api::{Pagination, paged};
        let pagination = limit.map_or(Pagination::All, Pagination::Limit);
        let mut rows =
            run_paged_query(&self.inner, "list", paged(RestList(listing), pagination)).await?;
        // The crate stops after the page that reached the limit, not at it.
        rows.truncate(limit.unwrap_or(usize::MAX));
        Ok(rows)
    }

    /// Returns entries with `spent_at >= since`, newest first. Catches time
    /// logged via the web UI or other clients.
    #[instrument(skip(self))]
    async fn list_timelogs(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<Timelog>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let endpoint = MyTimelogs {
                start_time: since.to_rfc3339(),
                after: after.clone(),
            };
            // A read is idempotent, so a momentary network blip is absorbed
            // here instead of demoting the whole session.
            let raw: serde_json::Value = retry_transient("fetch timelogs", || async {
                endpoint.query_async(&self.inner).await.map_err(classify)
            })
            .await?;
            let (page, next) = timelogs_page(&raw)?;
            out.extend(page);
            match next {
                // A repeated cursor would loop forever.
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => break,
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.spent_at));
        info!(count = out.len(), "fetched timelogs from GitLab");
        Ok(out)
    }
}

/// `GET <listing path>` — the one endpoint behind every [`Listing`].
struct RestList<'a>(&'a Listing);

impl gitlab::api::Endpoint for RestList<'_> {
    fn method(&self) -> http::Method {
        http::Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        self.0.path().into()
    }

    fn parameters(&self) -> gitlab::api::QueryParams<'_> {
        let mut params = gitlab::api::QueryParams::default();
        for (k, v) in self.0.params() {
            params.push(k, v);
        }
        params
    }
}

impl gitlab::api::Pageable for RestList<'_> {}

/// Run a paged list `query` against `client` into raw JSON, retrying transient
/// errors with exponential back-off (see [`retry_transient`]).
async fn run_paged_query<Q>(
    client: &gitlab::AsyncGitlab,
    op: &str,
    query: Q,
) -> Result<Vec<serde_json::Value>>
where
    Q: gitlab::api::AsyncQuery<Vec<serde_json::Value>, gitlab::AsyncGitlab> + Sync,
{
    retry_transient(op, || async {
        query.query_async(client).await.map_err(classify)
    })
    .await
}

/// Map a GitLab API error to [`Error::Transient`] for network failures,
/// [`Error::Throttled`] for 429/5xx, and [`Error::Gitlab`] for permanent
/// rejections (auth, other 4xx, bad JSON, …).
fn classify<E>(e: gitlab::api::ApiError<E>) -> Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    use gitlab::api::ApiError as A;
    let detail = e.to_string();
    let (status, retry_after) = match &e {
        A::Client { .. } => return Error::Transient(detail),
        // Only the single-request path parses the rate-limit headers; the
        // paged path reports a 429 as one of the plain status variants.
        A::GitlabRateLimited { retry_after, .. } => (429, Some(*retry_after)),
        A::GitlabService { status, .. }
        | A::GitlabWithStatus { status, .. }
        | A::GitlabObjectWithStatus { status, .. }
        | A::GitlabUnrecognizedWithStatus { status, .. } => (status.as_u16(), None),
        _ => return Error::Gitlab(detail),
    };
    throttled_or_rejected(status, retry_after, detail)
}

/// [`Error::Throttled`] for 429/5xx, [`Error::Unauthorized`] for 401,
/// [`Error::Gitlab`] for any other status.
fn throttled_or_rejected(status: u16, retry_after: Option<Duration>, detail: String) -> Error {
    if status == 401 {
        Error::Unauthorized(detail)
    } else if status == 429 || (500..600).contains(&status) {
        Error::Throttled {
            status,
            retry_after: retry_after.filter(|d| !d.is_zero()),
            detail,
        }
    } else {
        Error::Gitlab(detail)
    }
}

/// Same split as [`classify`], but for the [`gitlab::GitlabError`] returned by
/// `GitlabBuilder::build_async`. The builder runs an initial connection check,
/// so an unreachable host must surface as [`Error::Transient`] (retryable) and
/// not a permanent [`Error::Gitlab`] — otherwise `connect` reports a network
/// outage as a rejected token.
fn classify_build(e: gitlab::GitlabError) -> Error {
    use gitlab::GitlabError;
    match e {
        // The connection check goes through the REST client, so route its
        // ApiError through the same classifier as every other call.
        GitlabError::Api { source } => classify(source),
        // Transport failure or an empty reply — network-level, safe to retry.
        e @ (GitlabError::Communication { .. } | GitlabError::NoResponse { .. }) => {
            Error::Transient(e.to_string())
        }
        GitlabError::Http { status } => {
            throttled_or_rejected(status.as_u16(), None, format!("HTTP {status}"))
        }
        // URL/auth-header/GraphQL/JSON failures are permanent.
        other => Error::Gitlab(other.to_string()),
    }
}

/// `POST /projects/:project_id/<kind>/:iid/add_spent_time`
struct AddSpentTime<'a> {
    kind: Issuable,
    project_id: i64,
    iid: i64,
    duration: &'a str,
    summary: Option<&'a str>,
}

impl gitlab::api::Endpoint for AddSpentTime<'_> {
    fn method(&self) -> http::Method {
        http::Method::POST
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!(
            "projects/{}/{}/{}/add_spent_time",
            self.project_id,
            self.kind.path_segment(),
            self.iid
        )
        .into()
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let mut body = serde_json::json!({"duration": self.duration});
        if let Some(summary) = self.summary {
            body["summary"] = serde_json::Value::String(summary.to_owned());
        }
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// `POST /api/graphql` for `Mutation.timelogCreate`.
///
/// Hits the GraphQL endpoint instead of the REST `add_spent_time` because the
/// latter has no `spent_at` parameter — GitLab stamps it as "now" on receipt,
/// which is wrong for tasks the retry queue has been sitting on.
struct TimelogCreate<'a> {
    issuable_id: String,
    time_spent: &'a str,
    summary: &'a str,
    spent_at: String,
}

impl gitlab::api::Endpoint for TimelogCreate<'_> {
    fn method(&self) -> http::Method {
        http::Method::POST
    }

    fn endpoint(&self) -> Cow<'static, str> {
        "api/graphql".into()
    }

    fn url_base(&self) -> UrlBase {
        UrlBase::Instance
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let body = serde_json::json!({
            "query": r#"
                mutation($id: IssuableID!, $time: String!, $summary: String!, $spent: Time) {
                    timelogCreate(input: { issuableId: $id, timeSpent: $time, summary: $summary, spentAt: $spent }) {
                        errors
                    }
                }
            "#,
            "variables": {
                "id": self.issuable_id,
                "time": self.time_spent,
                "summary": self.summary,
                "spent": self.spent_at,
            },
        });
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// `GET /user` — returns the authenticated user's profile. Only `id` is used.
struct CurrentUserEndpoint;

impl gitlab::api::Endpoint for CurrentUserEndpoint {
    fn method(&self) -> http::Method {
        http::Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        "user".into()
    }
}

/// `GET /projects/:project_id/<kind>/:iid`. Used to read the existing
/// assignee list before mutating it.
struct GetIssuableEndpoint {
    kind: Issuable,
    project_id: i64,
    iid: i64,
}

impl gitlab::api::Endpoint for GetIssuableEndpoint {
    fn method(&self) -> http::Method {
        http::Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!(
            "projects/{}/{}/{}",
            self.project_id,
            self.kind.path_segment(),
            self.iid
        )
        .into()
    }
}

/// `PUT /projects/:project_id/<kind>/:iid` with `assignee_ids=[...]`.
struct UpdateAssigneesEndpoint {
    kind: Issuable,
    project_id: i64,
    iid: i64,
    assignee_ids: Vec<i64>,
}

impl gitlab::api::Endpoint for UpdateAssigneesEndpoint {
    fn method(&self) -> http::Method {
        http::Method::PUT
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!(
            "projects/{}/{}/{}",
            self.project_id,
            self.kind.path_segment(),
            self.iid
        )
        .into()
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let body = serde_json::json!({"assignee_ids": self.assignee_ids});
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// Compute the new assignee list when adding (`add=true`) or removing
/// (`add=false`) `self_id`. Returns `None` when no change is needed — the
/// caller can skip the PUT entirely.
fn compute_new_assignees(current: &[i64], self_id: i64, add: bool) -> Option<Vec<i64>> {
    let already = current.contains(&self_id);
    if add == already {
        return None;
    }
    if add {
        let mut out = current.to_vec();
        out.push(self_id);
        Some(out)
    } else {
        Some(
            current
                .iter()
                .copied()
                .filter(|id| *id != self_id)
                .collect(),
        )
    }
}

/// `PUT /projects/:project_id/<kind>/:iid` with `state_event=close`.
struct CloseEndpoint {
    kind: Issuable,
    project_id: i64,
    iid: i64,
}

impl gitlab::api::Endpoint for CloseEndpoint {
    fn method(&self) -> http::Method {
        http::Method::PUT
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!(
            "projects/{}/{}/{}",
            self.project_id,
            self.kind.path_segment(),
            self.iid
        )
        .into()
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let body = serde_json::json!({"state_event": "close"});
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// `POST /api/graphql` for `currentUser.timelogs`.
///
/// Pulls the authenticated user's timelogs since `start_time`. Used by the
/// history refresh cycle so entries logged outside the daemon (web UI, other
/// clients) still show up.
struct MyTimelogs {
    start_time: String,
    /// `endCursor` of the previous page; `None` for the first.
    after: Option<String>,
}

impl gitlab::api::Endpoint for MyTimelogs {
    fn method(&self) -> http::Method {
        http::Method::POST
    }

    fn endpoint(&self) -> Cow<'static, str> {
        "api/graphql".into()
    }

    fn url_base(&self) -> UrlBase {
        UrlBase::Instance
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let body = serde_json::json!({
            "query": r#"
                query($start: Time!, $after: String) {
                    currentUser {
                        timelogs(startTime: $start, first: 100, after: $after) {
                            pageInfo { hasNextPage endCursor }
                            nodes {
                                id
                                timeSpent
                                spentAt
                                summary
                                project { id }
                                issue { iid title webUrl }
                                mergeRequest { iid title webUrl }
                            }
                        }
                    }
                }
            "#,
            "variables": { "start": self.start_time, "after": self.after },
        });
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// Parse one `currentUser.timelogs` response page into its timelogs and the
/// cursor of the next page (`None` on the last one).
fn timelogs_page(raw: &serde_json::Value) -> Result<(Vec<Timelog>, Option<String>)> {
    if let Some(errs) = raw["errors"].as_array()
        && !errs.is_empty()
    {
        let msg = errs
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::Gitlab(format!("currentUser.timelogs: {msg}")));
    }

    let connection = &raw["data"]["currentUser"]["timelogs"];
    let nodes = connection["nodes"].as_array().ok_or_else(|| {
        Error::Gitlab(format!(
            "currentUser.timelogs returned unexpected shape: {raw}"
        ))
    })?;
    let next = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .unwrap_or(false)
        .then(|| {
            connection["pageInfo"]["endCursor"]
                .as_str()
                .map(str::to_string)
        })
        .flatten();
    Ok((nodes.iter().filter_map(timelog_from_node).collect(), next))
}

/// Parse one `currentUser.timelogs` node. `None` skips the node: an
/// unparsable timelog GID, or a timelog attached to neither an issue nor a
/// merge request (e.g. the issuable is no longer visible to the user).
fn timelog_from_node(n: &serde_json::Value) -> Option<Timelog> {
    let id = parse_gid(n["id"].as_str().unwrap_or(""))?;

    let (kind, issuable) = if !n["issue"].is_null() {
        (Issuable::Issue, &n["issue"])
    } else if !n["mergeRequest"].is_null() {
        (Issuable::MergeRequest, &n["mergeRequest"])
    } else {
        return None;
    };

    let spent_at = n["spentAt"].as_str().unwrap_or("");
    Some(Timelog {
        id,
        spent_at: chrono::DateTime::parse_from_rfc3339(spent_at)
            .map(|d| d.timestamp().max(0) as u64)
            .unwrap_or(0),
        kind,
        project_id: parse_gid(n["project"]["id"].as_str().unwrap_or(""))
            .map(|id| id as i64)
            .unwrap_or(0),
        // GraphQL returns iid as a string; tolerate a plain number too.
        iid: issuable["iid"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| issuable["iid"].as_i64())
            .unwrap_or(0),
        title: issuable["title"].as_str().unwrap_or("").to_string(),
        web_url: issuable["webUrl"].as_str().unwrap_or("").to_string(),
        time_spent: n["timeSpent"].as_i64().unwrap_or(0).max(0) as u64,
        summary: n["summary"].as_str().unwrap_or("").to_string(),
    })
}

/// Pull the trailing integer out of a `gid://gitlab/Timelog/<id>` global ID.
fn parse_gid(gid: &str) -> Option<u64> {
    gid.rsplit('/').next().and_then(|s| s.parse().ok())
}

/// Format a duration in seconds as `"1h 30m"` (or `"45s"` when sub-minute).
/// Matches the style GitLab itself uses for `human_total_time_spent`.
pub fn format_duration(secs: u64) -> String {
    if secs == 0 {
        return "0m".to_string();
    }
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if hours == 0 && mins == 0 && rem > 0 {
        parts.push(format!("{rem}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Anchors the exact output grammar; the shape over all inputs is covered
    /// by `format_duration_renders_whole_minutes_of_any_input`.
    #[test]
    fn format_duration_pins_the_gitlab_style_grammar() {
        assert_eq!(format_duration(5400), "1h 30m");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(0), "0m");
    }

    /// Split a `"2h 5m"`-style rendering back into (hours, minutes).
    fn parse_h_m(s: &str) -> (u64, u64) {
        let (mut hours, mut mins) = (0, 0);
        for part in s.split(' ') {
            if let Some(h) = part.strip_suffix('h') {
                hours = h.parse().unwrap();
            } else if let Some(m) = part.strip_suffix('m') {
                mins = m.parse().unwrap();
            } else {
                panic!("unexpected part {part:?} in {s:?}");
            }
        }
        (hours, mins)
    }

    proptest! {
        #[test]
        fn format_duration_renders_whole_minutes_of_any_input(secs in any::<u64>()) {
            let out = format_duration(secs);
            prop_assert!(!out.is_empty());
            if secs == 0 {
                prop_assert_eq!(out, "0m");
            } else if secs < 60 {
                prop_assert_eq!(out, format!("{secs}s"));
            } else {
                // Past a minute the seconds remainder is dropped, never shown.
                let (hours, mins) = parse_h_m(&out);
                prop_assert!(mins < 60);
                prop_assert_eq!(hours * 3600 + mins * 60, secs - secs % 60);
            }
        }

        #[test]
        fn parse_gid_roundtrips_any_id(n in any::<u64>()) {
            prop_assert_eq!(parse_gid(&format!("gid://gitlab/Timelog/{n}")), Some(n));
            prop_assert_eq!(parse_gid(&n.to_string()), Some(n));
        }

        #[test]
        fn parse_gid_rejects_non_numeric_tails(tail in "[a-zA-Z ]{0,6}") {
            // The empty tail also covers the trailing-slash and empty-input
            // forms.
            prop_assert_eq!(parse_gid(&format!("gid://gitlab/Timelog/{tail}")), None);
            prop_assert_eq!(parse_gid(&tail), None);
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("boom")]
    struct Boom;

    #[test]
    fn classify_splits_network_throttle_and_rejection() {
        use gitlab::api::ApiError as A;
        let code = |s: u16| http::StatusCode::from_u16(s).unwrap();
        let every_status_shape = |s: u16| -> Vec<A<Boom>> {
            vec![
                A::GitlabService {
                    status: code(s),
                    data: Vec::new(),
                },
                A::GitlabWithStatus {
                    status: code(s),
                    msg: "m".into(),
                },
                A::GitlabObjectWithStatus {
                    status: code(s),
                    obj: serde_json::json!({}),
                },
                A::GitlabUnrecognizedWithStatus {
                    status: code(s),
                    obj: serde_json::json!({}),
                },
            ]
        };

        assert!(matches!(
            classify(A::<Boom>::Client { source: Boom }),
            Error::Transient(_)
        ));
        for s in [429, 500, 502, 503] {
            for e in every_status_shape(s) {
                assert!(
                    matches!(classify(e), Error::Throttled { status, retry_after: None, .. } if status == s),
                    "{s} is throttled"
                );
            }
        }
        for s in [400, 403, 404] {
            for e in every_status_shape(s) {
                assert!(matches!(classify(e), Error::Gitlab(_)), "{s} is permanent");
            }
        }
        for e in every_status_shape(401) {
            assert!(
                matches!(classify(e), Error::Unauthorized(_)),
                "a dead token"
            );
        }

        let limited = |secs| A::<Boom>::GitlabRateLimited {
            rl_limit: 0,
            rl_name: String::new(),
            rl_observed: 0,
            rl_remaining: 0,
            rl_reset: chrono::DateTime::UNIX_EPOCH,
            retry_after: Duration::from_secs(secs),
        };
        assert!(matches!(
            classify(limited(30)),
            Error::Throttled { status: 429, retry_after: Some(d), .. } if d == Duration::from_secs(30)
        ));
        assert!(
            matches!(
                classify(limited(0)),
                Error::Throttled {
                    retry_after: None,
                    ..
                }
            ),
            "a missing Retry-After header parses as 0 and means unknown"
        );
    }

    /// Pins what each listing asks GitLab for; a wrong scope or state here
    /// silently changes which rows the store holds.
    #[test]
    fn listings_render_path_and_query() {
        let t = chrono::DateTime::<chrono::Utc>::from_timestamp(1_782_900_000, 0);
        let q = |l: &Listing| {
            l.params()
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&")
        };
        let cases = [
            (
                Listing::AssignedIssues,
                "issues",
                "scope=assigned_to_me&state=opened",
            ),
            (
                Listing::AssignedMergeRequests,
                "merge_requests",
                "scope=assigned_to_me&state=opened",
            ),
            (
                Listing::ProjectIssues {
                    project_id: 7,
                    updated_after: None,
                },
                "projects/7/issues",
                "order_by=updated_at&sort=desc",
            ),
            (
                Listing::ProjectMergeRequests {
                    project_id: 7,
                    updated_after: t,
                },
                "projects/7/merge_requests",
                "order_by=updated_at&sort=desc&updated_after=2026-07-01T10:00:00Z",
            ),
            (
                Listing::AllIssues { updated_after: t },
                "issues",
                "scope=all&updated_after=2026-07-01T10:00:00Z",
            ),
            (
                Listing::AllMergeRequests {
                    updated_after: None,
                },
                "merge_requests",
                "scope=all",
            ),
            (
                Listing::MemberProjects,
                "projects",
                "membership=true&simple=true",
            ),
            (Listing::MemberGroups, "groups", "min_access_level=10"),
            (
                Listing::ProjectBoards { project_id: 7 },
                "projects/7/boards",
                "",
            ),
            (
                Listing::Events {
                    after: chrono::NaiveDate::from_ymd_opt(2026, 6, 30),
                },
                "events",
                "after=2026-06-30",
            ),
            (
                Listing::Issuable {
                    kind: Issuable::MergeRequest,
                    project_id: 7,
                    iid: 3,
                },
                "projects/7/merge_requests",
                "iids[]=3",
            ),
        ];
        for (listing, path, query) in cases {
            assert_eq!(listing.path(), path, "{listing:?}");
            assert_eq!(q(&listing), query, "{listing:?}");
        }
    }

    #[test]
    fn issuable_maps_to_rest_segment_and_gid_type() {
        assert_eq!(Issuable::Issue.path_segment(), "issues");
        assert_eq!(Issuable::MergeRequest.path_segment(), "merge_requests");
        assert_eq!(Issuable::Issue.gid_type(), "Issue");
        assert_eq!(Issuable::MergeRequest.gid_type(), "MergeRequest");
    }

    /// The write endpoints render the same URL shape per kind — a wrong
    /// segment here would silently hit the wrong resource class.
    #[test]
    fn write_endpoints_render_kind_specific_paths() {
        use gitlab::api::Endpoint;

        for (kind, seg) in [
            (Issuable::Issue, "issues"),
            (Issuable::MergeRequest, "merge_requests"),
        ] {
            let close = CloseEndpoint {
                kind,
                project_id: 7,
                iid: 42,
            };
            assert_eq!(close.endpoint(), format!("projects/7/{seg}/42"));

            let spend = AddSpentTime {
                kind,
                project_id: 7,
                iid: 42,
                duration: "1h",
                summary: None,
            };
            assert_eq!(
                spend.endpoint(),
                format!("projects/7/{seg}/42/add_spent_time")
            );

            let get = GetIssuableEndpoint {
                kind,
                project_id: 7,
                iid: 42,
            };
            assert_eq!(get.endpoint(), format!("projects/7/{seg}/42"));

            let update = UpdateAssigneesEndpoint {
                kind,
                project_id: 7,
                iid: 42,
                assignee_ids: vec![1],
            };
            assert_eq!(update.endpoint(), format!("projects/7/{seg}/42"));
        }
    }

    proptest! {
        #[test]
        fn compute_new_assignees_add_appends_exactly_when_absent(
            current in proptest::collection::vec(0i64..20, 0..8),
            self_id in 0i64..20,
        ) {
            match compute_new_assignees(&current, self_id, true) {
                None => prop_assert!(current.contains(&self_id), "no-op only when already assigned"),
                Some(new) => {
                    prop_assert!(!current.contains(&self_id));
                    prop_assert_eq!(*new.last().unwrap(), self_id);
                    prop_assert_eq!(new[..new.len() - 1].to_vec(), current);
                }
            }
        }

        #[test]
        fn compute_new_assignees_remove_drops_exactly_the_self_id(
            current in proptest::collection::vec(0i64..20, 0..8),
            self_id in 0i64..20,
        ) {
            match compute_new_assignees(&current, self_id, false) {
                None => prop_assert!(!current.contains(&self_id), "no-op only when not assigned"),
                Some(new) => {
                    prop_assert!(current.contains(&self_id));
                    let expected: Vec<i64> =
                        current.iter().copied().filter(|id| *id != self_id).collect();
                    prop_assert_eq!(new, expected);
                }
            }
        }

        #[test]
        fn compute_new_assignees_add_then_remove_restores_the_original(
            current in proptest::collection::vec(0i64..20, 0..8),
            self_id in 20i64..40, // guaranteed absent from `current`
        ) {
            let added = compute_new_assignees(&current, self_id, true).expect("absent → change");
            let removed = compute_new_assignees(&added, self_id, false).expect("present → change");
            prop_assert_eq!(removed, current);
        }
    }

    #[test]
    fn timelog_from_node_detects_kind_and_project() {
        let issue_node = serde_json::json!({
            "id": "gid://gitlab/Timelog/11",
            "timeSpent": 5400,
            "spentAt": "2026-07-01T10:00:00Z",
            "summary": "s",
            "project": { "id": "gid://gitlab/Project/7" },
            "issue": { "iid": "42", "title": "I", "webUrl": "https://gl/i/42" },
            "mergeRequest": null,
        });
        let t = timelog_from_node(&issue_node).unwrap();
        assert_eq!(t.kind, Issuable::Issue);
        assert_eq!(t.project_id, 7);
        assert_eq!(t.iid, 42);
        assert_eq!(t.title, "I");
        assert_eq!(t.time_spent, 5400);

        let mr_node = serde_json::json!({
            "id": "gid://gitlab/Timelog/12",
            "timeSpent": 1800,
            "spentAt": "2026-07-01T10:00:00Z",
            "summary": "",
            "project": { "id": "gid://gitlab/Project/7" },
            "issue": null,
            "mergeRequest": { "iid": "5", "title": "M", "webUrl": "https://gl/mr/5" },
        });
        let t = timelog_from_node(&mr_node).unwrap();
        assert_eq!(t.kind, Issuable::MergeRequest);
        assert_eq!(t.iid, 5);
        assert_eq!(t.title, "M");

        // A timelog whose issuable is no longer visible: today's iid-0 junk
        // rows — now skipped outright.
        let orphan = serde_json::json!({
            "id": "gid://gitlab/Timelog/13",
            "timeSpent": 60,
            "spentAt": "2026-07-01T10:00:00Z",
            "issue": null,
            "mergeRequest": null,
        });
        assert!(timelog_from_node(&orphan).is_none());

        // Missing project field → 0, the enrichment fallback marker.
        let no_project = serde_json::json!({
            "id": "gid://gitlab/Timelog/14",
            "timeSpent": 60,
            "spentAt": "2026-07-01T10:00:00Z",
            "issue": { "iid": 1, "title": "t", "webUrl": "u" },
        });
        assert_eq!(timelog_from_node(&no_project).unwrap().project_id, 0);
    }

    #[test]
    fn timelogs_page_follows_the_cursor_until_the_last_page() {
        let node = serde_json::json!({
            "id": "gid://gitlab/Timelog/11",
            "timeSpent": 60,
            "spentAt": "2026-07-01T10:00:00Z",
            "issue": { "iid": "1", "title": "t", "webUrl": "u" },
        });
        let page = |has_next: bool| {
            serde_json::json!({"data": {"currentUser": {"timelogs": {
                "pageInfo": { "hasNextPage": has_next, "endCursor": "abc" },
                "nodes": [node.clone()],
            }}}})
        };

        let (logs, next) = timelogs_page(&page(true)).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(next.as_deref(), Some("abc"));

        let (_, next) = timelogs_page(&page(false)).unwrap();
        assert_eq!(next, None, "last page ends the walk");

        let no_page_info = serde_json::json!({"data": {"currentUser": {"timelogs": {
            "nodes": [],
        }}}});
        assert_eq!(timelogs_page(&no_page_info).unwrap().1, None);
    }

    #[test]
    fn timelogs_page_surfaces_graphql_errors_and_bad_shapes() {
        let errors = serde_json::json!({"errors": [{"message": "nope"}]});
        assert!(matches!(timelogs_page(&errors), Err(Error::Gitlab(m)) if m.contains("nope")));
        let bad = serde_json::json!({"data": {"currentUser": null}});
        assert!(timelogs_page(&bad).is_err());
    }
}
