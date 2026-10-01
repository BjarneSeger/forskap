//! GitLab API access — the only module that knows about the `gitlab` crate.
//!
//! Wraps `gitlab::AsyncGitlab`: one paginated endpoint for every read
//! ([`Listing`]), the GraphQL timelog query, the project avatar download, and
//! the write endpoints the crate doesn't ship (`add_spent_time`, `close`,
//! assignment, token rotation).

use std::borrow::Cow;
use std::future::Future;
use std::time::Duration;

use gitlab::api::{AsyncQuery, UrlBase};
use tracing::{info, instrument, warn};

use crate::error::{Error, Result};
use crate::secrets::Token;
use crate::sync::model::Timelog;

/// Which GitLab issuable an operation targets. Internal counterpart of the
/// wire `IssuableKind`, kept separate so persisted queue/history records
/// don't couple the on-disk format to the api crate; `Default = Issue`
/// because every record written before MR support was an issue, which lets
/// them deserialize via `#[serde(default)]`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
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
    /// Login name of the authenticated user, next to the id for `WhoAmI`.
    current_username: String,
    /// The token `inner` authenticates with.
    token: Token,
}

/// What GitLab knows about the token a client authenticates with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub scopes: Vec<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The token dies at the start of this day (UTC); `None` never expires.
    pub expires_at: Option<chrono::NaiveDate>,
}

/// The successor of a rotated token. The old one is revoked already.
#[derive(Debug, Clone)]
pub struct RotatedToken {
    pub token: Token,
    /// `None` if the answer's lifetime was unreadable.
    pub info: Option<TokenInfo>,
}

impl GitlabClient {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    pub fn current_user_id(&self) -> i64 {
        self.current_user_id
    }

