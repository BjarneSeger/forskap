# The `org.thehoster.forskapd` interface

Machine-readable definition: [`forskap-api/varlink/org.thehoster.forskapd.varlink`](../../forskap-api/varlink/org.thehoster.forskapd.varlink)
— that file is the source of truth; this document explains the behavior behind it.

**Caching model**: the daemon has no TTL. A background sync worker owns freshness:
it runs a few jobs at a time (`sync.max_in_flight`) from a persisted, jittered schedule — the assigned
issue/MR lists and the recent timelog window every few minutes
(`refresh.quick.interval_secs`), each tracked project's issues and MRs as
`updated_after` deltas every `search.partial_interval_secs` (default 30 min) with a
full resync that also reconciles deletions every `search.full_interval_secs`
(default weekly; the epics of the groups above those projects go the same way),
and the full timelog history, board columns, project/group memberships and the
issues you authored or were assigned (open or closed, updated within
`search.tracked_retention_hours`) daily (`refresh.slow.interval_secs`). Read
methods serve whatever was last synced from the local store
(`$XDG_DATA_HOME/forskapd/db/`). Reads never trigger a GitLab round-trip.

**Write model**: the methods that change an existing issue or merge request reply
success even when GitLab is unreachable — the
operation is persisted to a retry queue and drained on reconnect (exponential backoff,
dead-lettered after the retry window; see `GetFailures`). Only an actual GitLab
*rejection* surfaces as `GitlabError`. The reads reflect a write at once — a queued
or just-applied close/unassign hides the item from the assigned lists, and
`ListIssues` shows the issue as closed or no longer assigned — and the
jobs that display it rerun right after it lands. `CreateIssue` is the exception: it
is sent to GitLab once and never queued, so it fails while GitLab is away (see
[Writing directly](#writing-directly-never-queued)).

# Types

```varlink
type Issue (
  id:           int,    # global issue ID (unique across the GitLab instance)
  iid:          int,    # per-project issue number (the "#42" shown in the UI)
  project_id:   int,
  title:        string,
  web_url:      string,
  state:        string, # "opened" | "closed"
  parent:       string, # URL of the issue's epic; empty when it has none
  total_time:   string, # GitLab's human-readable total spent time ("2h"); empty when none
  graph_status: string, # board column the issue sits in, derived from its labels
                        # matched against the project's issue board; empty when no
                        # board/label matches
  open_count:   int,    # opens recorded through RecordOpen (within usage.retention_hours)
  project_avatar: string, # file of the project's avatar, see Project.avatar; empty when none
  project_path: string,   # the project's full path ("team/api"): the stored project's, else
                          # the one in web_url; empty when neither gives it
  updated_at:   int       # unix seconds, GitLab's updated_at as of the last sync; 0 when unknown
)
```

```varlink
type IssuableKind (issue, merge_request)
```

The two things time can be tracked on. Every method and type that addresses an
issue or a merge request carries an `IssuableKind` next to the `(project_id, iid)`
pair; the iid is the per-project number the UI shows (`#42` for issues, `!7` for
MRs).

```varlink
type HistoryEvent (
  timestamp:  int,          # unix seconds — spent_at for synced entries, enqueue time for queued ones
  source:     HistorySource, # gitlab (synced timelog) | queued (pending PostTime in the retry queue)
  kind:       IssuableKind, # what the time was logged on
  project_id: int,
  iid:        int,
  title:      string,       # empty on queued events whose issuable is not in the caches
  web_url:    string,
  duration:   string,
  summary:    string
)
```

```varlink
type SyncJobStatus (
  running,      # its fetch is in flight
  demanded,     # requested ahead of the schedule; runs before anything merely due
  due,          # its time has come; runs once the worker gets to it
  waiting,      # not due yet
  backing_off   # failed; held back until next_due
)

type SyncJob (
  key:           string,        # stable job id: "assigned/issues", "recent/authored/issues", "timelogs/recent", "events", "project/<id>/issues", …
  status:        SyncJobStatus,
  last_ok:       ?int,          # unix seconds, start of the last successful run; absent if it never ran
  next_due:      ?int,          # unix seconds, when the schedule runs it next (the retry time while backing off);
                                # absent while running or demanded, before the first run, and for a job that is
                                # never due again (a fetched project avatar)
  running_since: ?int,          # unix seconds, only while running
  failures:      int,           # consecutive failed runs
  last_error:    ?string        # why the last run failed, until a run succeeds
)
```

```varlink
type ActivityEvent (
  timestamp:    int,      # unix seconds
  action:       string,   # GitLab's action name: "pushed to", "opened", "commented on", "accepted", "joined", …
  target_type:  string,   # "Issue", "MergeRequest", "Milestone", …; of a comment, what was commented on; empty on pushes and membership events
  target_iid:   ?int,     # the target's number in its project, where it has one
  target_title: ?string,
  project_id:   int,      # 0 for events outside a project
  project_path: ?string,  # null when neither the project nor the item is in the store
  web_url:      ?string,  # the issue / MR, a pushed branch's commits, else the project; null when unknown
  ref:          ?string,  # pushes only: the branch or tag
  commit_count: ?int,     # pushes only
  commit_title: ?string,  # pushes only: the newest commit's title; null when the ref was deleted
  description:  ?string   # what the event did: a comment's first line (at most 200 characters, a cut one ends in …),
                          # a push's commit_title; null for every other event
)
```

```varlink
type FailedTask (
  id:         int,          # handle for RetryFailure / DismissFailure
  op:         string,       # which write failed ("PostTime", "Close", "AssignSelf", "UnassignSelf")
  kind:       IssuableKind,
  project_id: int,
  iid:        int,
  detail:     string,       # operation-specific summary (e.g. the duration)
  error:      string,       # the GitLab error that dead-lettered it
  queued_at:  int,          # unix seconds
  failed_at:  int           # unix seconds
)
```

Tasks dead-lettered by a daemon predating MR support render with the current `op`
names (a close reads `"Close"`, never `"CloseIssue"`) and `kind` `issue`.

```varlink
type MergeRequest (
  id:         int,      # global MR ID (unique across the GitLab instance)
  iid:        int,      # per-project MR number (the "!7" shown in the UI)
  project_id: int,
  title:      string,
  web_url:    string,
  state:      string,   # "opened" | "closed" | "merged" | "locked"
  assignees:  []string, # assignee usernames, captured at the last search sync
  open_count: int,      # opens recorded through RecordOpen (within usage.retention_hours)
  project_avatar: string, # file of the project's avatar, see Project.avatar; empty when none
  project_path: string,   # the project's full path ("team/api"): the stored project's, else
                          # the one in web_url; empty when neither gives it
  updated_at: int         # unix seconds, GitLab's updated_at as of the last sync; 0 when unknown
)
```

```varlink
type Project (
  id:       int,
  name:     string,
  path:     string,  # full namespace path ("team/backend/api")
  web_url:  string,
  avatar:   string,  # absolute path of the avatar image on the daemon's machine;
                     # empty when the project has none
  archived: bool     # whether the project is archived (read-only on GitLab)
)
```

`archived` is for the client to act on — grey the project out, sort it last, leave it
out of a picker: the daemon itself treats an archived project like any other, so
`Search` matches and orders it the same. A project synced by a daemon that didn't store
the flag reads `false` until the member projects are synced again, which the upgraded
daemon does at its first start.

The daemon downloads the avatars of the member projects into
`$XDG_CACHE_HOME/forskapd/avatars/` (a private project's avatar is only readable with
the token), so a launcher can show the file as it is. The extension tells the format
(`png`, `jpg`, `gif`, `webp`, `ico`, `bmp`, `tiff`, `svg`). The path changes only when
the image does, so a client may cache images by path: a new image gets a new file name,
while an unchanged one keeps its file, left as it is, though the daemon downloads it
again whenever GitLab's avatar URL changes (any update of the project does that). A
file written by a daemon from before this naming (0.12 and older) moves once more, at
its project's first download after the upgrade. The path is empty until the download
ran, for projects you are not a member of, for images above 1 MiB, and on GitLab before
16.9. The file may be gone if the cache directory was emptied; the daemon fetches it
again at its next start.

```varlink
type Group (
  id:      int,
  name:    string,
  path:    string,  # full group path ("team/backend")
  web_url: string
)
```

```varlink
type Epic (
  id:         int,     # global epic ID
  iid:        int,     # per-group epic number (the "&5" shown in the UI)
  group_id:   int,     # the group the epic belongs to
  title:      string,
  web_url:    string,
  state:      string,  # "opened" or "closed"
  open_count: int,     # opens recorded through RecordEpicOpen (within usage.retention_hours)
  group_path: string,  # the group's full path ("team/backend"): the stored group's, else
                       # the one in web_url; empty when neither gives it
  updated_at: int      # unix seconds, GitLab's updated_at as of the last sync; 0 when unknown
)
```

An epic belongs to a group and is numbered within it, so `(group_id, iid)` addresses
one. Epics are read-only here: `IssuableKind` and the write methods don't cover them.

# Errors

`org.varlink.service.InvalidParameter (parameter: string)` — the call's arguments
don't fit the method: a required one is missing, or an enum argument (`IssuableKind`,
`SearchKind`, `CacheScope`, `IssueRole`, `IssueState`) carries a value the interface
doesn't have. `parameter` says what is wrong.

`GitlabError (message: string)` — GitLab rejected the request (invalid input, API
error, rate limit), or a local precondition failed (malformed issue reference, invalid
duration, unknown failure id, a new issue without a title). For `CreateIssue` it also
reports that GitLab could not be reached. `message` is human-readable.

`NotAuthenticated (reason: ?NotAuthReason, detail: ?string)` — the daemon has no live
GitLab session (it is *dormant*). `reason` says why; `detail` carries free text (host,
underlying error) for the reasons that have one. Both fields are optional so older
daemons that send neither stay compatible — clients fall back to a generic
"run `forskap auth login`" message.

```varlink
type NotAuthReason (no_credentials, keychain_error, unreachable, token_rejected, logged_out)
```

| reason           | meaning                                                            |
|------------------|--------------------------------------------------------------------|
| `no_credentials` | no credentials stored (never logged in)                            |
| `keychain_error` | reading the OS keychain failed (`detail` = the error)              |
| `unreachable`    | credentials exist but GitLab could not be reached (`detail` set)   |
| `token_rejected` | credentials exist but GitLab rejected the token (`detail` set)     |
| `logged_out`     | the user explicitly logged out this session                        |

The daemon auto-recovers from `unreachable` in the background (unless disabled via
`[reconnect]` config); the other reasons need the user. A token GitLab rejects while
the keychain holds a newer one (rotated by another machine sharing the keychain) is
reported as `unreachable` for the moment it takes to reconnect with that one.

# Methods

## Reading

### `GetAssignedIssues(groups: ?[]string) -> (issues: []Issue)`

Open issues assigned to the authenticated user, served purely from the cache,
grouped by namespace. `groups` filters to the given group namespaces (parsed from
each issue's `web_url`, subgroups included); an issue matching several requested
groups is listed once. Omitted or empty `groups` returns everything. When the list
has never been synced: replies with an empty list if a session exists (first sync
pending), `NotAuthenticated` otherwise.

### `GetAssignedMergeRequests(groups: ?[]string) -> (merge_requests: []MergeRequest)`

Open merge requests assigned to the authenticated user, served purely from the
cache and synced on the quick cadence like the assigned issues. `groups` filters by
namespace exactly like `GetAssignedIssues`. Replies newest-updated first. When the
list has never been synced: empty list if a session exists, `NotAuthenticated`
otherwise.

### `ListIssues(role: ?IssueRole, updated_after: ?int, states: ?[]IssueState) -> (issues: []Issue)`

Your own issues across projects and states: the ones you authored or are assigned
to that were updated recently, closed ones included, newest-updated first. Served
purely from the cache. `Issue.parent` carries each one's epic, so a client can, for
instance, tell which epic most of your recent work in a project belongs to.

```varlink
type IssueRole (author, assignee)
type IssueState (opened, closed)
```

`role` picks one of the two lists; omitted, the reply is their union, with an issue
you both authored and are assigned to listed once. `states` keeps only issues in one
of the given states (omitted or empty = both). `updated_after` (unix seconds) keeps
only issues whose `updated_at` is at or after it — inclusive, like GitLab's parameter
of that name.

The lists reach back `search.tracked_retention_hours` (default 90 days) at most: an
issue last updated before that is not synced, so an older `updated_after` returns
what is stored, and an issue drops out once it has gone that long without an update.
They are synced on the slow cadence (`refresh.slow.interval_secs`, daily by
default), so an issue created, assigned or changed elsewhere can take a day to
show. In a project of the search corpus the listed issues' fields follow that sync
instead (`search.partial_interval_secs`); only which issues are listed waits. Writes through
the daemon show at once: a queued or just-applied `Close` lists the issue as
`closed`, an `UnassignSelf` takes it out of the `assignee` list (not out of the
`author` one), and both lists are synced again right after an issue write lands. An
issue you were just assigned appears with that sync.

Unlike `GetAssignedIssues`, which keeps every open assigned issue however old and
is synced every few minutes, this method is bounded by time and not by state; the two
are synced separately and may briefly disagree. The projects listed here are not
"tracked" by it: an issue closed two months ago pulls no project into the search
corpus. `graph_status` therefore stays empty for issues of projects whose boards
aren't synced (see `Search`).

When a list the call needs has never been synced — both of them with `role`
omitted: replies with an empty list if a session exists (first sync pending),
`NotAuthenticated` otherwise.

### `Search(query: string, kinds: ?[]SearchKind, limit: ?int, scope: ?SearchScope) -> (issues: []Issue, merge_requests: []MergeRequest, projects: []Project, groups: []Group, epics: []Epic)`

Searches the locally cached corpus — a pure cache read, no GitLab round-trip.
Matching is a case-insensitive substring test on issue/MR/epic titles and labels and
on project/group names and paths; a query of the exact form `#123` additionally
matches issues and MRs by their per-project number, one of the form `&5` epics by
their per-group number. Descriptions are not cached and not searched.

```varlink
type SearchKind (issues, merge_requests, projects, groups, epics)
```

`kinds` restricts the reply to a subset of them (omitted or empty = all five).
`limit` caps each returned array separately (default 50; must be positive).

```varlink
type SearchScope (projects: ?[]int, groups: ?[]string)
```

`scope` narrows every kind to the listed projects and groups, applied before
`limit` so a scoped search fills its `limit` from the scope alone. An item passes
if it matches *any* listed criterion: issues and merge requests by their
`project_id` or by the namespace of their `web_url` lying in a group (subgroups
included, as in `GetAssignedIssues`); projects by their `id` or their path lying in
a group; groups by their path; epics by the path of their group. A kind no listed
criterion can name comes back empty — `projects` alone yields no groups and no
epics. A scope that is omitted, or whose lists are both omitted or empty, is no
scope at all.

**Ranking**: issues, MRs and epics are ordered by their `RecordOpen` /
`RecordEpicOpen` statistics — most opens
first, ties by most recent open, then newest-updated — so never-opened items keep
the newest-first order among themselves below the frequently opened ones. Each row
reports its count as `open_count`. Projects and groups are sorted by path. An empty or
whitespace-only `query` selects the "frequently opened" view: only issues, MRs and
epics with at least one recorded open, ranked the same way; projects and groups are empty in
that mode. The statistics are read once per call and a read failure degrades to the
plain recency order.

What the corpus contains depends on the `[search]` daemon config. The default
`population = "tracked"` holds the issues and MRs of the member projects you are
active in: where you have assigned issues/MRs, pushed, opened or commented on an issue
or MR, or logged time within `search.tracked_retention_hours` (default 90 days).
Activity in a project you aren't a member of only keeps your assigned items there.
The issues `ListIssues` serves are searchable too, wherever they are.
Each project contributes at most its `search.max_items_per_project` most recently
updated issues and MRs. `"member"`
holds every member project's, `"all"` everything the token can see (`"auto"` is an
alias of `"tracked"`). Projects and groups are always membership-scoped.
Epics come from the member groups above those projects, at any depth (`"all"`: from
every member group), each group contributing at most its
`search.max_items_per_project` most recently updated ones. Epics need GitLab Premium
or Ultimate: on other instances `epics` is always empty, and the daemon asks each
group only once a day.
Issue `graph_status` comes from the synced board columns of the issue's project and
is empty for projects whose boards were never synced (only those of assigned issues'
projects and of tracked member projects are).
When the member projects have never been synced: replies with empty arrays if a
session exists (first sync pending), `NotAuthenticated` otherwise.

### `GetHistory(days: ?int) -> (events: []HistoryEvent)`

Time-tracking events from the last `days` days (default 7). Merges two sources,
distinguished by `source`: `gitlab` — timelogs synced from GitLab; `queued` —
`PostTime` operations still waiting in the retry queue (so freshly logged time shows
up even while GitLab is unreachable). Events carry the issuable `kind` — time logged
on merge requests appears here like issue time. Served from local state; never
errors on cache trouble (degrades to whatever is readable).

### `GetActivity(days: ?int) -> (events: []ActivityEvent)`

The user's contribution events (GitLab's `GET /events`: pushes, comments, opened,
closed and merged items, memberships) from the last `days` days (default 7), newest
first. They are the events the sync keeps as evidence for the `"tracked"` population,
so they reach back `search.tracked_retention_hours` (default 90 days) at most: a
larger `days` returns what is stored. The sync walks every page GitLab announces:
`/events` drops the rows the user may not see after slicing a page, so a short page
is not the last one. Events carry no link of their own; `web_url`
and `project_path` come from the stored issue, merge request and project rows and
are null for events in projects the store doesn't know (the project row exists for
member projects only). A comment's text is not stored. Served from the store alone;
degrades to an empty reply on cache trouble. When the events have never been synced:
replies with an empty array if a session exists (first sync pending),
`NotAuthenticated` otherwise.

