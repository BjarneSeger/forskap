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

// SearchResults groups the per-kind result sets of Search.
type SearchResults struct {
	Issues        []Issue
	MergeRequests []MergeRequest
	Projects      []Project
	Groups        []Group
}

// Search searches the daemon's locally cached corpus (no GitLab round-trip).
// kinds optionally restricts the reply to a subset of "issues", "merge_requests",
// "projects", "groups" (nil = all four); limit caps each result set separately
// (nil = daemon default of 50). Issues and MRs come most-opened first (see
// RecordOpen); an empty query lists only items with recorded opens.
func (c *Client) Search(ctx context.Context, query string, kinds *[]string, limit *int64) (SearchResults, error) {
	issues, mrs, projects, groups, err := Search().Call(ctx, c.conn, query, kinds, limit)
	return SearchResults{issues, mrs, projects, groups}, err
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

// RecordOpen counts one open of an issue or merge request in the daemon's
// local open statistics; Search ranks frequently opened items first and
// reports the count as open_count. Local bookkeeping only: it succeeds while
// the daemon is dormant and never contacts GitLab.
func (c *Client) RecordOpen(ctx context.Context, projectID, iid int64, kind IssuableKind) error {
	return RecordOpen().Call(ctx, c.conn, projectID, iid, kind)
}

// ClearCache clears the daemon's cache, optionally scoped to the given keys
// (nil = clear everything).
func (c *Client) ClearCache(ctx context.Context, scope *[]string) error {
	return ClearCache().Call(ctx, c.conn, scope)
}

// GetHistory returns tracked-time history events, optionally limited to the last
// n days (nil = daemon default window).
func (c *Client) GetHistory(ctx context.Context, days *int64) ([]HistoryEvent, error) {
	return GetHistory().Call(ctx, c.conn, days)
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

// WhoAmI returns the authenticated host and GitLab user id.
func (c *Client) WhoAmI(ctx context.Context) (host string, userID int64, err error) {
	host, userID, _, _, err = WhoAmI().Call(ctx, c.conn)
	return host, userID, err
}

// TokenStatus returns when the daemon's GitLab token expires (unix seconds;
// nil if it never does or the daemon doesn't know yet) and whether the daemon
// rotates it before that. Wraps the varlink method `WhoAmI`.
func (c *Client) TokenStatus(ctx context.Context) (expiresAt *int64, rotates bool, err error) {
	_, _, expiresAt, rotates, err = WhoAmI().Call(ctx, c.conn)
	return expiresAt, rotates, err
}
