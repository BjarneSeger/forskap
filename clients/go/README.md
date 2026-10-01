# Go binding for forskapd

A Go client for the `org.thehoster.forskapd` [varlink](https://varlink.org)
interface exposed by the [`forskapd`](../../forskapd/README.md) daemon.

The low-level types and call helpers are **generated** from the single
source-of-truth interface definition
([`forskap-api/varlink/org.thehoster.forskapd.varlink`](../../forskap-api/varlink/org.thehoster.forskapd.varlink))
using the official [`varlink/go`](https://github.com/varlink/go) generator, so the
binding never drifts from the wire contract. A small hand-written `Client` wraps
those helpers with socket discovery and one method per varlink method.

## Install

```sh
go get github.com/BjarneSeger/forskap/clients/go
```

Releases are tagged `clients/go/vX.Y.Z` and carry the version of the
[`forskap-api`](../../forskap-api/README.md) crate they were generated from, so a
version names one state of the interface. Append `@v0.31.0` to pin one.

The generated package is named after the interface, so import it under an alias:

```go
import forskap "github.com/BjarneSeger/forskap/clients/go"
```

## Usage

```go
ctx := context.Background()

c, err := forskap.Dial(ctx) // resolves the daemon socket like forskap-cli does
if err != nil {
	log.Fatal(err)
}
defer c.Close()

issues, err := c.GetAssignedWorkItems(ctx, nil)
if err != nil {
	var notAuth *forskap.NotAuthenticated
	if errors.As(err, &notAuth) {
		log.Fatal("not authenticated; run: forskap auth login --host gitlab.com")
	}
	log.Fatal(err)
}
for _, is := range issues {
	fmt.Printf("#%d %s\n", is.Iid, is.Title)
}
```

### Connecting

`forskap.Dial` resolves the daemon address with the same precedence as `forskap`:

1. `$FORSKAPD_SOCKET` (used verbatim — include the `unix:` scheme)
2. `unix:$XDG_RUNTIME_DIR/forskapd.socket` (not on macOS)
3. `forskapd/forskapd.socket` in the data directory: `~/Library/Application Support`
   on macOS, elsewhere `$XDG_DATA_HOME` or `~/.local/share`

Use `forskap.DialAddress(ctx, "unix:/path/to.socket")` to point elsewhere, or
`forskap.DefaultAddress()` to inspect what `Dial` would pick.

To check that the daemon speaks an interface this binding can use, ask it first:

```go
status, err := c.GetStatus(ctx)
var missing *varlink.MethodNotFound // github.com/varlink/go/varlink
switch {
case errors.As(err, &missing):
	// a daemon older than v0.32.0: restart it after an upgrade
case err != nil:
	return err
case !status.Compatible():
	// it speaks status.APIVersion, this binding forskap.APIVersion
}
```

`forskap.APIVersion` is the interface version the binding was generated from.
`Compatible` holds while the minor versions match (from 1.0 on: the same major and a
daemon minor at least the binding's); a patch apart is a fix to a binding alone.
`GetStatus` also says whether the daemon has a GitLab session, and never fails for
want of one.

### Errors

Daemon-side errors surface as typed values you match with `errors.As`; each says what
to do next, and all but `NotAuthenticated` carry a `.Message`:

- `*forskap.InvalidArgument` — an argument's value the daemon refuses up front
  (`.Argument` names it); nothing was sent or stored.
- `*forskap.NotFound` — the daemon has no such thing (a failure id it doesn't know).
- `*forskap.GitlabError` — GitLab refused the request; `.Status` is its HTTP status,
  `nil` where the daemon has none.
- `*forskap.GitlabUnavailable` — GitLab was out of reach or answered 429/5xx and the
  daemon did not queue the call: whether it was carried out is unknown.
- `*forskap.Internal` — the daemon failed on its own (keychain, storage), or the
  method is switched off.
- `*forskap.NotAuthenticated` — no valid credentials; `.Reason` is one of the
  `forskap.Reason*` constants (e.g. `forskap.ReasonLoggedOut`), `.Detail` is optional.

```go
var unknown *forskap.GitlabUnavailable
if errors.As(err, &unknown) {
	// look before creating the issue again
}
```

### Optional parameters

Optional varlink parameters are pointers; pass `nil` to omit them
(e.g. `c.GetHistory(ctx, nil)` for the daemon's default window,
`c.Search(ctx, "query", nil, nil, nil, nil, nil)` for all kinds and types, the
default limit and no project/group scope,
`c.ListWorkItems(ctx, nil, nil, nil)` for the issues you authored or are assigned to
in any state, or
`c.PostTime(ctx, pid, iid, forskap.KindWorkItem, "1h", &summary)`). The varlink
`Close` method maps to `c.CloseIssuable` — the Go name `Close` is taken by the
connection releaser. Enum values come from constants: `KindWorkItem` /
`KindMergeRequest` for an `IssuableKind`, `Search*` for the kinds of `Search`,
`Scope*` for the scopes of `ClearCache`, `RoleAuthor` / `RoleAssignee` and
`StateOpened` / `StateClosed` for the role and the states of `ListWorkItems`.

### Work items

Issues, tasks and epics are all `WorkItem`s, told apart by `Type` (`"issue"`,
`"task"`, `"incident"`, `"test_case"`, `"epic"`). An issue lives in a project and
has `Project_id` set, an epic in a group and has `Group_id` set; exactly one of the
two is. `Namespace_path` is the project's or the group's full path (`nil` when
unknown). `Id` is the work item's global ID, the one GitLab's work items API knows. An
issue under an epic carries it as `Parent`, a `WorkItemRef` with the epic's group,
number, title and absolute link.

`c.Search` returns the issues and epics it finds as one list, ranked together under
one limit; pass `types` to keep some of them, `excludeTypes` to leave some out
(before the limit, so it fills from the rest):

```go
epic := []string{"epic"}
epics, err := c.Search(ctx, "billing", nil, nil, nil, &epic, nil)
issues, err := c.Search(ctx, "billing", nil, nil, nil, nil, &epic)
```

`c.RecordOpen` counts an open of an issue or merge request by its project, or of an
epic by its group:

```go
err := c.RecordOpen(ctx, forskap.KindWorkItem, epic.Iid, nil, epic.Group_id)
```

Optional fields of a reply are pointers as well, `nil` when the daemon left them
out. `c.GetSyncJobs` sets `SyncJob.Unavailable` on every job since `v0.28.0`: `true`
for a job GitLab refuses for good (a project's merge requests or boards switched off,
epics without GitLab Premium), which the daemon only asks once a day — no failure to
report. It is `nil` from an older daemon, which doesn't tell.

Since `v0.30.0` a running job says how far it is: `SyncJob.Fetched` is the rows its
fetch has so far, `SyncJob.Expected` the total GitLab announced (`nil` where it
announced none), and `SyncJob.Full` tells a full run from a delta for the jobs that
have both. All three are `nil` on a job that isn't running.

`c.ListWorkItems` lists your own issues across projects, closed ones included,
newest-updated first; each carries its epic as `Parent`. It reaches back the
daemon's `search.tracked_retention_hours` (90 days by default):

```go
role := forskap.RoleAuthor
since := time.Now().AddDate(0, 0, -30).Unix()
closed := []forskap.WorkItemState{forskap.StateClosed}
issues, err := c.ListWorkItems(ctx, &role, &since, &closed)
```

`c.CreateWorkItem` files an issue and returns its number and link (`nil` where GitLab
created it but its answer didn't say them); the issue shows in `Search` and
`ListWorkItems` at once. A `parent` puts it under an epic,
named by its group and number:

```go
assign := true
labels := []string{"bug"}
epic := forskap.WorkItemRef{Group_id: &groupID, Iid: 5}
iid, url, err := c.CreateWorkItem(ctx, projectID, "Fix the login", nil, &labels, &assign, &epic)
```

It is the one write the daemon never queues: while GitLab is unreachable it fails
with `*forskap.NotAuthenticated`, GitLab refusing it (a parent it can't find
included) with `*forskap.GitlabError`, and a network error, a 429 or a 5xx with
`*forskap.GitlabUnavailable`. Don't retry the last blindly — GitLab may have created
the issue before its answer was lost, and a second call files it again.

## Regenerating

The generated files `orgthehosterforskapd.go` and `version.go` (`APIVersion`, from
the version in `forskap-api/Cargo.toml`) are committed and marked `DO NOT EDIT`.
After changing the `.varlink` interface or that version, regenerate them:

```sh
cd clients/go
go generate ./...
```

The generator is pinned via the `tool` directive in `go.mod`, and the
[`Go binding`](../../.github/workflows/go-binding.yml) CI workflow re-runs
`go generate` and fails if a committed file is out of date.

Licensed under either Apache-2.0 or MIT license, at your option.
