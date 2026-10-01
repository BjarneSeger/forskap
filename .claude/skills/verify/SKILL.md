---
name: verify
description: Build, launch, and drive forskapd + forskap end-to-end in an isolated environment for verifying daemon/CLI changes at their real surface (varlink socket).
---

# Verify forskapd changes end-to-end

The daemon (`forskapd`) serves a varlink unix socket; the CLI (`forskap`) is
the user surface. Both resolve the socket from `$XDG_RUNTIME_DIR/forskapd.socket`
and data/config/cache from the XDG dirs, so a fully isolated instance only needs env vars.

Two kinds of instance, pick by what the change is about:

- **Dry run** (`forskapd --dry-run`) for client and CLI changes that only need
  plausible data: a demo account in a private temp dir, no keychain, no GitLab, no
  network, nothing of the real daemon's read or written — so write commands are fine
  here. It runs the real sync engine, read handlers and write cascade against an
  in-memory GitLab, so it also covers daemon read/write paths that don't depend on
  GitLab's actual answers.
- **Isolated real instance** (the recipe below) for anything about GitLab's actual
  behaviour: the API's answers, pagination, errors, rate limits, auth, token
  rotation, the keychain.

## Dry run

```bash
cargo build -p forskapd -p forskap-cli
./target/debug/forskapd --dry-run > /tmp/dry.addr 2> /tmp/dry.log &   # stdout: the address, once synced
until [ -s /tmp/dry.addr ]; do sleep 0.1; done
export FORSKAPD_SOCKET=$(head -n1 /tmp/dry.addr)   # unix:/tmp/forskapd-dry-run.XXXXXX/forskapd.socket
./target/debug/forskap issue list
./target/debug/forskap issue close 12 -p acme/backend/api   # writes only change the demo
kill %1                                                      # SIGTERM/SIGINT remove the temp dir
```

- Check `FORSKAPD_SOCKET` names the dry run in every command: without it the CLI
  talks to the production daemon.
- The CLI keeps its own state (last item time was logged on) in the real XDG dirs;
  point `XDG_STATE_HOME`/`XDG_CONFIG_HOME` at a scratch dir for the CLI when that
  matters.
- `Login`/`Logout` are refused; `WhoAmI` answers `@demo` on `dry-run.invalid`.
- SIGKILL leaves `/tmp/forskapd-dry-run.*` behind; remove it by hand.

## Isolated real instance

```bash
cargo build -p forskapd -p forskap-cli

S=$(mktemp -d /tmp/gt-verify.XXXX)          # KEEP SHORT — socket path must fit SUN_LEN (~108 chars)
mkdir -p $S/{config/forskapd,data,cache,runtime}; chmod 700 $S/runtime
export XDG_CONFIG_HOME=$S/config XDG_DATA_HOME=$S/data XDG_CACHE_HOME=$S/cache XDG_RUNTIME_DIR=$S/runtime
# It shares the real keychain entry: it must never rotate the real token.
printf '[auth]\nrotate = "never"\n' > $S/config/forskapd/config.toml

FORSKAPD_LOG=info ./target/debug/forskapd > $S/daemon.log 2>&1 &
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
- **Token rotation**: an isolated real instance must set `[auth]` `rotate = "never"`
  in its config (the recipe writes it). Otherwise it may rotate the real token,
  which revokes the one the production daemon and every other tool use.
- **Stale socket**: after SIGKILL the daemon leaves the socket file; the next
  start replaces it. `AddrInUse` now means another daemon is listening there.
- **Without `XDG_RUNTIME_DIR`** the socket is `$XDG_DATA_HOME/forskapd/forskapd.socket`.
- **Avatars**: files under `$XDG_CACHE_HOME/forskapd/avatars/`. Without the
  override the instance sweeps the real daemon's avatar files (it removes files
  its own store doesn't name).
- **Storage**: fjall database at `$XDG_DATA_HOME/forskapd/db/`.
  Startup deletes legacy `*.redb` files in `$XDG_DATA_HOME/forskapd/`;
  plant fakes there to test the cleanup path.
- Daemon starts dormant (still serving) if the keychain has no credentials;
  reads then serve whatever is cached.

## Private keyring (never the real one)

`gnome-keyring-daemon --unlock` operates on `$XDG_DATA_HOME/keyrings/login.keyring`
and `$XDG_RUNTIME_DIR/keyring/control`. Run with the real dirs it re-keys the
**real** login keyring to the password you feed it and overwrites the real
`service=forskapd` item. Only ever start it inside `dbus-run-session` with every
XDG dir pointed at the scratch dir, in the same shell so the bus dies with it:

```bash
[ "${XDG_DATA_HOME:-$HOME/.local/share}" != "$HOME/.local/share" ] || { echo "refusing: real XDG_DATA_HOME"; exit 1; }
dbus-run-session -- bash -c '
  eval $(echo -n "test-pw" | gnome-keyring-daemon --unlock --components=secrets --daemonize)
  printf "%s" "{\"host\":\"localhost:8930\",\"token\":\"…\"}" \
    | secret-tool store --label "forskapd credentials" service forskapd
  ./target/debug/forskapd > $XDG_DATA_HOME/../daemon.log 2>&1 & D=$!
  # … drive the CLI here …
  kill $D $(pgrep -f "gnome-keyring-daemon --unlock")
'
```

Afterwards `stat ~/.local/share/keyrings/login.keyring` must show an unchanged mtime.