    pub fn current_username(&self) -> &str {
        &self.current_username
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
    /// Issues the user authored, all states, updated at or after
    /// `updated_after`.
    RecentAuthoredIssues {
        updated_after: chrono::DateTime<chrono::Utc>,
    },
    /// Issues assigned to the user, all states, updated at or after
    /// `updated_after`.
    RecentAssignedIssues {
        updated_after: chrono::DateTime<chrono::Utc>,
    },
    /// Projects the user is a member of.
    MemberProjects,
    /// Groups the user is a member of (a bare `GET /groups` would include
    /// public non-member groups).
    MemberGroups,
    /// A group's own epics, all states; `updated_after` for a delta. Needs
    /// GitLab Premium: other instances answer 403 or 404.
    GroupEpics {
        group_id: i64,
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// A project's boards, lists embedded.
    ProjectBoards { project_id: i64 },
    /// The user's own contribution events created after `after` (a date;
    /// GitLab compares exclusively). Oldest first: a new event lands behind
    /// the walk instead of shifting every later page.
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
            Self::AssignedIssues
            | Self::AllIssues { .. }
            | Self::RecentAuthoredIssues { .. }
            | Self::RecentAssignedIssues { .. } => "issues".into(),
            Self::AssignedMergeRequests | Self::AllMergeRequests { .. } => "merge_requests".into(),
            Self::ProjectIssues { project_id, .. } => format!("projects/{project_id}/issues"),
            Self::ProjectMergeRequests { project_id, .. } => {
                format!("projects/{project_id}/merge_requests")
            }
            Self::MemberProjects => "projects".into(),
            Self::MemberGroups => "groups".into(),
            Self::GroupEpics { group_id, .. } => format!("groups/{group_id}/epics"),
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
            // In GitLab's default order (by creation), not by `updated_at`:
            // an item updated mid-walk would jump to a page already read
            // and be missed, and no follow-up delta catches it here.
            Self::RecentAuthoredIssues { updated_after }
            | Self::RecentAssignedIssues { updated_after } => {
                let scope = match self {
                    Self::RecentAuthoredIssues { .. } => "created_by_me",
                    _ => "assigned_to_me",
                };
                let mut p = vec![("scope", scope.into()), ("state", "all".into())];
                p.extend(after(&Some(*updated_after)));
                p
            }
            // Not `simple=true`: that representation leaves out `archived`.
            Self::MemberProjects => vec![("membership", "true".into())],
            // 10 = Guest, the lowest membership level.
            Self::MemberGroups => vec![("min_access_level", "10".into())],
            // A subgroup's epics belong to its own listing, so a group and
            // its parent never fetch the same ones.
            Self::GroupEpics { updated_after, .. } => {
                let mut p = vec![
                    ("include_descendant_groups", "false".into()),
                    ("order_by", "updated_at".into()),
                    ("sort", "desc".into()),
                ];
                p.extend(after(updated_after));
                p
            }
            Self::ProjectBoards { .. } => Vec::new(),
            Self::Events { after } => {
                let mut p = vec![("sort", "asc".into())];
                p.extend(after.map(|d| ("after", d.format("%Y-%m-%d").to_string())));
                p
            }
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
    /// no REST listing for them, so this is the one GraphQL read. A timelog
    /// whose issue or MR the user can no longer read comes with `iid` 0: it
    /// exists, but its details are gone.
    async fn list_timelogs(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<Timelog>>;

    /// A project's avatar image as uploaded; `None` when GitLab has none to
    /// serve (no avatar, or an instance older than 16.9 without the endpoint).
    async fn project_avatar(&self, project_id: i64) -> Result<Option<Vec<u8>>>;

    /// Scopes and lifetime of the token this client authenticates with.
    async fn token_info(&self) -> Result<TokenInfo>;

    /// Replace the client's token by a new one expiring at `expires_at`
    /// (GitLab's default lifetime if `None`). On success the client's own
    /// token is revoked: every later call through it fails with a 401.
    async fn rotate_token(&self, expires_at: Option<chrono::NaiveDate>) -> Result<RotatedToken>;
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

    pub async fn connect(host: &str, token: &Token) -> Result<Self> {
        let inner = gitlab::GitlabBuilder::new(host.to_string(), token.expose().to_string())
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
        let current_username = user["username"]
            .as_str()
            .ok_or_else(|| Error::Gitlab(format!("GET /user response missing username: {user}")))?;
        info!(
            current_user_id,
            current_username, "resolved authenticated GitLab user"
        );

        Ok(Self {
            inner,
            host: host.to_string(),
            current_user_id,
            current_username: current_username.to_string(),
            token: token.clone(),
        })
    }

    /// Like [`GitlabClient::connect`], but retries transient (network) failures
    /// with bounded exponential back-off (see [`retry_transient`]). A permanent
    /// rejection (a bad token) fails immediately without retrying. Used by the
    /// interactive `Login` handler so a momentary blip doesn't fail the command.
    pub async fn connect_with_retry(host: &str, token: &Token) -> Result<Self> {
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
        walk_pages(&self.inner, listing, limit).await
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

    #[instrument(skip(self))]
    async fn project_avatar(&self, project_id: i64) -> Result<Option<Vec<u8>>> {
        let endpoint = gitlab::api::raw(ProjectAvatarEndpoint { project_id });
        retry_transient("fetch avatar", || async {
            match endpoint.query_async(&self.inner).await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if status_of(&e) == Some(404) => Ok(None),
                Err(e) => Err(classify(e)),
            }
        })
        .await
    }

    #[instrument(skip(self))]
    async fn token_info(&self) -> Result<TokenInfo> {
        let raw: serde_json::Value = SelfTokenEndpoint
            .query_async(&self.inner)
            .await
            .map_err(classify)?;
        token_info_from(&raw)
    }

    #[instrument(skip(self))]
    async fn rotate_token(&self, expires_at: Option<chrono::NaiveDate>) -> Result<RotatedToken> {
        // Never retried in here: whether a failed attempt may be repeated is
        // the caller's decision, the old token may be gone already.
        let raw: serde_json::Value = RotateSelfTokenEndpoint { expires_at }
            .query_async(&self.inner)
            .await
            .map_err(|e| {
                let refused = status_of(&e).is_some();
                match classify(e) {
                    // No status to tell a refusal by: GitLab may have rotated.
                    Error::Gitlab(detail) if !refused => Error::RotationLost(detail),
                    other => other,
                }
            })?;
        rotated_token_from(&raw)
    }
}

/// `GET /projects/:id/avatar`: the image itself, readable with the token
/// where the avatar's upload URL needs a browser session.
struct ProjectAvatarEndpoint {
    project_id: i64,
}

impl gitlab::api::Endpoint for ProjectAvatarEndpoint {
    fn method(&self) -> http::Method {
        http::Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!("projects/{}/avatar", self.project_id).into()
    }
}

/// `GET /personal_access_tokens/self`
struct SelfTokenEndpoint;

impl gitlab::api::Endpoint for SelfTokenEndpoint {
    fn method(&self) -> http::Method {
        http::Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        "personal_access_tokens/self".into()
    }
}

/// `POST /personal_access_tokens/self/rotate`
struct RotateSelfTokenEndpoint {
    expires_at: Option<chrono::NaiveDate>,
}

impl gitlab::api::Endpoint for RotateSelfTokenEndpoint {
    fn method(&self) -> http::Method {
        http::Method::POST
    }

    fn endpoint(&self) -> Cow<'static, str> {
        "personal_access_tokens/self/rotate".into()
    }

    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let Some(expires_at) = self.expires_at else {
            return Ok(None);
        };
        let body = serde_json::json!({"expires_at": expires_at.format("%Y-%m-%d").to_string()});
        Ok(Some(("application/json", serde_json::to_vec(&body)?)))
    }
}

/// Parse a personal access token as GitLab's REST API returns it.
fn token_info_from(raw: &serde_json::Value) -> Result<TokenInfo> {
    let scopes = raw["scopes"]
        .as_array()
        .ok_or_else(|| Error::Gitlab("token response carries no scopes".into()))?
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();
    let expires_at = match &raw["expires_at"] {
        serde_json::Value::Null => None,
        v => Some(
            v.as_str()
                .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                .ok_or_else(|| Error::Gitlab(format!("unparsable token expiry: {v}")))?,
        ),
    };
    Ok(TokenInfo {
        scopes,
        created_at: raw["created_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.to_utc()),
        expires_at,
    })
}

/// Parse a rotate response. Its errors never quote the response: it holds
/// the new token. Only a missing token fails it: the old one is revoked, so
/// the new one must not be dropped over an unreadable lifetime.
fn rotated_token_from(raw: &serde_json::Value) -> Result<RotatedToken> {
    let token = raw["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::RotationLost("rotate response carries no token".into()))?;
    Ok(RotatedToken {
        token: Token::new(token),
        info: token_info_from(raw).ok(),
    })
}

/// Rows per page; GitLab's maximum.
const PER_PAGE: usize = 100;

/// One page of a listing as GitLab answered it.
struct Page {
    rows: Vec<serde_json::Value>,
    /// `X-Next-Page` named one.
    has_next: bool,
}

/// Fetch every page of `listing` GitLab announces, at most `limit` rows.
///
/// A short page is not the last one: `/events` drops the rows the user may
/// not see after slicing the page (the `gitlab` crate's paged query stops
/// there and loses the rest), so only an empty page or a short page without
/// a successor ends the walk. Pages are retried one at a time.
async fn walk_pages<C>(
    client: &C,
    listing: &Listing,
    limit: Option<usize>,
) -> Result<Vec<serde_json::Value>>
where
    C: gitlab::api::AsyncClient + Sync,
{
    let mut rows = Vec::new();
    let mut page = 1u64;
    loop {
        let fetched =
            retry_transient("list", || async { fetch_page(client, listing, page).await }).await?;
        let empty = fetched.rows.is_empty();
        let short = fetched.rows.len() < PER_PAGE;
        rows.extend(fetched.rows);
        let enough = limit.is_some_and(|l| rows.len() >= l);
        if empty || enough || (short && !fetched.has_next) {
            break;
        }
        page += 1;
    }
    rows.truncate(limit.unwrap_or(usize::MAX));
    Ok(rows)
}

/// `GET <listing path>?<listing params>&page=<page>&per_page=100`.
async fn fetch_page<C>(client: &C, listing: &Listing, page: u64) -> Result<Page>
where
    C: gitlab::api::AsyncClient + Sync,
{
    let mut url = client.rest_endpoint(&listing.path()).map_err(classify)?;
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in listing.params() {
            query.append_pair(k, &v);
        }
        query.append_pair("page", &page.to_string());
        query.append_pair("per_page", &PER_PAGE.to_string());
    }
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(url.as_str())
        .header(http::header::ACCEPT, "application/json");
    let rsp = client
        .rest_async(request, Vec::new())
        .await
        .map_err(classify)?;
    let status = rsp.status();
    if !status.is_success() {
        let retry_after = header(&rsp, http::header::RETRY_AFTER.as_str())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let detail = format!(
            "{status} on {}: {}",
            listing.path(),
            error_message(rsp.body())
        );
        return Err(throttled_or_rejected(status.as_u16(), retry_after, detail));
    }
    let rows = serde_json::from_slice(rsp.body())
        .map_err(|e| Error::Gitlab(format!("page {page} of {}: {e}", listing.path())))?;
    let has_next = header(&rsp, "x-next-page").is_some_and(|v| !v.is_empty());
    Ok(Page { rows, has_next })
}

fn header<'a, B>(rsp: &'a http::Response<B>, name: &str) -> Option<&'a str> {
    rsp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// GitLab's `{"message": …}` or `{"error": …}`, else the body itself.
fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            [&v["message"], &v["error"]]
                .into_iter()
                .find(|m| !m.is_null())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| text.chars().take(200).collect())
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
    let retry_after = match &e {
        A::Client { .. } => return Error::Transient(detail),
        A::GitlabRateLimited { retry_after, .. } => Some(*retry_after),
        _ => None,
    };
    match status_of(&e) {
        Some(status) => throttled_or_rejected(status, retry_after, detail),
        None => Error::Gitlab(detail),
    }
}

