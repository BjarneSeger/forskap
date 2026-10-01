// Code generation for the varlink binding. See doc.go for the package overview.
//
// orgthehosterforskapd.go is generated from the single source-of-truth
// interface definition at
// forskap-api/varlink/org.thehoster.forskapd.varlink, version.go from the
// version in forskap-api/Cargo.toml. Do not edit either by hand; run
// `go generate ./...` after changing the .varlink interface or that version.
//
// The generator writes its output next to its input file and names it after the
// interface, so we copy the .varlink into this module, generate, then remove the
// copy — keeping every write inside clients/go and leaving the api crate untouched.

//go:generate cp ../../forskap-api/varlink/org.thehoster.forskapd.varlink ./interface.varlink
//go:generate go tool varlink-go-interface-generator ./interface.varlink
//go:generate rm ./interface.varlink
//go:generate go run genversion.go

package orgthehosterforskapd
