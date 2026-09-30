# forskapd

A small daemon that exposes a [varlink](https://varlink.org) IPC socket for GitLab
time tracking with caching. Provides the basis for other tools in this workspace.

## Installation
forskapd provides precompiled releases for arm64 and amd64, with packages for
debian, rpm and arch. See the `releases`-tab.

## Configuration

The daemon reads a TOML config file. Values are layered, highest priority first:

1. `$XDG_CONFIG_HOME/forskapd/config.toml` — your overrides
2. `/usr/share/forskapd/config.toml` — the package-provided default
3. the values baked into the daemon

Every key is optional; a missing key falls back to the next layer. Print an
annotated template with the current defaults:

```sh
cargo run -p forskapd --bin gen-config-template
```

Keys are grouped into TOML tables, one per concern:

| Key | Default | Description |
|---|---|---|
| `[server]` `socket` | `$XDG_RUNTIME_DIR/forskapd.socket` (falls back to `/tmp`) | Varlink Unix socket the daemon listens on. Ignored under systemd socket activation. |
| `[refresh.quick]` `interval_secs` | `300` | Seconds between quick syncs of the assigned issue/MR lists and the recent timelog window (floor 60). |
| `[refresh.quick]` `window_hours` | `24` | How far back the quick timelog sync reaches (last 24h). |
| `[refresh.slow]` `interval_secs` | `86400` | Seconds between slow syncs of the full timelog history, the board columns and your project/group memberships (once a day; floor 60). |
| `[history]` `retention_hours` | `2160` | Total timelog history kept (90 days): synced in full on the slow cadence, anything older is pruned. |
| `[queue]` `base_delay_secs` | `1` | Retry-queue backoff initial delay. |
| `[queue]` `max_delay_secs` | `1800` | Retry-queue backoff cap (30 min). |
| `[queue]` `max_lifetime_secs` | `604800` | How long a task retries before being dead-lettered (7 days). |
| `[queue]` `session_wait_secs` | `30` | Worker sleep while the daemon is dormant (no session). |
| `[queue]` `max_in_flight` | `4` | Most queued writes sent to GitLab at once; writes to the same issue or MR go one at a time, in order (floor 1). |
| `[reconnect]` `enabled` | `true` | Auto-reconnect after an unreachable-GitLab dormancy (down at boot or dropped mid-run). When `false`, recovery is manual (`forskap auth login` or restart). |
| `[reconnect]` `base_delay_secs` | `2` | Auto-reconnect backoff initial delay. |
| `[reconnect]` `max_delay_secs` | `60` | Auto-reconnect backoff cap (1 min); retries continue indefinitely at the cap. |
| `[search]` `population` | `"tracked"` | What the search corpus holds for issues/MRs: `"tracked"` = the member projects you are active in (assignments, pushes, issues, MRs, comments, timelogs — see `tracked_retention_hours`; activity in a non-member project, such as an upstream you contribute to, only keeps your assigned items there), `"member"` = every member project, `"all"` = everything the token can see (`scope=all`; huge on large instances, rejected by gitlab.com). `"auto"` is an alias of `"tracked"`. Projects and groups are always membership-scoped; epics (GitLab Premium and up) come from the member groups above the corpus projects. |
| `[search]` `partial_interval_secs` | `1800` | Seconds between incremental syncs of each corpus project (30 min). Restarting inside this window does not re-poll GitLab. |
| `[search]` `full_interval_secs` | `604800` | Seconds between full resyncs of each corpus project (7 days), which also remove deleted items. |
| `[search]` `tracked_retention_hours` | `2160` | How long your activity in a project (an assignment, a push, an issue, MR or comment, a timelog) keeps it in the `"tracked"` population (90 days). Your contribution events are kept for the same time, so it also bounds how far back `forskap activity` reaches. |
| `[search]` `max_items_per_project` | `1000` | Most issues and most MRs kept per corpus project, and most epics per group — the most recently updated ones (floor 100). Bounds the sync of very large projects. |
| `[sync]` `jitter` | `0.15` | Random spread applied to every sync interval, as a fraction (0–0.5), so jobs sharing an interval don't hit GitLab together. |
| `[sync]` `job_gap_ms` | `250` | Pause between starting two sync jobs (jittered), so a backlog of due jobs trickles out. |
| `[sync]` `max_in_flight` | `2` | Most sync jobs reading from GitLab at once; jobs of the same project go one at a time (floor 1). |
| `[sync]` `startup_spread_secs` | `60` | Window over which jobs already overdue at startup are spread; the assigned lists and recent timelogs always run at once. |
| `[usage]` `retention_hours` | `2160` | How long an issue/MR keeps its `RecordOpen` ranking after its last open (90 days); older entries are dropped on the next recorded open. |
| `[auth]` `rotate` | `"scoped"` | Which tokens are replaced by a fresh one before they expire: `"scoped"` = only tokens with the `self_rotate` scope, `"always"` = also tokens that can rotate through the `api` scope, `"never"` = none. Rotating revokes the token you pasted, which breaks every other tool using it — hence the default. A token without an expiry date is never rotated. |
| `[auth]` `rotate_before_days` | `7` | How many days before its expiry a token is rotated (floor 1). A token living less than three times as long is rotated once a third of its lifetime is left. |

Credentials are configured through the `org.thehoster.forskapd.Login`
interface or by just calling `forskap auth login`.

### Token rotation

A token with an expiry date and the `self_rotate` scope is rotated by the daemon
shortly before it expires (see `[auth]` above): GitLab issues a new token living as
long as the old one did (its default lifetime if the instance refuses that) and
revokes the old one. The new token replaces the old one in the keychain;
`forskap auth status` shows the expiry and whether rotation is active.

Should the keychain refuse the new token, the daemon keeps running on it, logs an
error and keeps retrying the write. If it is restarted before that worked, the
keychain holds the revoked token and `forskap auth login` with a new one is needed.

On machines sharing the keychain entry (iCloud Keychain) one of them rotates; the
others pick the new token up from the keychain once GitLab rejects the old one. Each
daemon rotates up to a day earlier than configured, by a random share of its own, so
two of them rarely rotate before the keychain has synchronized. If they do, the
later rotation revokes the new token and `forskap auth login` is needed.

### Project avatars

The avatars of the projects you are a member of are downloaded to
`$XDG_CACHE_HOME/forskapd/avatars/`, for the launchers to show. Each is fetched once
and again when the project's avatar changes; `forskap sync refresh --scope search`
fetches all of them again. Needs GitLab 16.9 or newer.

Logging can be set by changing the `FORSKAPD_LOG` environment variable to
`trace`, `debug`, `info`, `warn` or `error` (ordered from most to least verbose)

## Checking everything works
```sh
varlinkctl call unix:$XDG_RUNTIME_DIR/forskapd.socket org.thehoster.forskapd.GetAssignedIssues {}
```

## Building locally

### Requirements

- Rust 1.85+
- A GitLab personal access token with at least `read_api` + `write_api` scopes
  (plus `self_rotate` to have it rotated before it expires)

### Build

```sh
cargo build --release
```

### Run

```sh
cargo run --release
```

## Varlink interface

The interface name is `org.thehoster.forskapd`.
For more information, see [the interface docs](docs/varlink_interface.md) and
[the library crate](../forskap-api/README.md).