### `WhoAmI() -> (host: string, user_id: int, username: string, token_expires_at: ?int, token_rotates: bool)`

The connected GitLab host and the authenticated user's ID and login name (`username`:
the `@name` GitLab shows, what `author_username=` filters take), answered from the
session without a round-trip. `NotAuthenticated` when dormant.

`token_expires_at` is the moment the token expires, in unix seconds; absent when it
never expires or the daemon hasn't read the token's details yet (it does so in the
background after connecting). `token_rotates` tells whether the daemon will replace
the token by a fresh one before that, under the current `[auth]` config: `false` for
a token without an expiry or without the needed scope, with `rotate = "never"`, and
once GitLab refused to rotate it.

## Writing (queued when GitLab is away)

The four methods of this section take the target as `(project_id, iid, kind)` —
`kind` selects issue vs merge request; the same operation works on both. They validate the reference
eagerly (`project_id`/`iid` must be positive) and reply `GitlabError` on a
malformed one without attempting or queuing anything. On an unreachable session or
a transient network failure the operation is queued for retry and the call
**replies success**; a GitLab rejection replies `GitlabError`. Other dormancy
reasons reply `NotAuthenticated`.

### `PostTime(project_id: int, iid: int, kind: IssuableKind, duration: string, summary: ?string) -> ()`

