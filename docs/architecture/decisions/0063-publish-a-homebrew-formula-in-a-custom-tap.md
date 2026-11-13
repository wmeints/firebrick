# 63. Publish a Homebrew formula in a custom tap

## Status

Accepted

## Context

Installing firebrick meant downloading a release archive, verifying its checksum
and copying `fbk` and `fbkd` into a directory on the `PATH` by hand, or building
both crates with Cargo, which needs a Rust toolchain and `protoc`. Users on
macOS and Linux expect to run `brew install`, and to upgrade and uninstall the
same way (#276).

Homebrew installs formulae from homebrew-core or from a tap, a GitHub repository
named `homebrew-<name>`. homebrew-core builds every formula from source, reviews
each change, and only accepts projects that meet its notability requirements.

## Decision

The release workflow publishes `Formula/firebrick.rb` to a custom tap,
`wmeints/homebrew-tap`, so users run `brew install wmeints/tap/firebrick`:

- **A custom tap, not homebrew-core.** The tap updates on every release without
  waiting for a review, and firebrick doesn't need to meet homebrew-core's
  requirements first.
- **Prebuilt binaries, not a source build.** The formula downloads the release
  archive for the machine, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` or
  `aarch64-unknown-linux-gnu`, pinned to its sha256, and installs `fbk` and
  `fbkd` side by side. A source build would need Rust and `protoc` on every
  machine and build for minutes, and the binaries would differ from the release
  ones that embed the microsandbox runtime. On macOS the formula depends on
  `arch: :arm64`, so Homebrew refuses it on an Intel Mac instead of installing a
  binary that can't run.
- **A scoped token pushing from the release workflow.** A `homebrew` job runs
  after the `release` job, renders the formula with
  `.github/scripts/homebrew-formula.sh` and commits it to the tap's `main`
  branch. It authenticates with `HOMEBREW_TAP_TOKEN`, a fine-grained token with
  only `contents: write` on the tap, stored in the `release` environment, so a
  reviewer approves each push
  ([ADR 0045](0045-approve-publishing-in-a-release-environment.md)). The job's
  `GITHUB_TOKEN` stays at `contents: read`. It skips pre-release tags and
  commits nothing when the formula is unchanged, so rerunning it after a partial
  release succeeds.

## Consequences

- macOS (Apple Silicon) and Linux users install, upgrade and uninstall firebrick
  with Homebrew. Windows users still use the `.zip` archive or Cargo.
- A release takes a third approval in the `release` environment, for the
  `homebrew` job.
- The maintainer owns the `wmeints/homebrew-tap` repository and must renew
  `HOMEBREW_TAP_TOKEN` before it expires; an expired token fails the `homebrew`
  job, and rerunning it with a new token updates the tap.
- The macOS binaries aren't signed or notarized. Homebrew doesn't quarantine the
  files it downloads with `curl`, so they run without removing the quarantine
  flag.
- The formula depends on the archive names and targets of the `package` job.
  Renaming an archive or dropping a target means changing the script too.
