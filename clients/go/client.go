package orgthehosterforskapd

// This file is hand-written (NOT generated). It provides an ergonomic Client on
// top of the generated call helpers in orgthehosterforskapd.go: it resolves
// the daemon socket the same way forskap-cli does, opens the varlink connection, and
// exposes one Go method per varlink method.

import (
	"context"
	"os"
	"path/filepath"

	"github.com/varlink/go/varlink"
)

// NotAuthReason values reported inside a *NotAuthenticated error. The generated
// binding models the enum as a bare string; these constants mirror the variants
// declared in the .varlink interface so callers can compare against them.
const (
	ReasonNoCredentials NotAuthReason = "no_credentials"
	ReasonKeychainError NotAuthReason = "keychain_error"
	ReasonUnreachable   NotAuthReason = "unreachable"
	ReasonTokenRejected NotAuthReason = "token_rejected"
	ReasonLoggedOut     NotAuthReason = "logged_out"
)

// IssuableKind values selecting what a write method targets, mirroring the
// .varlink enum the same way the NotAuthReason constants do.
const (
	KindIssue        IssuableKind = "issue"
	KindMergeRequest IssuableKind = "merge_request"
)

// SearchKind values restricting Search to some of its result sets.
const (
	SearchIssues        SearchKind = "issues"
	SearchMergeRequests SearchKind = "merge_requests"
	SearchProjects      SearchKind = "projects"
	SearchGroups        SearchKind = "groups"
	SearchEpics         SearchKind = "epics"
)

// CacheScope values selecting what ClearCache drops: the assigned lists, the
// search corpus, the three age bands of the time history, the open statistics.
const (
	ScopeAssigned CacheScope = "assigned"
	ScopeSearch   CacheScope = "search"
	ScopeQuick    CacheScope = "quick"
	ScopeSlow     CacheScope = "slow"
	ScopeStale    CacheScope = "stale"
	ScopeUsage    CacheScope = "usage"
)

// IssueRole values picking one of the two lists ListIssues serves: the issues
// the authenticated user authored, or the ones assigned to them.
const (
	RoleAuthor   IssueRole = "author"
	RoleAssignee IssueRole = "assignee"
)

// IssueState values ListIssues filters by; they are the strings Issue.State
// carries.
const (
	StateOpened IssueState = "opened"
	StateClosed IssueState = "closed"
)

// HistorySource values telling a synced timelog from a PostTime still queued.
const (
	SourceGitlab HistorySource = "gitlab"
	SourceQueued HistorySource = "queued"
)

// DefaultAddress resolves the forskapd varlink address using the same
// precedence as forskap-cli:
//
//	$FORSKAPD_SOCKET, if set (used verbatim, so include the "unix:" scheme)
//	→ $GITLAB_TRACKRD_SOCKET, its name before the rename
//	→ unix:$XDG_RUNTIME_DIR/forskapd.socket
//	→ unix:/tmp/forskapd.socket
func DefaultAddress() string {
	for _, name := range []string{"FORSKAPD_SOCKET", "GITLAB_TRACKRD_SOCKET"} {
		if s := os.Getenv(name); s != "" {
			return s
		}
	}
	if x := os.Getenv("XDG_RUNTIME_DIR"); x != "" {
		return "unix:" + filepath.Join(x, "forskapd.socket")
	}
	return "unix:/tmp/forskapd.socket"
}

// Client is a connected varlink client for the org.thehoster.forskapd
// interface. Create one with Dial or DialAddress and release it with Close.
type Client struct {
	conn *varlink.Connection
}

// Dial connects to the daemon at DefaultAddress.
func Dial(ctx context.Context) (*Client, error) {
	return DialAddress(ctx, DefaultAddress())
}

// DialAddress connects to the daemon at an explicit varlink address, e.g.
// "unix:/run/user/1000/forskapd.socket" or "tcp:127.0.0.1:12345".
func DialAddress(ctx context.Context, address string) (*Client, error) {
	conn, err := varlink.NewConnection(ctx, address)
	if err != nil {
		return nil, err
	}
	return &Client{conn: conn}, nil
}

// Close releases the underlying connection.
func (c *Client) Close() error {
	return c.conn.Close()
}

