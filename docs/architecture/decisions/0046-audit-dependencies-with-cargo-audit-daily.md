# 46. Audit dependencies with cargo-audit daily

## Status

Accepted

## Context

Firebrick is a security boundary for coding agents, but nothing in the build
checked whether the crates in `Cargo.lock` had known vulnerabilities. A
maintainer only learned about a RustSec advisory against a dependency by
checking by hand. Advisories appear against crates that haven't changed, so a
check that only runs when `Cargo.lock` changes misses most of them.

## Decision

- `.github/workflows/audit.yaml` runs `cargo audit` against `Cargo.lock` with
  the latest RustSec advisory database. cargo-audit is the RustSec project's own
  scanner and does exactly this one check; cargo-deny would add license, ban and
  source checks that nobody asked for.
- The workflow runs on a daily schedule and on manual runs. Pull requests and
  pushes to `main` run it only when they change the workflow itself, following
  [ADR 0036](0036-run-each-ci-workflow-only-for-the-changes-it-checks.md). The
  daily run catches both new dependencies with advisories, within a day of
  merging, and new advisories against dependencies that didn't change, without
  making unrelated pull requests fail on an advisory published that morning.
- A vulnerability advisory fails the job. Unmaintained, unsound and yanked
  warnings are printed but don't fail it, which is `cargo audit`'s default
  without `--deny warnings`: they need a maintainer's judgement, not an
  immediate fix, and would keep the scheduled run red.
- cargo-audit is installed through mise and checksummed in `mise.lock`, like the
  other CI tools
  ([ADR 0043](0043-pin-github-actions-and-ci-tools-to-checksums.md)). The job
  installs only `rust` and cargo-audit, needs no secrets, and reads the
  repository only.
- An advisory that doesn't apply and that no upgrade fixes goes into
  `[advisories] ignore` in `.cargo/audit.toml`, with a comment giving the
  reason.

## Consequences

- GitHub notifies about a failed scheduled run, so a new vulnerability reaches
  the maintainers within a day.
- A dependency with a vulnerability can merge, and the next daily run fails.
- The first run reported RUSTSEC-2023-0071 (the Marvin Attack timing side
  channel in `rsa`), which has no fixed version. russh enables its `rsa` feature
  by default, and firebrick-daemon and microsandbox both depend on russh with
  its default features, so the crate can't be removed. Firebrick and
  microsandbox only create and use Ed25519 keys, so `.cargo/audit.toml` ignores
  it.
- The cargo-audit release has no native macOS arm64 build; mise installs the
  x86_64 binary there, which runs under Rosetta.