/// The HTTP status GitLab answered with; `None` for a failure before the
/// request or while reading the answer.
fn status_of<E>(e: &gitlab::api::ApiError<E>) -> Option<u16>
where
    E: std::error::Error + Send + Sync + 'static,
{
    use gitlab::api::ApiError as A;
    match e {
        A::GitlabRateLimited { .. } => Some(429),
        A::GitlabService { status, .. }
        | A::GitlabWithStatus { status, .. }
        | A::GitlabObjectWithStatus { status, .. }
        | A::GitlabUnrecognizedWithStatus { status, .. } => Some(status.as_u16()),
        _ => None,
    }
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

/// Parse one `currentUser.timelogs` node; `None` for an unparsable timelog
/// GID. A timelog attached to neither an issue nor a merge request (the
/// issuable is no longer visible to the user) comes back with `iid` 0.
fn timelog_from_node(n: &serde_json::Value) -> Option<Timelog> {
    let id = parse_gid(n["id"].as_str().unwrap_or(""))?;

    let (kind, issuable) = if !n["issue"].is_null() {
        (Issuable::Issue, &n["issue"])
    } else if !n["mergeRequest"].is_null() {
        (Issuable::MergeRequest, &n["mergeRequest"])
    } else {
        (Issuable::default(), &serde_json::Value::Null)
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

    /// What the fake answers a page request with, in request order.
    enum Answer {
        /// Rows `from..to` as `{"id": n}`, with `X-Next-Page` set or empty.
        Rows(std::ops::Range<usize>, bool),
        Status(u16, Option<&'static str>),
    }

    /// A GitLab that serves canned pages and records every URL asked for.
    #[derive(Default)]
    struct PagedFake {
        answers: std::sync::Mutex<Vec<Answer>>,
        urls: std::sync::Mutex<Vec<String>>,
    }

    impl PagedFake {
        fn with(answers: Vec<Answer>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                ..Default::default()
            }
        }

        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl gitlab::api::RestClient for PagedFake {
        type Error = Boom;

        fn rest_endpoint(
            &self,
            endpoint: &str,
        ) -> std::result::Result<url::Url, gitlab::api::ApiError<Boom>> {
            Ok(url::Url::parse("https://gitlab.test/api/v4/")
                .unwrap()
                .join(endpoint)
                .unwrap())
        }
    }

    #[async_trait::async_trait]
    impl gitlab::api::AsyncClient for PagedFake {
        async fn rest_async(
            &self,
            request: http::request::Builder,
            _body: Vec<u8>,
        ) -> std::result::Result<http::Response<bytes::Bytes>, gitlab::api::ApiError<Boom>>
        {
            let request = request.body(()).unwrap();
            self.urls.lock().unwrap().push(request.uri().to_string());
            let answer = {
                let mut answers = self.answers.lock().unwrap();
                if answers.is_empty() {
                    Answer::Rows(0..0, false)
                } else {
                    answers.remove(0)
                }
            };
            let rsp = match answer {
                Answer::Rows(range, next) => {
                    let rows: Vec<_> = range.map(|i| serde_json::json!({"id": i})).collect();
                    http::Response::builder()
                        .status(200)
                        .header("x-next-page", if next { "2" } else { "" })
                        .body(bytes::Bytes::from(serde_json::to_vec(&rows).unwrap()))
                }
                Answer::Status(code, retry_after) => {
                    let mut rsp = http::Response::builder().status(code);
                    if let Some(secs) = retry_after {
                        rsp = rsp.header("retry-after", secs);
                    }
                    rsp.body(bytes::Bytes::from_static(br#"{"message":"nope"}"#))
                }
            };
            Ok(rsp.unwrap())
        }
    }

    fn ids(rows: &[serde_json::Value]) -> Vec<u64> {
        rows.iter().map(|r| r["id"].as_u64().unwrap()).collect()
    }

    const EVENTS: Listing = Listing::Events { after: None };

    /// The regression: `/events` drops invisible rows after slicing the
    /// page, so page 1 came back with 99 rows and page 2 was never asked
    /// for.
    #[tokio::test]
    async fn a_short_page_with_a_successor_is_followed() {
        let fake = PagedFake::with(vec![
            Answer::Rows(0..99, true),
            Answer::Rows(99..158, false),
        ]);
        let rows = walk_pages(&fake, &EVENTS, None).await.unwrap();
        assert_eq!(ids(&rows), (0..158).collect::<Vec<_>>());
        assert_eq!(
            fake.urls(),
            [
                "https://gitlab.test/api/v4/events?sort=asc&page=1&per_page=100",
                "https://gitlab.test/api/v4/events?sort=asc&page=2&per_page=100",
            ]
        );
    }

    #[tokio::test]
    async fn a_short_page_without_a_successor_ends_the_walk() {
        let fake = PagedFake::with(vec![Answer::Rows(0..17, false)]);
        let rows = walk_pages(&fake, &EVENTS, None).await.unwrap();
        assert_eq!(rows.len(), 17);
        assert_eq!(fake.urls().len(), 1);
    }

    /// GitLab stops counting at 10 000 rows, so a missing successor on a
    /// full page proves nothing; the empty page after it does.
    #[tokio::test]
    async fn a_full_page_is_followed_until_an_empty_one() {
        let fake = PagedFake::with(vec![Answer::Rows(0..100, false), Answer::Rows(0..0, false)]);
        let rows = walk_pages(&fake, &EVENTS, None).await.unwrap();
        assert_eq!(rows.len(), 100);
        assert_eq!(fake.urls().len(), 2);
    }

    #[tokio::test]
    async fn the_limit_ends_the_walk_and_caps_the_rows() {
        let fake = PagedFake::with(vec![
            Answer::Rows(0..100, true),
            Answer::Rows(100..200, true),
            Answer::Rows(200..300, true),
        ]);
        let rows = walk_pages(&fake, &EVENTS, Some(150)).await.unwrap();
        assert_eq!(ids(&rows), (0..150).collect::<Vec<_>>());
        assert_eq!(fake.urls().len(), 2);
    }

    #[tokio::test]
    async fn a_page_walk_classifies_gitlab_answers() {
        let fake = PagedFake::with(vec![Answer::Status(429, Some("7"))]);
        assert!(matches!(
            walk_pages(&fake, &EVENTS, None).await,
            Err(Error::Throttled { status: 429, retry_after: Some(d), .. }) if d == Duration::from_secs(7)
        ));

        let fake = PagedFake::with(vec![Answer::Status(401, None)]);
        assert!(matches!(
            walk_pages(&fake, &EVENTS, None).await,
            Err(Error::Unauthorized(_))
        ));

        let fake = PagedFake::with(vec![Answer::Status(404, None)]);
        assert!(matches!(
            walk_pages(&fake, &EVENTS, None).await,
            Err(Error::Gitlab(detail)) if detail.contains("404") && detail.contains("nope")
        ));
    }

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
                Listing::RecentAuthoredIssues {
                    updated_after: t.unwrap(),
                },
                "issues",
                "scope=created_by_me&state=all&updated_after=2026-07-01T10:00:00Z",
            ),
            (
                Listing::RecentAssignedIssues {
                    updated_after: t.unwrap(),
                },
                "issues",
                "scope=assigned_to_me&state=all&updated_after=2026-07-01T10:00:00Z",
            ),
            (Listing::MemberProjects, "projects", "membership=true"),
            (Listing::MemberGroups, "groups", "min_access_level=10"),
            (
                Listing::GroupEpics {
                    group_id: 3,
                    updated_after: t,
                },
                "groups/3/epics",
                "include_descendant_groups=false&order_by=updated_at&sort=desc\
                 &updated_after=2026-07-01T10:00:00Z",
            ),
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
                "after=2026-06-30&sort=asc",
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
    fn the_avatar_endpoint_renders_its_path() {
        use gitlab::api::Endpoint;

        let avatar = ProjectAvatarEndpoint { project_id: 7 };
        assert_eq!(avatar.endpoint(), "projects/7/avatar");
        assert_eq!(avatar.method(), http::Method::GET);
    }

    #[test]
    fn token_endpoints_render_path_and_body() {
        use gitlab::api::Endpoint;

        assert_eq!(SelfTokenEndpoint.endpoint(), "personal_access_tokens/self");
        assert_eq!(SelfTokenEndpoint.method(), http::Method::GET);

        let rotate = RotateSelfTokenEndpoint {
            expires_at: chrono::NaiveDate::from_ymd_opt(2027, 1, 31),
        };
        assert_eq!(rotate.endpoint(), "personal_access_tokens/self/rotate");
        assert_eq!(rotate.method(), http::Method::POST);
        let (mime, body) = rotate.body().unwrap().unwrap();
        assert_eq!(mime, "application/json");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"expires_at": "2027-01-31"})
        );

        let default_lifetime = RotateSelfTokenEndpoint { expires_at: None };
        assert!(default_lifetime.body().unwrap().is_none());
    }

    #[test]
    fn token_info_parses_lifetime_and_scopes() {
        let raw = serde_json::json!({
            "id": 1,
            "scopes": ["api", "self_rotate"],
            "created_at": "2026-01-01T10:00:00.000Z",
            "expires_at": "2026-12-31",
        });
        let info = token_info_from(&raw).unwrap();
        assert_eq!(info.scopes, ["api", "self_rotate"]);
        assert_eq!(
            info.created_at.map(|d| d.timestamp()),
            Some(1_767_261_600),
            "{info:?}"
        );
        assert_eq!(
            info.expires_at,
            chrono::NaiveDate::from_ymd_opt(2026, 12, 31)
        );

        let forever = serde_json::json!({"scopes": [], "expires_at": null});
        let info = token_info_from(&forever).unwrap();
        assert_eq!((info.expires_at, info.created_at), (None, None));

        assert!(token_info_from(&serde_json::json!({"expires_at": null})).is_err());
        let garbled = serde_json::json!({"scopes": [], "expires_at": "soon"});
        assert!(token_info_from(&garbled).is_err());
    }

    #[test]
    fn a_rotate_response_without_a_token_is_an_error_not_quoting_it() {
        let raw = serde_json::json!({
            "scopes": ["self_rotate"],
            "created_at": "2026-01-01T10:00:00Z",
            "expires_at": "2026-12-31",
            "token": "glpat-new",
        });
        let rotated = rotated_token_from(&raw).unwrap();
        assert_eq!(rotated.token.expose(), "glpat-new");
        assert_eq!(
            rotated.info.and_then(|i| i.expires_at),
            chrono::NaiveDate::from_ymd_opt(2026, 12, 31)
        );

        // The token survives an unreadable lifetime.
        let broken = serde_json::json!({"token": "glpat-new", "expires_at": "soon"});
        let rotated = rotated_token_from(&broken).unwrap();
        assert_eq!(rotated.token.expose(), "glpat-new");
        assert_eq!(rotated.info, None);
        assert!(!format!("{rotated:?}").contains("glpat-new"));

        let lost = rotated_token_from(&serde_json::json!({"scopes": []})).unwrap_err();
        assert!(matches!(lost, Error::RotationLost(_)), "{lost}");
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

        // A timelog whose issuable is no longer visible: it exists, but
        // without an iid (so it's never stored as a row of its own).
        let orphan = serde_json::json!({
            "id": "gid://gitlab/Timelog/13",
            "timeSpent": 60,
            "spentAt": "2026-07-01T10:00:00Z",
            "issue": null,
            "mergeRequest": null,
        });
        let t = timelog_from_node(&orphan).unwrap();
        assert_eq!((t.id, t.iid), (13, 0));
        assert!(!crate::sync::model::Resource::is_valid(&t));

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
