# 42. Pin GitHub Actions and CI tools to checksums

## Status

Accepted

## Context

The workflows referenced every action by a major version tag, such as
`actions/checkout@v7`. A tag is a movable git ref: whoever controls the action's
repository, legitimately or through a stolen token, can point it at other code,
and the next run executes it. This happened to `tj-actions/changed-files` and
`reviewdog/action-setup` in 2025.

The release workflow is where that matters. Its `publish` job runs
`arduino/setup-protoc` and `jdx/mise-action` with `id-token: write`, so either
action could mint the OIDC token that crates.io Trusted Publishing accepts and
publish its own `firebrick-*` crates. The `package` job runs them before `cargo
build`, so they could backdoor `fbk` and `fbkd`. The `image` and `release` jobs
can push the `firebrick-base` image and replace release assets. The pull request
workflows don't expose this: no workflow runs on `pull_request_target`,
`workflow_run` or `issue_comment`, and fork pull requests get a read-only token.

`jdx/mise-action` installs the tools in `mise.toml` by version, without a
lockfile, so the downloaded Rust toolchain, Node, pnpm and protoc weren't
checked against known checksums either.

## Decision

- Every action is pinned to a full commit SHA, with the release it matches in a
  comment: `actions/checkout@<sha> # v7.0.1`.
- Dependabot (`.github/dependabot.yml`) proposes new SHAs weekly, grouped in one
  pull request, and waits seven days after a release before it does.
- `mise.lock` records the version, download URL and checksum of each tool in
  `mise.toml` for `linux-x64`, `linux-arm64` and `macos-arm64`.
  `jdx/mise-action` installs with `--locked` when the lockfile exists, so CI
  fails on a checksum mismatch or a tool the lockfile doesn't cover.
- Every `jdx/mise-action` step pins mise itself with `version: 2026.9.10`, the
  release that generated `mise.lock`. Without it, each run downloads the newest
  mise, and that binary is what verifies the checksums.
- The `package` and `publish` jobs of the release workflow install only `rust`
  and `protoc` (`install_args`), so no other tool from `mise.toml` runs in a job
  that can mint the crates.io token or build the release binaries.
- The release workflow drops `arduino/setup-protoc`. `jdx/mise-action` already
  installs the `protoc` pinned in `mise.toml`.

## Consequences

- A moved tag no longer changes what the workflows run. Updating an action takes
  a Dependabot pull request, which a maintainer reviews.
- The SHA pin covers the action's own repository only. An action that downloads
  more code at runtime, or a composite action that references other actions by
  tag, can still pull in unpinned code.
- Changing a tool version in `mise.toml` needs `mise lock` to refresh
  `mise.lock`; otherwise CI fails. `mise install` updates an existing lockfile
  on its own. `mise.lock` is a Rust, website and workflow input, so its changes
  run the same checks as `mise.toml`.
- Dependabot doesn't update the mise version in the workflows; raise it by hand
  together with `mise lock`. The mise download isn't checked against a checksum,
  because `jdx/mise-action` takes one `sha256` per step and the release matrix
  spans three platforms.
- `core:rust` lock entries record only the version; rustup verifies the
  toolchain itself.
- Repository settings that also block this, such as requiring SHA-pinned actions
  and a protected environment for the `publish` job, live outside the
  repository.