// Methods below map one-to-one onto the varlink interface. Errors returned by
// the daemon surface as *GitlabError or *NotAuthenticated (match with errors.As);
// optional parameters are pointers, where nil omits the field on the wire.

// GetAssignedIssues returns issues assigned to the authenticated user, optionally
// filtered to the given group paths (nil = all groups).
func (c *Client) GetAssignedIssues(ctx context.Context, groups *[]string) ([]Issue, error) {
	return GetAssignedIssues().Call(ctx, c.conn, groups)
}

// GetAssignedMergeRequests returns open merge requests assigned to the
// authenticated user, optionally filtered to the given group paths (nil = all
// groups). Served from the daemon's search corpus, so freshness follows the
// search sync cadence.
func (c *Client) GetAssignedMergeRequests(ctx context.Context, groups *[]string) ([]MergeRequest, error) {
	return GetAssignedMergeRequests().Call(ctx, c.conn, groups)
}

// ListIssues returns the issues the authenticated user authored or is assigned
// to, closed ones included, newest-updated first. role picks one of the Role*
// lists (nil = both, each issue once); updatedAfter (unix seconds, inclusive)
// keeps only issues updated since; states keeps only the given State* ones (nil
// or empty = both). Served from what the daemon synced, which reaches back its
// search.tracked_retention_hours (90 days by default) and is refreshed daily.
func (c *Client) ListIssues(ctx context.Context, role *IssueRole, updatedAfter *int64, states *[]IssueState) ([]Issue, error) {
	return ListIssues().Call(ctx, c.conn, role, updatedAfter, states)
}

// SearchResults groups the per-kind result sets of Search.
type SearchResults struct {
	Issues        []Issue
	MergeRequests []MergeRequest
	Projects      []Project
	Groups        []Group
	Epics         []Epic
}

// Search searches the daemon's locally cached corpus (no GitLab round-trip).
// kinds optionally restricts the reply to a subset of the Search* kinds (nil =
// all five); limit caps each result set separately (nil = daemon default of 50);
// scope keeps only items in any of its projects (by ID) or groups (by path,
// subgroups included), applied before limit (nil = everything). Issues, MRs
// and epics come most-opened first (see RecordOpen, RecordEpicOpen); an empty
// query lists only items with recorded opens. Epics need GitLab Premium or
// Ultimate.
func (c *Client) Search(ctx context.Context, query string, kinds *[]SearchKind, limit *int64, scope *SearchScope) (SearchResults, error) {
	issues, mrs, projects, groups, epics, err := Search().Call(ctx, c.conn, query, kinds, limit, scope)
	return SearchResults{issues, mrs, projects, groups, epics}, err
}

// PostTime logs a time-tracking entry on an issue or merge request (per kind).
// summary is optional (nil to omit).
func (c *Client) PostTime(ctx context.Context, projectID, iid int64, kind IssuableKind, duration string, summary *string) error {
	return PostTime().Call(ctx, c.conn, projectID, iid, kind, duration, summary)
}

// CloseIssuable closes an issue or merge request (per kind). Wraps the varlink
// method `Close`; the Go name differs because Close is taken by the
// connection-releasing io.Closer method above.
func (c *Client) CloseIssuable(ctx context.Context, projectID, iid int64, kind IssuableKind) error {
	return Close().Call(ctx, c.conn, projectID, iid, kind)
}

// AssignSelf assigns the authenticated user to an issue or merge request.
func (c *Client) AssignSelf(ctx context.Context, projectID, iid int64, kind IssuableKind) error {
	return AssignSelf().Call(ctx, c.conn, projectID, iid, kind)
}

// UnassignSelf removes the authenticated user from an issuable's assignees.
func (c *Client) UnassignSelf(ctx context.Context, projectID, iid int64, kind IssuableKind) error {
	return UnassignSelf().Call(ctx, c.conn, projectID, iid, kind)
}

// CreateIssue creates an issue in a project and returns its number and link.
// description (GitLab Markdown), labels and epicID (the epic's global ID, its
// Epic.ID; GitLab Premium and up) are optional; assignSelf assigns the issue
// to the authenticated user (nil = nobody is assigned).
//
// Unlike the other writes it is never queued: without a live GitLab session
// it fails with *NotAuthenticated, and any failure of the request, a network
// error included, is a *GitlabError. Do not retry such a failure blindly:
// GitLab may have created the issue before the answer was lost, and a second
// call would file it again. On success Search, ListIssues and (if GitLab
// assigned it) GetAssignedIssues show the issue at once.
func (c *Client) CreateIssue(ctx context.Context, projectID int64, title string, description *string, labels *[]string, assignSelf *bool, epicID *int64) (iid int64, webURL string, err error) {
	return CreateIssue().Call(ctx, c.conn, projectID, title, description, labels, assignSelf, epicID)
}

