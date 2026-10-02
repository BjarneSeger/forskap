# The `org.thehoster.forskapd` interface

The shapes are in [`forskap-api/varlink/org.thehoster.forskapd.varlink`](../../forskap-api/varlink/org.thehoster.forskapd.varlink),
the source of truth: every type, field, method and error, most with a line on what
it means. A running daemon serves that file as it is, comments included:

```sh
varlinkctl introspect unix:$XDG_RUNTIME_DIR/forskapd.socket org.thehoster.forskapd
```

This document says what the definition can't: the caching and write models, what
each method does beyond its line, orderings, edge cases, and how to call it.

The daemon serves a second interface on the same socket,
`org.thehoster.forskapd.admin`, for the bundled CLI: logging in and out, clearing the
cache, the sync worker's jobs. It has
[a part of its own](#the-orgthehosterforskapdadmin-interface) at the end; everything
before that part is about `org.thehoster.forskapd`.

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
dead-lettered after the retry window; see `GetFailures`). Only GitLab *refusing* it
surfaces as `GitlabError`, and a `PostTime` GitLab answered with a 5xx as
`GitlabUnavailable` (see [Errors](#errors)). The reads reflect a write at once — a queued
or just-applied close/unassign hides the item from the assigned lists, and
`ListWorkItems` shows the issue as closed or no longer assigned — and the
jobs that display it rerun right after it lands. `CreateWorkItem` is the exception:
it is sent to GitLab once and never queued, so it fails while GitLab is away (see
[Writing directly](#writing-directly-never-queued)).

# Finding the socket

The daemon listens on a unix socket of the user it runs as. A client looks for it in
this order:

1. `$FORSKAPD_SOCKET`, used verbatim as a varlink address (`unix:/path/to.socket`),
   else the same under its name from before the rename, `$GITLAB_TRACKRD_SOCKET`.
2. `$XDG_RUNTIME_DIR/forskapd.socket`, where that variable holds an absolute path;
   not on macOS, which has no runtime directory.
3. `forskapd/forskapd.socket` in the user's data directory: `$XDG_DATA_HOME` if it is
   an absolute path, else `~/.local/share`; on macOS `~/Library/Application Support`.

The last two are the daemon's default socket: `forskap_api::default_socket()` to a
Rust client, `DefaultAddress()` to a Go one. A daemon told to use another
(`forskapd --socket`, `[server] socket` in its config) is only found through
`$FORSKAPD_SOCKET`. The `forskap` CLI also takes a `socket` from its own config file,
after the environment and before the default; that file is the CLI's business, and
other clients don't read it.

The socket is its user's alone: mode 0600, in a directory of theirs, never a shared
one like `/tmp` where another user could put a socket first. On Linux the packages
ship a systemd user socket unit, `forskapd.socket`, listening on the runtime directory's
path; once it is enabled (`systemctl --user enable --now forskapd.socket`), the first
connection starts the daemon, so a client needn't check whether it runs. Installed
through Homebrew, the daemon runs as a `brew services` service and creates the socket
when it starts.

# Compatibility

`org.thehoster.forskapd` is stable: a client built against one version of it works
with every daemon of the same major version and a minor version at least its own.
These rules cover `org.thehoster.forskapd` only;
[the admin interface](#the-orgthehosterforskapdadmin-interface) is outside all of
them.

The interface's version is the forskap-api crate's: every change to the interface
bumps the minor version, a patch is a fix to a binding alone. A client tells which
version a daemon speaks by `GetStatus.api_version`, and both bindings carry the rule
above: `forskap_api::compatible` in Rust, `Status.Compatible` in Go, against the
version they were built from. A daemon that answers `GetStatus` with `MethodNotFound`
is older than 0.32.0, from before the interface was stable.

The daemon leaves an optional field without a value out of what it sends, wherever
the field is; the Rust and Go bindings leave such an argument out too.

Within a major version, a client can rely on these:

- Nothing is removed or renamed: no method, argument, reply field, type, struct field,
  enum variant, error or error parameter.
- Nothing changes its type: a field, argument or error parameter keeps the same named
  type or builtin under the same `?`, `[]` and `[string]`, so an optional one stays
  optional and a required one required; a struct stays a struct and an enum an enum.
- A field new to a reply, to a struct or to an error is optional (`?T`).
- A new argument is optional, so a call that leaves it out means what it meant before.
  The methods whose arguments grow take them in one struct (`SearchOptions`,
  `WorkItemFilter`, `NewWorkItem`, `Scope`), and a new one is a new optional field of
  it: the bindings' signatures stay, and a caller that fills the struct from `Default`
  (`SearchOptions { limit: Some(5), ..Default::default() }` in Rust) or names the
  fields it sets (Go) keeps compiling.
- An enum that a reply or an error can carry, directly or in a field of a struct it
  carries (`IssuableKind`, `HistorySource`, `NotAuthReason`), gets no new variants: a
  new state is a new optional field. The enums only arguments take (`SearchKind`,
  `WorkItemRole`, `WorkItemState`) may get new ones, which an older daemon refuses.
- New methods, types and errors may appear.

And it must tolerate these:

- Reply fields it doesn't know: it ignores them. The Rust and Go bindings do.
- An optional field that is `null`: it means the same as the field being absent. The
  Rust and Go bindings read both as `None` / `nil`.
- `org.varlink.service.InvalidParameter` for an argument or an enum value the daemon
  doesn't know: a newer client talking to an older daemon. `parameter` names an
  unknown field, and says which value for an unknown enum value.
- `org.varlink.service.MethodNotFound` for a method the daemon doesn't have.

A test of the forskap-api crate enforces the rules, and CI runs it on every change to
the crate. Each released minor version of the definition is kept, never to be edited,
in [`forskap-api/varlink/snapshots/`](../../forskap-api/varlink/snapshots/): the
definition has to be the snapshot of its own version and keep the rules against every
snapshot of its major version. A change that breaks them fails the test, and so does
a change that comes without a new minor version.

An incompatible change is a new interface under a new name, served next to the old one
on the same socket, so that a client of the old one keeps working.

# Types

The fields are described in the definition; this is what their lines leave out.

## `WorkItem` and `WorkItemRef`

GitLab has made issues, tasks and epics one kind of thing, the *work item*. The wire
follows: every issue the daemon stores is a work item of its project, with the type
GitLab gives it (`issue_type`; a task is a work item of the type `task`, listed among
a project's issues), and every epic a work item of its group, of the type `epic`.
Exactly one of `project_id` and `group_id` is set. The daemon still reads them through
GitLab's REST API (`/issues`, `/groups/:id/epics`); the shape doesn't depend on that.

`id` is the work item's global ID — the ID GitLab's work items API knows, not the
legacy epic ID the REST epics API addresses an epic by. An epic's comes from its
`work_item_id`, which GitLab sends from 18.4 on: forskap needs GitLab 18.4 or newer.
An epic synced by a daemon that didn't store it reads `0` until its group's epics are
synced again, which the upgraded daemon does at its first start.

An issue under an epic names it as `parent`: `group_id`, `iid`, `type` `epic`, its
title and its link. GitLab links an issue's epic relative to the instance; the daemon
answers with the stored epic's `web_url`, else the relative link behind the scheme and
host of the issue's own. An epic has no `parent`. An issue synced by a daemon
that didn't store its epic's group and number has no `parent` until its project is
synced again, likewise at the first start.

## `IssuableKind`

The two things time can be tracked on. Every write that addresses an issue or a
merge request carries an `IssuableKind` next to the `(project_id, iid)` pair, an issue
being a `work_item`; the iid is the per-project number the UI shows (`#42` for
issues, `!7` for MRs). `RecordOpen` takes a `work_item` by its group as well: an epic.

## `FailedTask`

Tasks dead-lettered by a daemon predating MR support render with the current `op`
names (a close reads `"Close"`, never `"CloseIssue"`) and `kind` `work_item`.

## `Project`

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
its project's first download after the upgrade. `avatar` (and the items'
`project_avatar`) is absent until the download ran, for projects you are not a member
of, for images above 1 MiB, and on GitLab before 16.9. The file may be gone if the
cache directory was emptied; the daemon fetches it again at its next start.

## `Group`

An epic belongs to a group and is numbered within it, so `(group_id, iid)` addresses
one. Epics are read-only here: the write methods address a project's work items only.

# Errors

`org.varlink.service.InvalidParameter (parameter: string)` — the call's arguments
don't fit the method: a required one is missing, an enum argument (`IssuableKind`,
`SearchKind`, `WorkItemRole`, `WorkItemState`) carries a value the
interface doesn't have, or an argument object has a field the method doesn't know,
nested ones included (`options` of `Search`, `item` of `CreateWorkItem`). `parameter`
says what is wrong; for an unknown field it is that field's name, `.`-joined below the
top level (`"labels"`, `"options.scope.users"`), for the methods without arguments
too. An argument a newer interface added is thus refused by an older daemon rather
than ignored.

The errors of the interface itself say what the caller does next; each carries a
human-readable `message`.

`InvalidArgument (argument: string, message: string)` — the call fits the interface,
but an argument's value is one the daemon refuses up front: a `project_id`,
`group_id`, `iid` or `options.limit` that isn't positive, a `duration` that is no
GitLab duration, a blank `item.title`, a label containing a comma (`item.labels`), an
`item.parent` that names no epic, a `RecordOpen` reference that names no single item
(`argument` is then the ID given too many, or `project_id` when neither is).
`argument` is the argument's name as the method declares it, a value inside an
argument struct named by its path. Nothing was sent to GitLab, queued or stored, and
the same call fails the same way again; it is answered whatever the session is.

`NotFound (message: string)` — the daemon has no such thing: an `id` that
`RetryFailure` or `DismissFailure` doesn't know (dismissed, retried already, or never
there).

`GitlabError (message: string, status: ?int)` — GitLab answered and refused: 403 (no
permission, an archived project), 404, 400 or 422 (input it won't take), 401 (a token
it no longer takes). `status` is the HTTP status of GitLab's answer; absent where the
daemon has none, an answer it could not read. Nothing was done, and calling again
won't help until what GitLab objected to has changed.

`GitlabUnavailable (message: string)` — GitLab could not be reached, or answered 429
or 5xx, and the daemon did not queue the call: whether GitLab carried it out is
unknown. `CreateWorkItem` replies it (for its parent lookup, which creates nothing,
and for the create itself), and the queued writes for a `PostTime` GitLab answered
with a 5xx: it may have booked the time, so
it is not replayed. Look before calling again — a second `CreateWorkItem` after a lost
answer files the issue twice.

`Internal (message: string)` — the daemon could not do it for a reason of its own:
its storage failed (`RecordOpen`, `RetryFailure`, `DismissFailure`,
`ClearFailures`). Not GitLab's doing; the daemon's log says more.

`NotAuthenticated (reason: ?NotAuthReason, detail: ?string)` — the daemon has no live
GitLab session (it is *dormant*). `reason` says why; `detail` carries free text (host,
underlying error) for the reasons that have one. Both fields are optional so older
daemons that send neither stay compatible — clients fall back to a generic
"run `forskap auth login`" message. The `NotAuthReason`s:

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

### `GetAssignedWorkItems(scope: ?Scope) -> (work_items: []WorkItem)`

Open issues assigned to the authenticated user, work items of their project, served
purely from the cache, grouped by namespace. `scope` keeps the issues in any of its
projects (by `project_id`) or groups (by the namespace of the issue's `web_url`,
subgroups included), the rule `Search` applies to an item; an issue matching several
is listed once. A scope that is omitted, or whose lists are both omitted or empty,
returns everything. When the list has never been synced: replies with an empty list
if a session exists (first sync pending), `NotAuthenticated` otherwise.

### `GetAssignedMergeRequests(scope: ?Scope) -> (merge_requests: []MergeRequest)`

Open merge requests assigned to the authenticated user, served purely from the
cache and synced on the quick cadence like the assigned issues. `scope` keeps them
exactly like `GetAssignedWorkItems`. Replies newest-updated first. When the list has
never been synced: empty list if a session exists, `NotAuthenticated` otherwise.

### `ListWorkItems(filter: ?WorkItemFilter) -> (work_items: []WorkItem)`

Your own issues across projects and states: the ones you authored or are assigned
to that were updated recently, closed ones included, newest-updated first, as work
items of their project. Served purely from the cache. `WorkItem.parent` carries each
one's epic, so a client can, for instance, tell which epic most of your recent work
in a project belongs to.

The `filter`'s `role` picks one of the two lists (`WorkItemRole`); omitted, the reply
is their union, with an issue you both authored and are assigned to listed once.
`states` keeps only issues in one of the given states (omitted or empty = both).
`updated_after` (unix seconds) keeps only issues whose `updated_at` is at or after
it — inclusive, like GitLab's parameter of that name. An omitted `filter` is one with
nothing set.

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

Unlike `GetAssignedWorkItems`, which keeps every open assigned issue however old and
is synced every few minutes, this method is bounded by time and not by state; the two
are synced separately and may briefly disagree. The projects listed here are not
"tracked" by it: an issue closed two months ago pulls no project into the search
corpus. `board_column` is therefore absent for issues of projects whose boards
aren't synced (see `Search`).

When a list the call needs has never been synced — both of them with `role`
omitted: replies with an empty list if a session exists (first sync pending),
`NotAuthenticated` otherwise.

### `Search(query: string, options: ?SearchOptions) -> (work_items: []WorkItem, merge_requests: []MergeRequest, projects: []Project, groups: []Group)`

Searches the locally cached corpus — a pure cache read, no GitLab round-trip.
Matching is a case-insensitive substring test on work item/MR titles and labels and
on project/group names and paths; a query of the exact form `#123` additionally
matches a project's work items and MRs by their per-project number, one of the form
`&5` epics by their per-group number. Descriptions are not cached and not searched.

The `options` narrow the reply; omitted, it is as if none were set. `kinds` restricts
the reply to some of its four arrays (omitted or empty = all four). `limit` caps each
returned array separately (default 50; one that isn't positive replies
`InvalidArgument` naming `options.limit`); issues and epics share `work_items`, so
they share its limit. `types` keeps only the work items of the listed types (`"issue"`, `"task"`,
`"epic"`, …), compared case-insensitively; omitted or empty = every type. A type
nothing has matches nothing.
`exclude_types` leaves out the work items of the listed types, compared the same way;
omitted or empty = none. With both, a work item must be of one of `types` and of none
of `exclude_types`. Both apply before `limit`, so the limit fills from the work items
that remain, and neither touches the other kinds. `"exclude_types": ["epic"]` asks
for every type a project has, the ones GitLab adds later included.

`scope` narrows every kind to the listed projects and groups, applied before
`limit` so a scoped search fills its `limit` from the scope alone. An item passes
if it matches *any* listed criterion: a project's work items and merge requests by
their `project_id` or by the namespace of their `web_url` lying in a group
(subgroups included, as `GetAssignedWorkItems` does); projects by their `id` or their
path lying in a group; groups by their path; epics by the path of their group. What
no listed criterion can name is left out — `projects` alone yields no groups and no
epics. A scope that is omitted, or whose lists are both omitted or empty, is no
scope at all.

**Ranking**: work items and MRs are ordered by their `RecordOpen` statistics — most
opens first, ties by most recent open, then newest-updated — so never-opened items
keep the newest-first order among themselves below the frequently opened ones; issues
and epics are ranked together. Each row reports its count as `open_count`. Projects
and groups are sorted by path. An empty or whitespace-only `query` selects the
"frequently opened" view: only work items and MRs with at least one recorded open,
ranked the same way; projects and groups are empty in that mode. The statistics are read once per call and a read failure degrades to the
plain recency order.

What the corpus contains depends on the `[search]` daemon config. The default
`population = "tracked"` holds the issues and MRs of the member projects you are
active in: where you have assigned issues/MRs, pushed, opened or commented on an issue
or MR, or logged time within `search.tracked_retention_hours` (default 90 days).
Activity in a project you aren't a member of only keeps your assigned items there.
The issues `ListWorkItems` serves are searchable too, wherever they are.
Each project contributes at most its `search.max_items_per_project` most recently
updated issues and MRs. `"member"`
holds every member project's, `"all"` everything the token can see (`"auto"` is an
alias of `"tracked"`). Projects and groups are always membership-scoped.
Epics come from the member groups above those projects, at any depth (`"all"`: from
every member group), each group contributing at most its
`search.max_items_per_project` most recently updated ones. Epics need GitLab Premium
or Ultimate: on other instances there are none among the work items, and the daemon
asks each group only once a day.
Issue `board_column` comes from the synced board columns of the issue's project and
is absent for projects whose boards were never synced (only those of assigned issues'
projects and of tracked member projects are). A project's switched-off features are
left out: no issues or boards where its issues are off, no MRs where its merge requests
are (see *Unavailable jobs* under the admin interface's `GetSyncJobs`).
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
is not the last one; and it re-walks the whole window every
`search.full_interval_secs`, so an event GitLab showed late or hid at the time is
picked up. Events carry no link of their own; `web_url`
and `project_path` come from the stored issue, merge request and project rows and
are absent for events in projects the store doesn't know (the project row exists for
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

A dry run (`forskapd --dry-run`) answers with the host `dry-run.invalid` and the user
`demo`.

### `GetStatus() -> (api_version: string, daemon_version: string, connected: bool, reason: ?NotAuthReason, detail: ?string, host: ?string, username: ?string, user_id: ?int)`

What a client asks first: which version of this interface the daemon speaks and
whether it has a GitLab session. `api_version` is the version of the `forskap-api`
crate the daemon was built with (`forskap_api::API_VERSION` to a Rust client,
`APIVersion` to a Go one; see [Compatibility](#compatibility) for which ones fit);
`daemon_version` is the daemon's own, the one `org.varlink.service.GetInfo` reports,
which says nothing about the interface. A daemon from before forskap-api 0.32.0 has
no `GetStatus` and answers `org.varlink.service.MethodNotFound`.

While `connected`, `host`, `username` and `user_id` are set as `WhoAmI` answers them
and `reason` and `detail` are absent; otherwise `reason` and `detail` are what
`NotAuthenticated` would carry and the other three are absent. Never an error:
served whatever the session is, without a GitLab round-trip.

## Writing (queued when GitLab is away)

The four methods of this section take the target as `(project_id, iid, kind)` —
`kind` selects issue vs merge request; the same operation works on both. They validate the reference
eagerly (`project_id`/`iid` must be positive) and reply `InvalidArgument` on a
malformed one without attempting or queuing anything. On an unreachable session, a
network failure, a 429 or a 5xx the operation is queued for retry and the call
**replies success**, except a `PostTime` GitLab answered with a 5xx: it may have
booked the time, so it is not queued and replies `GitlabUnavailable`. GitLab refusing
it replies `GitlabError`. Other dormancy reasons reply `NotAuthenticated`.

### `PostTime(project_id: int, iid: int, kind: IssuableKind, duration: string, summary: ?string) -> ()`

Records spent time on the issuable. `duration` uses GitLab's time-tracking syntax
(`"1h30m"`, `"45m"`, `"2d"`); an obviously malformed duration replies
`InvalidArgument` up front. `summary` becomes the timelog note.

### `Close(project_id: int, iid: int, kind: IssuableKind) -> ()`

Closes the issuable. Immediately reflected: the assigned lists stop showing it
before the next sync, and `ListWorkItems` shows it as `closed`.

### `AssignSelf(project_id: int, iid: int, kind: IssuableKind) -> ()`

Assigns the authenticated user to the issuable. The assigned list is re-synced
right after the write lands, so it appears within seconds.

### `UnassignSelf(project_id: int, iid: int, kind: IssuableKind) -> ()`

Removes the authenticated user from the issuable's assignees. Immediately
reflected: the assigned lists, and `ListWorkItems` for the `assignee` role, stop
showing it before the next sync.

## Writing directly (never queued)

### `CreateWorkItem(project_id: int, item: NewWorkItem) -> (iid: ?int, web_url: ?string)`

Creates the issue `item` describes in the project and replies with its number and its
link. Its `description` is GitLab Markdown. `labels` are label names; GitLab creates
the ones the project doesn't have yet. `assign_self` assigns the issue to the
authenticated user (omitted: nobody is assigned). `parent` puts the issue under an
epic, named by its `group_id` and `iid` (`type` may be omitted or `epic`; `title` and
`web_url` are ignored); that needs GitLab Premium or Ultimate. GitLab's REST API takes
the epic by its legacy ID, so the daemon reads that from the stored epic, else asks
GitLab for the epic (`GET /groups/:id/epics/:iid`) before creating anything.

Unlike every other write this one is **never queued**: the daemon sends it to GitLab
once and replies with what came of it.

- A blank `title`, a `project_id` that isn't positive, a label containing a comma
  (GitLab takes the labels as one comma-separated list) or a `parent` that names no
  epic (no `group_id`, a `project_id`, a number that isn't positive, another type)
  replies `InvalidArgument` (`item.title`, `project_id`, `item.labels`,
  `item.parent`) without GitLab being asked.
- Without a live session it replies `NotAuthenticated`, whatever the reason —
  `unreachable` too, where the other writes are queued.
- GitLab refusing the create (a 401 included) replies `GitlabError`; a network
  error, a 429 or a 5xx replies `GitlabUnavailable`: the issue may exist all the
  same. None of them demotes the session, and nothing is retried.
- A parent epic GitLab doesn't find replies `GitlabError`, one that can't be looked
  up for a network error, a 429 or a 5xx `GitlabUnavailable`; either way nothing is
  created.

The reason is that a create has no idempotency key. The queued writes address an
existing `(project_id, iid, kind)`; a create has no `iid` yet, and nothing tells a
replay whether an earlier attempt landed. Replayed after a partial success (GitLab
created the issue, the answer got lost), it would file the issue a second time. The
same holds for a caller: after a `GitlabUnavailable`, look before calling again.

On success the issue is visible at once, before any sync: `Search` finds it,
`ListWorkItems` lists it for the `author` role, and if GitLab assigned it to the user,
`GetAssignedWorkItems` and `ListWorkItems` for the `assignee` role list it too. The daemon
goes by GitLab's answer there, not by `assign_self`: a user who may not assign gets
the issue unassigned. A list that was never synced still reads as never synced. The
jobs displaying the issue rerun right after.

Once GitLab created the issue the call replies success, whatever happens then: if
GitLab's answer is unreadable, `iid` and `web_url` are absent.

Work-item status widgets are out of scope (GitLab sets them through GraphQL
`workItemUpdate`, a second call with another API surface): the issue starts in the
project's default status.

## Retry-queue failures (dead letters)

Writes that exhausted their retry window or were rejected while draining land in a
persistent dead-letter store.

### `GetFailures() -> (failures: []FailedTask)`

Lists dead-lettered tasks. Never errors; storage trouble degrades to an empty list.

### `RetryFailure(id: int) -> ()`

Moves a dead-lettered task back into the retry queue. `NotFound` when `id` is
unknown.

### `DismissFailure(id: int) -> ()`

Deletes one dead-lettered task. `NotFound` when `id` is unknown.

### `ClearFailures() -> ()`

Deletes all dead-lettered tasks.

## Usage statistics

### `RecordOpen(kind: IssuableKind, iid: int, project_id: ?int, group_id: ?int) -> ()`

Counts one open of a work item or merge request — the client's "the user just went
there" signal (`forskap issue open`, `forskap epic open`, a launcher activation).
Purely local bookkeeping: no GitLab round-trip, no queueing, works while dormant.
`Search` ranks by these counts and reports them as `open_count` (also on
`GetAssignedWorkItems` / `GetAssignedMergeRequests` rows). An entry expires
`usage.retention_hours` (default 90 days) after its last open, and the record is
capped at 1000 items (lowest counts dropped first); both are enforced on write.
Cleared only by the admin interface's `ClearCache` with the scope `usage`.

Exactly one of `project_id` and `group_id` names where the item lives: a project for
an issue or merge request, a group for an epic (`kind` `work_item`). A group's work
items are counted apart from a project's, so a group and a project sharing an ID
don't share counters. Both or neither given, a merge request by its group, or a
number or ID that isn't positive is an eager `InvalidArgument`.

# Calling from the shell

```sh
SOCKET=unix:$XDG_RUNTIME_DIR/forskapd.socket

# the interface version the daemon speaks, and its session
varlinkctl call $SOCKET org.thehoster.forskapd.GetStatus '{}'

# list assigned issues
varlinkctl call $SOCKET org.thehoster.forskapd.GetAssignedWorkItems '{}'

# the ones in the group team/backend, subgroups included
varlinkctl call $SOCKET org.thehoster.forskapd.GetAssignedWorkItems \
  '{"scope": {"groups": ["team/backend"]}}'

# the issues I authored that were closed, updated since 2026-09-01
varlinkctl call $SOCKET org.thehoster.forskapd.ListWorkItems \
  '{"filter": {"role": "author", "states": ["closed"], "updated_after": 1788220800}}'

# the epics about billing
varlinkctl call $SOCKET org.thehoster.forskapd.Search \
  '{"query": "billing", "options": {"kinds": ["work_items"], "types": ["epic"]}}'

# everything about billing but the epics
varlinkctl call $SOCKET org.thehoster.forskapd.Search \
  '{"query": "billing", "options": {"exclude_types": ["epic"]}}'

# count an open of epic &5 of group 9; oneway: the daemon runs it and answers
# nothing, not even an error
varlinkctl call --oneway $SOCKET org.thehoster.forskapd.RecordOpen \
  '{"kind": "work_item", "iid": 5, "group_id": 9}'

# post 1h30m to project 42, issue #7
varlinkctl call $SOCKET org.thehoster.forskapd.PostTime \
  '{"project_id": 42, "iid": 7, "kind": "work_item", "duration": "1h30m", "summary": "code review"}'

# close merge request !3 in project 42
varlinkctl call $SOCKET org.thehoster.forskapd.Close \
  '{"project_id": 42, "iid": 3, "kind": "merge_request"}'

# introspect the live interface
varlinkctl introspect $SOCKET org.thehoster.forskapd
```

# The `org.thehoster.forskapd.admin` interface

The daemon serves a second interface on the same socket, defined in
[`forskap-api/varlink/org.thehoster.forskapd.admin.varlink`](../../forskap-api/varlink/org.thehoster.forskapd.admin.varlink):
logging in and out, clearing the cache and the sync worker's jobs. It exists for the
bundled `forskap` CLI, and it mirrors the daemon's internals — the cache's bands, the
sync engine's job names, states and progress — which change whenever the sync does.
So it follows the daemon's version (`GetStatus.daemon_version`), not `api_version`,
and carries no stability promise: any daemon release may change or drop what it
has. The [Compatibility](#compatibility) rules don't cover it. The Go binding leaves
it out; the Rust crate has it as `forskap_api::admin`, for the CLI.

```sh
varlinkctl introspect $SOCKET org.thehoster.forskapd.admin
```

Its calls are answered like the main interface's: an argument the method doesn't know
or can't read replies `org.varlink.service.InvalidParameter`, a `oneway` call gets no
reply. It declares the errors its methods reply under its own name
(`org.thehoster.forskapd.admin.GitlabError`, `…GitlabUnavailable`, `…Internal`): a
varlink error belongs to one interface. Each means what the main interface's error of
that name means.

## Sync status

### `GetSyncJobs() -> (jobs: []SyncJob, paused_until: ?int)`

Lists the jobs the sync worker has planned, in the order it runs them: the ones in
flight, the ones demanded ahead of the schedule (a `ClearCache`, a write that just
landed), the due ones by priority, then the rest by `next_due`. The worker runs up to
`sync.max_in_flight` jobs at once, one per project, so several can be `running`,
each with its own `running_since`.

**Progress.** A `running` job says how far its fetch is: `fetched` counts its rows
page by page, `expected` is the total GitLab announced (`X-Total`, at most
`search.max_items_per_project` for a project's or a group's corpus). GitLab announces
none for a listing above 10 000 rows or for the timelogs, and an avatar download has
no rows: `expected` is then absent. Both are rough. A corpus run that reconciles reads
its listing twice (the second time only what changed since it started), and the two
totals add up; GitLab's total can also be off the rows it serves (`/events` counts
rows it then withholds), so `fetched` can end below or above `expected`. `full` tells
a full run from a delta for the jobs that have both: a project's issues and merge
requests, a group's epics, `all/issues`, `all/merge_requests` and `events`.

`paused_until` (unix seconds) is set while a GitLab rate limit (429) holds every
job back; the statuses then say what runs once the pause is over.

Status, not GitLab data: never errors and is served while dormant. A dormant
daemon runs nothing, so its jobs stay `due` until a session exists. `last_error`
is kept in memory only — after a daemon restart a job can be `backing_off` without
one.

**Unavailable jobs.** Some listings GitLab refuses for good, and failing them
forever would only drown the failures that matter. Two mechanisms keep them out:

- *Not planned.* A member project whose issues are switched off
  (`issues_access_level` `disabled`, or `issues_enabled` false on an older instance)
  gets no issues and no board job — boards belong to the issues — and one whose merge
  requests or repository are switched off gets no merge request job. What the project
  doesn't say plans as usual; the next member-projects sync after a change in the
  project's settings adds or drops the jobs (and their rows).
- *Refused three times in a row.* A per-project or per-group listing (`project/<id>/issues`,
  `…/merge_requests`, `…/boards`, `…/avatar`, `group/<id>/epics`) that GitLab answers
  `403` or `404` three times in a row is `unavailable: true`: it rests about a day
  between attempts and reports `waiting` with that `next_due`, its `failures` and (until
  a daemon restart) its `last_error` kept, and the daemon logs its refusals at debug
  level only. An epics listing is unavailable at its first rejection of any status (an
  instance without GitLab Premium has none). Network errors, `429`, `5xx` and `401`
  neither count nor start the count over — they say nothing about the listing; any
  other rejection (`400`, `422`, …) starts it over and backs off as a failure. A
  success, `Login` (it clears every backoff) or a `ClearCache` resetting the job
  (`assigned` for the boards, `search` for the rest, or everything) makes it available
  again; the count is persisted, so a restart doesn't. The account-wide listings
  (assigned lists, events, memberships, timelogs, …) never become unavailable: a
  refusal there means something is wrong with the session.

## Cache control

### `ClearCache(scope: ?[]CacheScope) -> ()`

Clears cached state and makes its sync jobs due at once. Omitted or empty `scope`
clears everything synced. Otherwise each scope selects a slice:

| scope      | clears                                                       |
|------------|--------------------------------------------------------------|
| `assigned` | the assigned issue/MR lists, the `ListWorkItems` lists and the board columns |
| `search` | the corpus: issues, MRs, epics, projects, groups, project avatars |
| `quick`  | history inside the quick window (last `refresh.quick.window_hours`) |
| `slow`   | history between the retention horizon and the quick window     |
| `stale`  | history older than `history.retention_hours` (normally already pruned) |
| `usage`  | the `RecordOpen` statistics — **only when listed explicitly**; the empty "everything" scope leaves them alone (user data, not a cache) |

When a session exists, the reply waits (up to 30 s) until what it cleared is
re-synced: the assigned lists for `assigned`, `search` and the empty scope (plus the
board columns of their projects that never synced), the recent and full history
for a history band and the empty scope. Everything else refills in the
background — the `ListWorkItems` lists among it, so that method can reply empty right
after a clear; `usage` alone makes no GitLab call. Replies success even when
dormant — the cleared state then stays empty until the next successful sync.

To show the refill while waiting, ask `GetSyncJobs` on a second connection: the
daemon answers the calls of one connection one after the other.

## Session

### `Login(host: string, token: string) -> ()`

Connects to `host` with the personal access token, stores the credentials in the OS
keychain, and flips the daemon to connected (waking the retry-queue drain).
`GitlabError` when GitLab rejects the token, `GitlabUnavailable` when it can't be
reached (a network error is tried four times) or answers 429 or 5xx, `Internal` when
the keychain write fails. Prefer `forskap auth login`, which walks through creating a
PAT with the right scopes.

### `Logout() -> ()`

Drops the session (subsequent calls of the main interface reply `NotAuthenticated`
with reason `logged_out`) and deletes the stored credentials from the keychain.
`Internal` when the keychain delete fails; the session is dropped all the same.

A dry run (`forskapd --dry-run`) turns both down with `Internal`, before GitLab or the
session is touched.

## Calling the admin interface from the shell

```sh
# what the sync worker is doing
varlinkctl call $SOCKET org.thehoster.forskapd.admin.GetSyncJobs '{}'

# drop the history and sync it again
varlinkctl call $SOCKET org.thehoster.forskapd.admin.ClearCache \
  '{"scope": ["quick", "slow", "stale"]}'
```
