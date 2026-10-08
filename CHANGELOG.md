# Changelog

What changed for users of `forskapd` and `forskap`, newest first. The varlink
interface (`forskap-api`, the Go binding in `clients/go`) has versions of its
own; a release that brings a new one names it. Releases up to 1.1.0 are
described on the [releases page](https://github.com/BjarneSeger/forskap/releases).

## Unreleased

## 1.2.0 - 2026-10-08

### Added

- `forskap issue create` opens `$VISUAL` or `$EDITOR` on the description, after
  a picker of the project's GitLab issue templates where it has any.
  `--template <NAME>` names one up front, `--no-edit` keeps the create
  one-shot. The daemon syncs a project's issue and merge request templates.
- `forskap queue list` shows the writes still waiting in the retry queue, each
  with what it waits for, above the failed ones. `forskap time log`, `close`,
  `assign` and `unassign` say when a write was only queued instead of
  reporting it as done.
- `forskap status` and `forskap auth status` say since when the daemon has no
  session, what the latest attempt to get one failed with and when the next
  comes, and whether it ends by itself or needs a login.
- `forskap auth status` and `forskap status` say when the daemon rotates the
  token, or why it doesn't, and warn while a rotated token hasn't reached the
  keychain.
- `forskap sync jobs` says what holds a job back whose turn has come: no
  session, the rate limit's pause, the job running ahead of it, no free slot.
  A job backing off still says why after a daemon restart.
- `forskap sync refresh` names what was not synced again after the clear, and
  why.
- A search provider for rofi: `forskap integration search-provider rofi` as a
  script mode (see the README).
- The text views are coloured by role: titles, project paths, confirmations,
  warnings, dimmed URLs and timestamps. Pipes and `--color never` get the
  same text as before.

### Changed

- `forskap queue list --output json` is one object,
  `{"queued": […], "paused_until": …, "failures": […]}`; it was the bare array
  of failures.

### Fixed

- A daemon started before the desktop session found the keyring locked and
  stayed logged out until a restart. It now waits and connects once the
  keyring is unlocked; writes made meanwhile are queued.
- The daemon no longer loses the keychain when the keyring service is
  replaced under it (a crash, a login starting its own).
- Completions of issue, merge request and epic numbers are scoped to the
  `--project` or `--group` on the line.
- A daemon older than the CLI is reported as such instead of as a
  "Varlink Error".
- `forskap queue list` gave the retry window of a failed write as
  "604800, seconds".

Interface: forskap-api 1.3.0, up from 1.0.0 — `SearchOptions.match_all` (1.1),
`GetDescriptionTemplates` (1.2), `GetQueue`, `queued` in the write replies,
`dormancy` in `GetStatus` and `rotation` in `WhoAmI` (1.3).
