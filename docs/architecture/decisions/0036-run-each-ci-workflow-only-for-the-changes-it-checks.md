# 36. Run each CI workflow only for the changes it checks

## Status

Accepted

## Context

`ci.yaml` ran on every pull request and push to `main`, including changes that
touched only the website or the docs, and spent more than five minutes on the
Rust build, clippy, the unit tests and the microVM integration tests. It also
ran `dprint check` and `actionlint`, which don't depend on the Rust code.

Within one job, cargo compiled `microsandbox` and the crates that depend on it
in each step, even with a warm `Swatinem/rust-cache`. `cargo check -v` names the
cause: `Dirty microsandbox v0.7.8: the file /tmp/firebrick-msb/bin/msb has
changed (… after last build …)`. The `microsandbox` build script installs the
`msb` runtime into `MSB_HOME` and prints `cargo:rerun-if-changed` for its files.
On a fresh runner `MSB_HOME` is empty, so the first cargo command runs the
script, which installs the runtime. Cargo dates the run to when it started, so
the runtime's files are newer than the run, and the next cargo command reruns
the script and recompiles `microsandbox`. `MSB_HOME` stays fixed at
`/tmp/firebrick-msb` (see `.cargo/config.toml`).

## Decision

- Each workflow has a `paths` filter on `push` and `pull_request` for the files
  it checks and its own file. `ci.yaml` runs for the Rust inputs (`crates/**`,
  the `Cargo` files, `.cargo/**`, `clippy.toml`, `mise.toml`).
- `dprint check` moves to `markdown.yaml` and `actionlint` to `actionlint.yaml`.
  They install only their own tools from `mise.toml`, not the Rust toolchain.
  Two workflows instead of one lint workflow, because GitHub filters paths per
  workflow, not per job.
- `ci.yaml` caches the runtime in `/tmp/firebrick-msb/bin` and
  `/tmp/firebrick-msb/lib` with `actions/cache`, keyed on `Cargo.lock`, and
  dates the restored files back to 2000-01-01 before the first cargo command, so
  they're older than any cached build script run.
- The `check` job drops its build step: clippy and the unit tests compile every
  target. The `integration` job restores the `check` job's rust-cache through a
  shared key instead of keeping its own.
- Lefthook runs clippy and the unit tests only for commits that stage Rust
  inputs, matching `.claude/hooks/stop-checks.sh`.

## Consequences

- A website-only or docs-only pull request runs no `CI` jobs, and a Markdown
  change outside `website/` runs only `dprint check`.
- With a warm cache, the `check` job compiles only the workspace crates, which
  rust-cache doesn't cache. A `Cargo.lock` change misses both caches and
  compiles `microsandbox` once per profile, as before.
- The runtime cache stores about 70 MB per `Cargo.lock`. GitHub evicts unused
  entries after seven days.
- A new file that a workflow checks must be added to its `paths` filter, or
  changes to it won't run the workflow. Without required status checks on
  `main`, a skipped workflow doesn't block a merge.
