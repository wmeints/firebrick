# 45. Approve publishing in a release environment

## Status

Accepted

## Context

[ADR 0043](0043-pin-github-actions-and-ci-tools-to-checksums.md) pins every
action to a commit SHA, but nothing made GitHub enforce the pins, and the
release workflow still published without a human in the loop. A merged version
bump tagged the version and ran `release.yaml` to the end on its own. If an
action in the `image` or `publish` job were compromised, it could push to GHCR
with `packages: write` or mint the OIDC token that crates.io Trusted Publishing
accepts, and ship a malicious release unnoticed. `main` also wasn't protected
against force pushes, so a stolen token could rewrite the commit a release
builds from.

## Decision

- The `image` and `publish` jobs of `release.yaml` run in a `release`
  environment. The environment requires `wmeints` to approve each deployment and
  allows deployments from the `main` branch and `v*.*.*` tags only: a release
  from `tag-release.yaml` runs on `main`, and a tag pushed by hand runs on the
  tag.
- The `package` and `release` jobs stay outside the environment. `package` only
  builds; `release` runs after `publish` and only creates the GitHub release.
- Each crate's two Trusted Publishing configurations on crates.io, for
  `tag-release.yaml` and `release.yaml`, require the `release` environment, so
  crates.io rejects an OIDC token minted by any job outside it.
- The repository requires actions pinned to a full commit SHA
  (`sha_pinning_required`), and the "Protect main branch" ruleset blocks
  deleting and force pushing `main`.
- The environment doesn't limit tags to protected ones. Anyone who can push a
  `v*.*.*` tag can already merge to `main`, and every deployment still needs
  approval, so a tag ruleset would add a setting without closing a path.

## Consequences

- Every release waits for two approvals in the Actions UI: one for `image` and,
  once the image is pushed, one for `publish`. `publish` depends on `image`, so
  its deployment only starts waiting after `image` succeeds. The `package` jobs
  build while the first approval waits.
- A run nobody approves within 30 days fails; rerun the `Release` workflow from
  the Actions UI.
- The settings live outside the repository. A maintainer applies them in the
  order the deployment view lists: the environment before the next release,
  because a job that names a missing environment creates it without reviewers,
  and the SHA pinning only once every workflow uses pinned actions.
- A workflow that references an action by tag fails at startup.