Records spent time on the issuable. `duration` uses GitLab's time-tracking syntax
(`"1h30m"`, `"45m"`, `"2d"`); an obviously malformed duration is rejected up front.
`summary` becomes the timelog note.

### `Close(project_id: int, iid: int, kind: IssuableKind) -> ()`

Closes the issuable. Immediately reflected: the assigned lists stop showing it
before the next sync, and `ListIssues` shows it as `closed`.

### `AssignSelf(project_id: int, iid: int, kind: IssuableKind) -> ()`

Assigns the authenticated user to the issuable. The assigned list is re-synced
right after the write lands, so it appears within seconds.

### `UnassignSelf(project_id: int, iid: int, kind: IssuableKind) -> ()`

Removes the authenticated user from the issuable's assignees. Immediately
reflected: the assigned lists, and `ListIssues` for the `assignee` role, stop
showing it before the next sync.

## Writing directly (never queued)

### `CreateIssue(project_id: int, title: string, description: ?string, labels: ?[]string, assign_self: ?bool, epic_id: ?int) -> (iid: int, web_url: string)`

Creates an issue in the project and replies with its number and its link.
`description` is GitLab Markdown. `labels` are label names; GitLab creates the ones
the project doesn't have yet. `assign_self` assigns the issue to the authenticated
user (omitted: nobody is assigned). `epic_id` puts the issue under an epic, named by
its global ID (`Epic.id`, not the per-group `iid`); that needs GitLab Premium or
Ultimate.

