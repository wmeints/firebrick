# 54. Install both binaries with a firebrick crate

## Status

Accepted

## Context

ADR 0031 publishes the workspace crates to crates.io, so users can install
Firebrick with `cargo install firebrick-cli firebrick-daemon`. They have to name
both crates and keep them at the same version, because `fbk` starts `fbkd` from
its own directory. Cargo installs only the binaries of the packages it names,
never those of their dependencies, so a crate that merely depends on the CLI and
the daemon wouldn't install `fbk` or `fbkd`.

## Decision

A `firebrick` crate in `crates/firebrick` owns the `fbk` and `fbkd` binary
targets. Each binary calls the entry point of a library: `firebrick_cli::app`
and `firebrick_daemon::app` hold the code that lived in the `main.rs` files
before. `firebrick-cli` and `firebrick-daemon` become library-only crates, so no
two workspace packages build binaries with the same name. The `msb_home`
integration test, which runs the `fbkd` binary, moves to the `firebrick` crate,
because Cargo only gives a package's tests the paths of its own binaries; the
crate has a `vm-tests` feature of its own for it.

## Consequences

- Users install both binaries at one version with `cargo install firebrick`.
- `cargo install firebrick-cli firebrick-daemon` stops working after 0.8.0,
  because those crates have no binaries anymore. Users who installed that way
  run `cargo uninstall firebrick-cli firebrick-daemon` before they install
  `firebrick`; otherwise Cargo refuses to overwrite binaries another package
  installed.
- The release publishes six crates; `cargo publish` orders `firebrick` after the
  CLI and the daemon.
- The `vm-tests` run for both `firebrick-daemon` and `firebrick`.
- crates.io only accepts Trusted Publishing configurations for a crate that
  exists, so the first `firebrick` release must be published once by hand with
  an API token before the `publish` job of `release.yaml` can publish it.
