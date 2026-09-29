---
name: verify
description: Build, launch, and drive forskapd + forskap end-to-end in an isolated environment for verifying daemon/CLI changes at their real surface (varlink socket).
---

# Verify forskapd changes end-to-end

The daemon (`forskapd`) serves a varlink unix socket; the CLI (`forskap`) is
the user surface. Both resolve the socket from `$XDG_RUNTIME_DIR/forskapd.socket`
and data/config from the XDG dirs, so a fully isolated instance only needs env vars.

## Recipe

```bash
cargo build -p forskapd -p forskap-cli

S=$(mktemp -d /tmp/gt-verify.XXXX)          # KEEP SHORT — socket path must fit SUN_LEN (~108 chars)
mkdir -p $S/{config,data,runtime}; chmod 700 $S/runtime
export XDG_CONFIG_HOME=$S/config XDG_DATA_HOME=$S/data XDG_RUNTIME_DIR=$S/runtime

RUST_LOG=info ./target/debug/forskapd > $S/daemon.log 2>&1 &
# wait for $S/runtime/forskapd.socket to appear, then drive:
./target/debug/forskap issue list      # issue cache read
./target/debug/forskap time history    # timelog history read
./target/debug/forskap queue list      # dead-letter listing
./target/debug/forskap sync refresh    # clears caches + re-fetches
```

## Gotchas

- **Credentials come from the OS keychain, not XDG** — the daemon will use the
  real `forskap auth login` credentials and talk to the real GitLab (read-only refresh
  fetches). Avoid driving write commands (`forskap time log`, `forskap issue close`, `forskap mr assign`)
  unless the write target is intentional; they post to the live GitLab.
- **Stale socket**: after SIGKILL the daemon leaves the socket file and a
  restart dies with `AddrInUse` — Use SIGTERM or `rm` the socket before restarting.
- **Storage**: fjall database at `$XDG_DATA_HOME/forskapd/db/`.
  Startup deletes legacy `*.redb` files in `$XDG_DATA_HOME/forskapd/`;
  plant fakes there to test the cleanup path.
- Daemon starts dormant (still serving) if the keychain has no credentials;
  reads then serve whatever is cached.