Unlike every other write this one is **never queued**: the daemon sends it to GitLab
once and replies with what came of it.

- A blank `title`, a `project_id` that isn't positive or a label containing a comma
  (GitLab takes the labels as one comma-separated list) replies `GitlabError`
  without GitLab being asked.
- Without a live session it replies `NotAuthenticated`, whatever the reason —
  `unreachable` too, where the other writes are queued.
- Any failure of the request replies `GitlabError`: a network error, a 429 or 5xx, a
  rejection, a 401. None of them demotes the session, and nothing is retried.

The reason is that a create has no idempotency key. The queued writes address an
existing `(project_id, iid, kind)`; a create has no `iid` yet, and nothing tells a
replay whether an earlier attempt landed. Replayed after a partial success (GitLab
created the issue, the answer got lost), it would file the issue a second time. The
same holds for a caller: after a `GitlabError` that isn't a plain rejection, look
before calling again.

On success the issue is visible at once, before any sync: `Search` finds it,
`ListIssues` lists it for the `author` role, and if GitLab assigned it to the user,
`GetAssignedIssues` and `ListIssues` for the `assignee` role list it too. The daemon
goes by GitLab's answer there, not by `assign_self`: a user who may not assign gets
the issue unassigned. A list that was never synced still reads as never synced. The
jobs displaying the issue rerun right after.

