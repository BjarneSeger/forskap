package orgthehosterforskapd_test

import (
	"context"
	"errors"
	"fmt"
	"log"

	forskap "github.com/BjarneSeger/forskap/clients/go"
)

// This example connects to a running forskapd daemon and lists the issues
// assigned to the authenticated user. It has no // Output: comment, so `go test`
// compiles it (guarding the public API) without executing it.
func Example() {
	ctx := context.Background()

	c, err := forskap.Dial(ctx)
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

	mrs, err := c.GetAssignedMergeRequests(ctx, nil)
	if err != nil {
		log.Fatal(err)
	}
	for _, mr := range mrs {
		fmt.Printf("!%d %s %v\n", mr.Iid, mr.Title, mr.Assignees)
	}

	// The issues I authored that are closed by now, newest-updated first.
	role := forskap.RoleAuthor
	closed := []forskap.WorkItemState{forskap.StateClosed}
	mine, err := c.ListWorkItems(ctx, &role, nil, &closed)
	if err != nil {
		log.Fatal(err)
	}
	for _, is := range mine {
		if is.Parent != nil {
			fmt.Printf("#%d %s (epic &%d)\n", is.Iid, is.Title, is.Parent.Iid)
		}
	}

	// The epics about billing.
	epics := []string{"epic"}
	res, err := c.Search(ctx, "billing", nil, nil, nil, &epics, nil)
	if err != nil {
		log.Fatal(err)
	}
	for _, e := range res.WorkItems {
		fmt.Printf("&%d %s\n", e.Iid, e.Title)
	}

	// Everything else about billing: tasks and the types to come included.
	res, err = c.Search(ctx, "billing", nil, nil, nil, nil, &epics)
	if err != nil {
		log.Fatal(err)
	}
	for _, w := range res.WorkItems {
		fmt.Printf("#%d %s (%s)\n", w.Iid, w.Title, w.Type)
	}
}

// Creating an issue is never queued, so its errors say what to do next.
func ExampleClient_CreateWorkItem() {
	ctx := context.Background()

	c, err := forskap.Dial(ctx)
	if err != nil {
		log.Fatal(err)
	}
	defer c.Close()

	labels := []string{"bug"}
	iid, url, err := c.CreateWorkItem(ctx, 42, "Fix the login", nil, &labels, nil, nil)
	var (
		invalid *forskap.InvalidArgument
		refused *forskap.GitlabError
		unknown *forskap.GitlabUnavailable
	)
	switch {
	case errors.As(err, &invalid):
		log.Fatalf("%s: %s", invalid.Argument, invalid.Message)
	case errors.As(err, &refused):
		log.Fatalf("GitLab refused: %s", refused.Message)
	case errors.As(err, &unknown):
		// GitLab may have created it before the answer was lost.
		log.Fatalf("look for the issue before creating it again: %s", unknown.Message)
	case err != nil:
		log.Fatal(err)
	}
	if iid != nil && url != nil {
		fmt.Printf("#%d %s\n", *iid, *url)
	}
}
