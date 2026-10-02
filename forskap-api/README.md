# forskap_api
Basic api definitions for communicating with forskapd.

See [varlink](./varlink/) for the definitions, [forskap-cli](../forskap-cli/README.md) for an
example user or [forskapd](../forskapd/README.md) for more information.

The crate root holds `org.thehoster.forskapd`, the interface to build on;
`forskap_api::admin` holds `org.thehoster.forskapd.admin`, the session, cache and sync
calls of the bundled CLI, which follow the daemon's version without a promise of
stability.

The [Go binding](../clients/go/README.md) is generated from the same `.varlink`
definition, so Go consumers stay in lock-step with the wire contract.

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) license, at
your option.
