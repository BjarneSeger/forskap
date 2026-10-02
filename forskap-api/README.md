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

## Stability

`org.thehoster.forskapd` is stable: within a major version a daemon serves every
method, type and error a client built against an earlier one uses, with the same
types, and only adds to them as the
[Compatibility](../forskapd/docs/varlink_interface.md#compatibility) rules say. A test
of this crate holds every change of the definition to them. The crate's version is
the interface's; `forskap_api::admin` is outside the promise.

A client checks the daemon before relying on it: a daemon of the same major version
and a minor version at least the client's fits.

```rust
use forskap_api::{VarlinkClient, VarlinkClientInterface as _};

let socket = forskap_api::default_socket().expect("a home directory");
let connection =
    varlink::AsyncConnection::with_address(format!("unix:{}", socket.display())).await?;
let status = VarlinkClient::new(connection).get_status().call().await?;
if !forskap_api::compatible(&status.api_version) {
    // the daemon speaks status.api_version, this client forskap_api::API_VERSION
}
```

A daemon that answers `GetStatus` with `org.varlink.service.MethodNotFound` is older
than 0.32.0, from before the interface was stable.

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) license, at
your option.