// RecordOpen counts one open of an issue or merge request in the daemon's
// local open statistics; Search ranks frequently opened items first and
// reports the count as open_count. Local bookkeeping only: it succeeds while
// the daemon is dormant and never contacts GitLab.
func (c *Client) RecordOpen(ctx context.Context, projectID, iid int64, kind IssuableKind) error {
	return RecordOpen().Call(ctx, c.conn, projectID, iid, kind)
}

// RecordEpicOpen is RecordOpen for an epic, which belongs to a group and is
// addressed by (groupID, iid).
func (c *Client) RecordEpicOpen(ctx context.Context, groupID, iid int64) error {
	return RecordEpicOpen().Call(ctx, c.conn, groupID, iid)
}

// ClearCache clears the daemon's cache, optionally only the given Scope* slices
// (nil = everything but the open statistics).
func (c *Client) ClearCache(ctx context.Context, scope *[]CacheScope) error {
	return ClearCache().Call(ctx, c.conn, scope)
}

// GetHistory returns tracked-time history events, optionally limited to the last
// n days (nil = daemon default window).
func (c *Client) GetHistory(ctx context.Context, days *int64) ([]HistoryEvent, error) {
	return GetHistory().Call(ctx, c.conn, days)
}

// GetActivity returns the user's contribution events (pushes, comments,
// opened and merged items), newest first, optionally limited to the last n
// days (nil = daemon default window). Served from what the daemon synced.
func (c *Client) GetActivity(ctx context.Context, days *int64) ([]ActivityEvent, error) {
	return GetActivity().Call(ctx, c.conn, days)
}

// GetFailures returns queued operations that have failed.
func (c *Client) GetFailures(ctx context.Context) ([]FailedTask, error) {
	return GetFailures().Call(ctx, c.conn)
}

// GetSyncJobs returns the daemon's planned sync jobs in the order its worker
// runs them, and until when (unix seconds) a GitLab rate limit pauses them all
// (nil = not paused). Status only: it succeeds while the daemon is dormant.
func (c *Client) GetSyncJobs(ctx context.Context) (jobs []SyncJob, pausedUntil *int64, err error) {
	return GetSyncJobs().Call(ctx, c.conn)
}

// RetryFailure re-enqueues a previously failed task by id.
func (c *Client) RetryFailure(ctx context.Context, id int64) error {
	return RetryFailure().Call(ctx, c.conn, id)
}

// DismissFailure discards a failed task by id.
func (c *Client) DismissFailure(ctx context.Context, id int64) error {
	return DismissFailure().Call(ctx, c.conn, id)
}

// ClearFailures discards all failed tasks.
func (c *Client) ClearFailures(ctx context.Context) error {
	return ClearFailures().Call(ctx, c.conn)
}

// Login stores credentials for a GitLab host in the daemon.
func (c *Client) Login(ctx context.Context, host, token string) error {
	return Login().Call(ctx, c.conn, host, token)
}

// Logout clears stored credentials.
func (c *Client) Logout(ctx context.Context) error {
	return Logout().Call(ctx, c.conn)
}

// WhoAmI returns the authenticated host, GitLab login name and user id.
func (c *Client) WhoAmI(ctx context.Context) (host, username string, userID int64, err error) {
	host, userID, username, _, _, err = WhoAmI().Call(ctx, c.conn)
	return host, username, userID, err
}

// TokenStatus returns when the daemon's GitLab token expires (unix seconds;
// nil if it never does or the daemon doesn't know yet) and whether the daemon
// rotates it before that. Wraps the varlink method `WhoAmI`.
func (c *Client) TokenStatus(ctx context.Context) (expiresAt *int64, rotates bool, err error) {
	_, _, _, expiresAt, rotates, err = WhoAmI().Call(ctx, c.conn)
	return expiresAt, rotates, err
}