Once GitLab created the issue the call replies success, whatever happens then: if
GitLab's answer is unreadable, `iid` is 0 and `web_url` empty.

Work-item status widgets are out of scope (GitLab sets them through GraphQL
`workItemUpdate`, a second call with another API surface): the issue starts in the
project's default status.

## Retry-queue failures (dead letters)

Writes that exhausted their retry window or were rejected while draining land in a
persistent dead-letter store.

### `GetFailures() -> (failures: []FailedTask)`

Lists dead-lettered tasks. Never errors; storage trouble degrades to an empty list.

### `RetryFailure(id: int) -> ()`

Moves a dead-lettered task back into the retry queue. `GitlabError` when `id` is
unknown.

### `DismissFailure(id: int) -> ()`

Deletes one dead-lettered task. `GitlabError` when `id` is unknown.

### `ClearFailures() -> ()`

Deletes all dead-lettered tasks.

## Sync status

### `GetSyncJobs() -> (jobs: []SyncJob, paused_until: ?int)`

Lists the jobs the sync worker has planned, in the order it runs them: the ones in
flight, the ones demanded ahead of the schedule (a `ClearCache`, a write that just
landed), the due ones by priority, then the rest by `next_due`. The worker runs up to
`sync.max_in_flight` jobs at once, one per project, so several can be `running`,
each with its own `running_since`.

