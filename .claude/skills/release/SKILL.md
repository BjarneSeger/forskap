---
name: release
description: Checklist for cutting a release of forskapd/forskap — completing the CHANGELOG.md section from what was merged, the version and lockfile bump, the checks to run before the tag, what the tag sets off, and what can and can't be repaired once a release is published and immutable.
---

# Cutting a release

A tag `vX.Y.Z` on `main` is the release: `release.yml` builds, attests and publishes
it. **A published release is immutable** (a repository setting): its assets and its tag
are locked, and the tag name is gone for good even if the release is deleted. Only the
title and the notes can still be edited. So everything below happens before the tag is
pushed, and **the tag is pushed only when the user says so**.

Not part of this: the interface (`forskap-api`, tagged `clients/go/v*`; the
`interface-change` skill) and the noctalia plugin (`catalog.toml`) have versions of
their own.

## 1. What went in

```sh
tag=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*')   # the last stable release
gh pr list --state merged --base main --json number,title,body \
  --search "merged:>$(git log -1 --format=%cI "$tag")"
git log --first-parent --no-merges "$tag"..HEAD      # pushed to main directly
git diff "$tag"..HEAD -- forskap-api/Cargo.toml      # a new interface version
```

The PR bodies say what a change means for a user; the commit subjects mostly don't.
GitHub's generated list never shows the direct commits.

## 2. The changelog section

`CHANGELOG.md`, under `## Unreleased`. Complete it; keep what is there already.

- Written for someone who uses the CLI and the daemon: what they can do now, what
  behaves differently, what no longer goes wrong — by command, flag or config key. Not
  how it was built.
- `### Added`, `### Changed` (what a script or a config has to follow), `### Fixed`;
  leave out the empty ones.
- Left out: refactors, tests, CI, docs, formula bumps, and fixes of something no release
  ever had.
- A new interface version gets a closing `Interface: forskap-api X.Y.Z …` line.
- Wrap as usual: `release-notes.sh` joins the lines, which GitHub would render as breaks.

The release notes are this section followed by GitHub's list of the merged PRs, so the
section needn't link them.

## 3. The version

Something under Added → minor; only Fixed → patch. A Changed that breaks a script or a
config: name it to the user and let them pick. A pre-release is `X.Y.Z-rc.N`.

- Rename `## Unreleased` to `## X.Y.Z - YYYY-MM-DD` (today) and put an empty
  `## Unreleased` above it. A pre-release leaves the heading alone: its notes are the
  `Unreleased` section.
- `[workspace.package] version` in `Cargo.toml`, then `cargo update --workspace` for
  `Cargo.lock` (the release builds are `--locked`).

## 4. Before the tag

```sh
bash .github/scripts/release-notes.sh --check
bash .github/scripts/release-notes.sh X.Y.Z     # what the release notes open with
cargo fmt --check && cargo test
gh run list --workflow release.yml --branch main --limit 1    # main's snapshot build
```

`release.yml`'s `check` job refuses a tag whose version isn't `Cargo.toml`'s or that has
no section — but only once the tag is pushed.

## 5. Commit and tag

`chore: Bump version to X.Y.Z` with the changelog, `Cargo.toml` and `Cargo.lock`; tag
`vX.Y.Z` on it. Show the user the section and stop: they push
(`git push origin main vX.Y.Z`), or tell you to.

## 6. The run

`check` → `macos` → `release` (a draft with every asset → the attestation → publish) →
`verify`, and for a stable tag `bump-formula` → `brew`.

- **Failed before "Publish the release"**: a draft is left and nothing is public.
  Re-run the job; it replaces the draft. If it takes a fix in the repository, ask the
  user: the tag is not locked yet and can still be moved, or the fix becomes the next
  patch version.
- **Failed after it** (`verify`, `bump-formula`, `brew`): "Re-run failed jobs" only.
  "Re-run all jobs" fails in `release`, since the release exists and can't be replaced,
  and skips the rest. Then the formula is bumped by hand
  (`bash .github/scripts/bump-formula.sh vX.Y.Z`, commit, push) and tested with
  `gh workflow run brew.yml`.
- **Published and wrong**: notes and title can be edited
  (`gh release edit vX.Y.Z --notes-file …`). Anything else is a new patch version; a
  published tag is never moved, deleted or reused.
- **`verify` says the release is not immutable**: the setting was switched off
  (Settings → General → Releases).
