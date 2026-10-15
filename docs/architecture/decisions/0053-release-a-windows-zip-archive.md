# 53. Release a Windows zip archive

## Status

Accepted

## Context

ADR 0001 left Windows out of the release because `fbk` and `fbkd` used unix
sockets and signals. Since ADR 0050 they run natively on Windows 11 x64, and the
CI `windows` job builds them and runs the unit tests on `windows-latest`.
Windows users could still only install them with Cargo, which needs the Visual
Studio Build Tools and `protoc`. The repository is on a personal GitHub account,
so there's no larger runner with the Windows Hypervisor Platform, and a standard
runner can't boot a VM.

## Decision

The `package` job of `release.yaml` also builds `x86_64-pc-windows-msvc` on
`windows-latest`. It packages `fbk.exe` and `fbkd.exe` in one directory in a
`.zip`, made with the runner's `7z`, because Windows extracts a zip with
Explorer or `Expand-Archive` without extra tools. The checksum is a `.sha256` in
the same format as the other targets. `.cargo/config.toml` sets `-C
target-feature=+crt-static` for `x86_64-pc-windows-msvc`, so the binaries link
the C runtime statically instead of needing the Microsoft Visual C++
Redistributable. microsandbox's `msb.exe` and `libkrunfw.dll` import only
Windows system DLLs, so a release install needs nothing else.

## Consequences

- Windows users install a release without a Rust toolchain or the Visual C++
  Redistributable.
- The setting lives in `.cargo/config.toml` rather than the release workflow, so
  the CI `windows` job builds and tests the same static binaries. It isn't part
  of the published crates, so `cargo install` from crates.io links the C runtime
  dynamically, which the Build Tools it needs provide.
- No workflow starts a sandbox with the Windows binaries before they're
  released, so only a developer's run of the `vm-tests` covers them.
- The Windows build makes the `package` job, and so the release, slower.
- The binaries aren't signed, so SmartScreen may warn about a downloaded archive
  until it's unblocked.
