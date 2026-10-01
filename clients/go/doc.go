// Package orgthehosterforskapd is a Go client for the
// org.thehoster.forskapd varlink interface exposed by the forskapd
// daemon over a Unix socket.
//
// The wire types and low-level call helpers (orgthehosterforskapd.go) are
// generated from the interface definition shared with the Rust crates, and
// APIVersion (version.go) from that definition's version; the Client type
// (client.go) is a thin, hand-written convenience layer that resolves the
// daemon socket and exposes one Go method per varlink method.
//
// Because the generated package name is derived from the interface name, callers
// usually import it under a shorter alias:
//
//	import forskap "github.com/BjarneSeger/forskap/clients/go"
//
//	ctx := context.Background()
//	c, err := forskap.Dial(ctx)
//	if err != nil {
//		log.Fatal(err)
//	}
//	defer c.Close()
//
//	issues, err := c.GetAssignedWorkItems(ctx, nil)
//	if err != nil {
//		var notAuth *forskap.NotAuthenticated
//		if errors.As(err, &notAuth) {
//			log.Fatal("log in first: forskap auth login --host gitlab.com")
//		}
//		log.Fatal(err)
//	}
//	for _, is := range issues {
//		fmt.Println(is.Iid, is.Title)
//	}
//
// Errors returned by the daemon surface as *InvalidArgument, *NotFound,
// *GitlabError, *GitlabUnavailable, *Internal or *NotAuthenticated; match
// them with errors.As. Optional parameters are pointers, where nil omits the
// field on the wire.
package orgthehosterforskapd
