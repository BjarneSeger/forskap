package orgthehosterforskapd

// This file is hand-written (NOT generated). It provides an ergonomic Client on
// top of the generated call helpers in orgthehosterforskapd.go: it resolves
// the daemon socket the same way forskap-cli does, opens the varlink connection, and
// exposes one Go method per varlink method. Its signatures are the binding's
// stable surface: a method whose arguments grow takes them in one struct, and
// one with several results returns a struct.

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"

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

// IssuableKind values selecting what a write method or RecordOpen targets,
// mirroring the .varlink enum the same way the NotAuthReason constants do.
// An issue is a work item.
const (
	KindWorkItem     IssuableKind = "work_item"
	KindMergeRequest IssuableKind = "merge_request"
)

// SearchKind values restricting Search to some of its result sets.
const (
	SearchWorkItems     SearchKind = "work_items"
	SearchMergeRequests SearchKind = "merge_requests"
	SearchProjects      SearchKind = "projects"
	SearchGroups        SearchKind = "groups"
)

// WorkItemRole values picking one of the two lists ListWorkItems serves: the
// issues the authenticated user authored, or the ones assigned to them.
const (
	RoleAuthor   WorkItemRole = "author"
	RoleAssignee WorkItemRole = "assignee"
)

// WorkItemState values ListWorkItems filters by; they are the strings
// WorkItem.State carries.
const (
	StateOpened WorkItemState = "opened"
	StateClosed WorkItemState = "closed"
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
//	→ unix:$XDG_RUNTIME_DIR/forskapd.socket (not on macOS)
//	→ unix:<data>/forskapd/forskapd.socket, <data> being
//	  ~/Library/Application Support on macOS, elsewhere $XDG_DATA_HOME or
//	  ~/.local/share
//
// The last two are the daemon's default socket. Without a home directory
// there is none, and the address is empty.
func DefaultAddress() string {
	for _, name := range []string{"FORSKAPD_SOCKET", "GITLAB_TRACKRD_SOCKET"} {
		if s := os.Getenv(name); s != "" {
			return s
		}
	}
	home, _ := os.UserHomeDir()
	if socket := defaultSocket(runtime.GOOS, os.Getenv, home); socket != "" {
		return "unix:" + socket
	}
	return ""
}

// defaultSocket mirrors default_socket of the forskap-api crate, which the
// daemon binds.
func defaultSocket(goos string, getenv func(string) string, home string) string {
	const name = "forskapd.socket"
	xdg := func(key string) string {
		// As the daemon reads them: not on macOS, and absolute paths only.
		if dir := getenv(key); goos != "darwin" && filepath.IsAbs(dir) {
			return dir
		}
		return ""
	}
	if dir := xdg("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, name)
	}
	data := xdg("XDG_DATA_HOME")
	if data == "" {
		if home == "" {
			return ""
		}
		data = filepath.Join(home, ".local", "share")
		if goos == "darwin" {
			data = filepath.Join(home, "Library", "Application Support")
		}
	}
	return filepath.Join(data, "forskapd", name)
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
// the daemon surface as *InvalidArgument, *NotFound, *GitlabError,
// *GitlabUnavailable, *Internal or *NotAuthenticated (match with errors.As);
// optional parameters are pointers, where nil omits the field on the wire.

// GetAssignedWorkItems returns the open issues assigned to the authenticated
// user, work items of their project, optionally only those in any of the
// scope's projects (by ID) or groups (by path, subgroups included); nil or an
// empty scope keeps them all.
func (c *Client) GetAssignedWorkItems(ctx context.Context, scope *Scope) ([]WorkItem, error) {
	return GetAssignedWorkItems().Call(ctx, c.conn, scope)
}

// GetAssignedMergeRequests returns open merge requests assigned to the
// authenticated user, newest-updated first, optionally only those in the
// scope as GetAssignedWorkItems keeps them.
func (c *Client) GetAssignedMergeRequests(ctx context.Context, scope *Scope) ([]MergeRequest, error) {
	return GetAssignedMergeRequests().Call(ctx, c.conn, scope)
}

// ListWorkItems returns the issues the authenticated user authored or is
// assigned to, closed ones included, newest-updated first. The filter's Role
// picks one of the Role* lists (nil = both, each issue once); Updated_after
// (unix seconds, inclusive) keeps only issues updated since; States keeps only
// the given State* ones (nil or empty = both). A nil filter keeps them all.
// Served from what the daemon synced, which reaches back its
// search.tracked_retention_hours (90 days by default) and is refreshed daily.
func (c *Client) ListWorkItems(ctx context.Context, filter *WorkItemFilter) ([]WorkItem, error) {
	return ListWorkItems().Call(ctx, c.conn, filter)
}

// SearchResults groups the per-kind result sets of Search.
type SearchResults struct {
	WorkItems     []WorkItem
	MergeRequests []MergeRequest
	Projects      []Project
	Groups        []Group
}

// Search searches the daemon's locally cached corpus (no GitLab round-trip).
// The options narrow it (nil = none): Kinds restricts the reply to a subset of
// the Search* kinds (nil = all four); Limit caps each result set separately,
// the issues and epics together (nil = daemon default of 50); Scope keeps only
// items in any of its projects (by ID) or groups (by path, subgroups
// included), applied before Limit (nil = everything); Types keeps only the
// work items of the given types and Exclude_types drops the ones of its types,
// both compared case-insensitively ("issue", "task", "epic", …; nil or empty =
// no filter) and applied before Limit. Work items and MRs come most-opened
// first (see RecordOpen); an empty query lists only items with recorded opens.
// Epics need GitLab Premium or Ultimate.
func (c *Client) Search(ctx context.Context, query string, options *SearchOptions) (SearchResults, error) {
	workItems, mrs, projects, groups, err := Search().Call(ctx, c.conn, query, options)
	return SearchResults{workItems, mrs, projects, groups}, err
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

// CreatedWorkItem is the issue CreateWorkItem filed.
type CreatedWorkItem struct {
	// IID and WebURL are its number and link, nil where GitLab created it but
	// its answer didn't say them.
	IID    *int64
	WebURL *string
}

// CreateWorkItem creates the issue item describes in a project. Its Title is
// required; Description (GitLab Markdown), Labels and Parent (an epic, named
// by its Group_id and Iid; GitLab Premium and up) are optional; Assign_self
// assigns the issue to the authenticated user (nil = nobody is assigned).
//
// Unlike the other writes it is never queued: without a live GitLab session
// it fails with *NotAuthenticated, GitLab refusing it with *GitlabError, and
// a network error, a 429 or a 5xx with *GitlabUnavailable. Do not retry the
// last blindly: GitLab may have created the issue before the answer was lost,
// and a second call would file it again. A parent the daemon can't find fails
// before anything is created. On success Search, ListWorkItems and (if GitLab
// assigned it) GetAssignedWorkItems show the issue at once.
func (c *Client) CreateWorkItem(ctx context.Context, projectID int64, item NewWorkItem) (CreatedWorkItem, error) {
	iid, webURL, err := CreateWorkItem().Call(ctx, c.conn, projectID, item)
	return CreatedWorkItem{IID: iid, WebURL: webURL}, err
}

// RecordOpen counts one open of a work item or merge request in the daemon's
// local open statistics; Search ranks frequently opened items first and
// reports the count as open_count. Exactly one of projectID and groupID names
// where it lives: a project for an issue or merge request, a group for an
// epic. Local bookkeeping only: it succeeds while the daemon is dormant and
// never contacts GitLab.
func (c *Client) RecordOpen(ctx context.Context, kind IssuableKind, iid int64, projectID, groupID *int64) error {
	return RecordOpen().Call(ctx, c.conn, kind, iid, projectID, groupID)
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

// GetDescriptionTemplates returns the cached description templates of a
// project, sorted by kind and name, or those of one kind; empty for a
// project whose templates never synced as for one that has none.
func (c *Client) GetDescriptionTemplates(ctx context.Context, projectID int64, kind *IssuableKind) ([]DescriptionTemplate, error) {
	return GetDescriptionTemplates().Call(ctx, c.conn, projectID, kind)
}

// GetFailures returns queued operations that have failed.
func (c *Client) GetFailures(ctx context.Context) ([]FailedTask, error) {
	return GetFailures().Call(ctx, c.conn)
}

// Status is what GetStatus reports.
type Status struct {
	// APIVersion is the version of the interface the daemon speaks, a
	// forskap-api version like the package's own APIVersion; Compatible
	// compares the two.
	APIVersion string
	// DaemonVersion is the daemon's own version, the one
	// org.varlink.service.GetInfo reports.
	DaemonVersion string
	// Connected tells whether the daemon has a GitLab session.
	Connected bool
	// Reason and Detail say why it has none, as a *NotAuthenticated would;
	// nil while Connected.
	Reason *NotAuthReason
	Detail *string
	// Host, Username and UserID name the account; nil unless Connected.
	Host     *string
	Username *string
	UserID   *int64
}

// Compatible tells whether the daemon speaks an interface this binding can use.
// Before 1.0 that takes the same minor version, as every interface change bumps
// it; from 1.0 on the same major and a minor at least the binding's. A version
// that doesn't parse is not compatible.
func (s Status) Compatible() bool {
	return compatible(s.APIVersion, APIVersion)
}

func compatible(daemon, binding string) bool {
	dMajor, dMinor, ok := majorMinor(daemon)
	bMajor, bMinor, bOK := majorMinor(binding)
	if !ok || !bOK || dMajor != bMajor {
		return false
	}
	if bMajor == 0 {
		return dMinor == bMinor
	}
	return dMinor >= bMinor
}

// majorMinor reads "MAJOR.MINOR.PATCH", the patch (and anything after it)
// unchecked.
func majorMinor(version string) (major, minor uint64, ok bool) {
	parts := strings.SplitN(version, ".", 3)
	if len(parts) != 3 {
		return 0, 0, false
	}
	major, err := strconv.ParseUint(parts[0], 10, 64)
	if err != nil {
		return 0, 0, false
	}
	minor, err = strconv.ParseUint(parts[1], 10, 64)
	if err != nil {
		return 0, 0, false
	}
	return major, minor, true
}

// GetStatus returns the interface version the daemon speaks, its own version
// and whether it has a GitLab session. It succeeds whatever the session is; a
// daemon older than v0.32.0 doesn't have it and fails with
// *varlink.MethodNotFound.
func (c *Client) GetStatus(ctx context.Context) (Status, error) {
	var s Status
	var err error
	s.APIVersion, s.DaemonVersion, s.Connected, s.Reason, s.Detail, s.Host, s.Username, s.UserID, err = GetStatus().Call(ctx, c.conn)
	return s, err
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

// Account is the GitLab account WhoAmI reports.
type Account struct {
	Host     string
	Username string
	UserID   int64
	// TokenExpiresAt is when the daemon's token expires, in unix seconds; nil
	// if it never does or the daemon doesn't know yet.
	TokenExpiresAt *int64
	// TokenRotates tells whether the daemon replaces the token before that.
	TokenRotates bool
}

// WhoAmI returns the account the daemon is connected as: the host, the GitLab
// login name and user id, and its token's expiry and rotation. It fails with
// *NotAuthenticated while the daemon has no GitLab session.
func (c *Client) WhoAmI(ctx context.Context) (Account, error) {
	var a Account
	var err error
	a.Host, a.UserID, a.Username, a.TokenExpiresAt, a.TokenRotates, err = WhoAmI().Call(ctx, c.conn)
	return a, err
}