`paused_until` (unix seconds) is set while a GitLab rate limit (429) holds every
job back; the statuses then say what runs once the pause is over.

Status, not GitLab data: never errors and is served while dormant. A dormant
daemon runs nothing, so its jobs stay `due` until a session exists. `last_error`
is kept in memory only — after a daemon restart a job can be `backing_off` without
one.

## Usage statistics

### `RecordOpen(project_id: int, iid: int, kind: IssuableKind) -> ()`

Counts one open of an issue or merge request — the client's "the user just went
there" signal (`forskap issue open`, a launcher activation). Purely local bookkeeping: no GitLab
round-trip, no queueing, works while dormant. `Search` ranks by these counts and
reports them as `open_count` (also on `GetAssignedIssues` /
`GetAssignedMergeRequests` rows). An entry expires `usage.retention_hours` (default
90 days) after its last open, and the record is capped at 1000 issuables (lowest
counts dropped first); both are enforced on write. A non-positive `project_id` or
`iid` is an eager `GitlabError`. Cleared only by `ClearCache` scope `usage`.

### `RecordEpicOpen(group_id: int, iid: int) -> ()`

`RecordOpen` for an epic, addressed by its group (`forskap epic open`, a launcher
activation): same bookkeeping, retention and cap, counted apart from issues and
MRs. A non-positive `group_id` or `iid` is an eager `GitlabError`.

