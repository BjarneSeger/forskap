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

issues, err := c.GetAssignedIssues(ctx, nil)
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
2. `unix:$XDG_RUNTIME_DIR/forskapd.socket`
3. `unix:/tmp/forskapd.socket`

Use `forskap.DialAddress(ctx, "unix:/path/to.socket")` to point elsewhere, or
`forskap.DefaultAddress()` to inspect what `Dial` would pick.

### Errors

Daemon-side errors surface as typed values you match with `errors.As`:

- `*forskap.GitlabError` — an upstream GitLab API error (`.Message`).
- `*forskap.NotAuthenticated` — no valid credentials; `.Reason` is one of the
  `forskap.Reason*` constants (e.g. `forskap.ReasonLoggedOut`), `.Detail` is optional.

### Optional parameters

Optional varlink parameters are pointers; pass `nil` to omit them
(e.g. `c.GetHistory(ctx, nil)` for the daemon's default window,
`c.Search(ctx, "query", nil, nil, nil)` for all kinds, the default limit and no
project/group scope, or
`c.PostTime(ctx, pid, iid, forskap.KindIssue, "1h", &summary)`). The varlink
`Close` method maps to `c.CloseIssuable` — the Go name `Close` is taken by the
connection releaser. Enum values come from constants: `KindIssue` /
`KindMergeRequest` for an `IssuableKind`, `Search*` for the kinds of `Search`,
`Scope*` for the scopes of `ClearCache`.

## Regenerating

The generated file `orgthehosterforskapd.go` is committed and marked
`DO NOT EDIT`. After changing the `.varlink` interface, regenerate it:

```sh
cd clients/go
go generate ./...
```

The generator is pinned via the `tool` directive in `go.mod`, and the
[`Go binding`](../../.github/workflows/go-binding.yml) CI workflow re-runs
`go generate` and fails if the committed file is out of date.

Licensed under either Apache-2.0 or MIT license, at your option.