## Cache control

### `ClearCache(scope: ?[]CacheScope) -> ()`

```varlink
type CacheScope (assigned, search, quick, slow, stale, usage)
```

Clears cached state and makes its sync jobs due at once. Omitted or empty `scope`
clears everything synced. Otherwise each scope selects a slice:

| scope      | clears                                                       |
|------------|--------------------------------------------------------------|
| `assigned` | the assigned issue/MR lists, the `ListIssues` lists and the board columns |
| `search` | the corpus: issues, MRs, epics, projects, groups, project avatars |
| `quick`  | history inside the quick window (last `refresh.quick.window_hours`) |
| `slow`   | history between the retention horizon and the quick window     |
| `stale`  | history older than `history.retention_hours` (normally already pruned) |
| `usage`  | the `RecordOpen` / `RecordEpicOpen` statistics — **only when listed explicitly**; the empty "everything" scope leaves them alone (user data, not a cache) |

When a session exists, the reply waits (up to 30 s) until what it cleared is
re-synced: the assigned lists for `assigned`, `search` and the empty scope (plus the
board columns of their projects that never synced), the recent and full history
for a history band and the empty scope. Everything else refills in the
background — the `ListIssues` lists among it, so that method can reply empty right
after a clear; `usage` alone makes no GitLab call. Replies success even when
dormant — the cleared state then stays empty until the next successful sync.

## Session

### `Login(host: string, token: string) -> ()`

Connects to `host` with the personal access token, stores the credentials in the OS
keychain, and flips the daemon to connected (waking the retry-queue drain).
`GitlabError` when GitLab rejects the token or the keychain write fails. Prefer
`forskap auth login`, which walks through creating a PAT with the right scopes.

### `Logout() -> ()`

Drops the session (subsequent calls reply `NotAuthenticated` with reason
`logged_out`) and deletes the stored credentials from the keychain.

# Calling from the shell

```sh
SOCKET=unix:$XDG_RUNTIME_DIR/forskapd.socket

# list assigned issues
varlinkctl call $SOCKET org.thehoster.forskapd.GetAssignedIssues '{}'

# the issues I authored that were closed, updated since 2026-09-01
varlinkctl call $SOCKET org.thehoster.forskapd.ListIssues \
  '{"role": "author", "states": ["closed"], "updated_after": 1788220800}'

# post 1h30m to project 42, issue #7
varlinkctl call $SOCKET org.thehoster.forskapd.PostTime \
  '{"project_id": 42, "iid": 7, "kind": "issue", "duration": "1h30m", "summary": "code review"}'

# close merge request !3 in project 42
varlinkctl call $SOCKET org.thehoster.forskapd.Close \
  '{"project_id": 42, "iid": 3, "kind": "merge_request"}'

# introspect the live interface
varlinkctl introspect $SOCKET org.thehoster.forskapd
```
