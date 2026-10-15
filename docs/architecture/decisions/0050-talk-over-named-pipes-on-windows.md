# 50. Talk over named pipes on Windows

## Status

Accepted

## Context

The constraints require firebrick to work on Windows, but `fbk` and `fbkd` only
compiled for Unix: they talk over a unix socket, check each other's UID with
`SO_PEERCRED`, and the network log
([ADR 0038](0038-record-network-activity-with-a-guest-logger-on-vsock.md)) and
the clipboard
([ADR 0048](0048-share-the-host-clipboard-over-a-vsock-route-with-host-tools.md))
reach `fbkd` through vsock routes to unix sockets. microsandbox 0.7.8 runs VMs
on Windows 11 with the Windows Hypervisor Platform (WHP) as a preview. There,
the host end of a vsock route must be a local named pipe (`\\.\pipe\<name>`),
and only stream routes work.

A named pipe differs from a unix socket in ways that matter for security:

- Pipe names are one machine-wide namespace that every user can list. Another
  user can create a pipe under a name before its owner does, and a client then
  connects to that impostor.
- The default access list of a pipe lets every user read it.
- A pipe has no peer credentials. Windows reports the process ID at the other
  end (`GetNamedPipeClientProcessId`, `GetNamedPipeServerProcessId`), and the
  user comes from that process's access token.

Rust and tokio cover the pipes themselves (`tokio::net::windows::named_pipe`),
but not access lists or process tokens. Those need Win32 calls, which are
`unsafe`.

## Decision

`firebrick_utils::ipc` is the one module that knows the transport. It offers
`Listener::bind`, `Listener::accept`, `connect`, `is_served`, `exists`,
`remove`, `endpoint` and `prepare_dir`, with a unix socket behind them on Unix
and a named pipe on Windows. An endpoint stays a `Path` on both, because
microsandbox takes the pipe name as the vsock route's host path. `fbk`, `fbkd`'s
gRPC server and the vsock listeners use only this module.

On Windows:

- `fbkd`'s pipe is `\\.\pipe\firebrick-<SID>-fbkd`. The network log and
  clipboard pipes are `<prefix>-netlog-<sandbox>` and
  `<prefix>-clipboard-<sandbox>`, because a pipe name has no directories. The
  SID keeps the names apart per user and stable across restarts, which matters
  because vsock routes are fixed when a sandbox is created. A
  `FIREBRICK_PIPE_PREFIX` that names a local pipe replaces the
  `\\.\pipe\firebrick-<SID>` prefix, so test daemons don't collide with the
  user's own, the way `XDG_RUNTIME_DIR` isolates them on Unix. Another value, or
  a SID that can't be read, is an error rather than a fallback to a name all
  users share.
- `fbk` starts a daemon only when connecting fails because the endpoint doesn't
  exist, or on Unix only a stale socket file is left. Any other error, such as a
  failed peer check or a pipe that stays busy, ends the command, so `fbk` never
  removes a live daemon's socket or starts a second daemon.
- `Listener::bind` creates the first instance with
  `FILE_FLAG_FIRST_PIPE_INSTANCE`, so it fails with `AddrInUse` when anyone
  already serves the name. Every instance rejects remote clients and has a
  protected access list that grants access to the user and `LocalSystem` only.
- `Listener::accept` refuses a client whose process doesn't run as the user or
  as `LocalSystem`, and `connect` refuses a server like that. This matches the
  Unix check, which trusts the same UID and root.
- The Win32 calls live in `firebrick_utils::win32`, the only module with
  `unsafe` code. Each block has a `HUMAN-APPROVED` comment, as `CLAUDE.md`
  requires. `firebrick-utils` depends on `windows-sys` on Windows, at the
  version tokio already uses, so the build keeps a single copy.

`fbkd` stops on Ctrl-C, Ctrl-Break, closing the console and Windows shutting
down. `fbk` starts it without a console and in a process group of its own, and
with `CREATE_BREAKAWAY_FROM_JOB` when the job object `fbk` runs in allows it.
That way `fbkd` outlives terminals that kill their job when they close, and
microsandbox, which starts every VM with that flag, can still start VMs.

The vm-tests can't run under `cargo test` on Windows. Cargo puts itself and the
test binaries in a job object without the breakaway right, so Windows refuses
microsandbox's `CREATE_BREAKAWAY_FROM_JOB` with "access denied". `mise run
integration-tests` and the stop hook run `scripts/vm-tests.ps1` on Windows
instead. The script builds the tests with `cargo test --no-run` and runs the
binaries itself. GitHub's Windows runners have no WHP, so a Windows CI job only
checks the format, clippy and the unit tests.

## Consequences

- `fbk` and `fbkd` build and run natively on Windows 11 with WHP, with the same
  peer checks as on Unix.
- Another local user can still create a sandbox's network log or clipboard pipe
  while `fbkd` doesn't serve it. `fbkd` then can't bind it and logs a warning.
  The guest, whose route microsandbox opens as a pipe client without checking
  the server, would talk to the impostor
  ([risks](../11-risks-and-technical-debt.md)).
- A pipe can't be half-closed. A clipboard request that ends without its line
  isn't answered with `invalid` on Windows: `fbkd` closes the connection after
  the request timeout instead.
- On Windows the vm-tests run with `mise run integration-tests`, not with the
  plain `cargo test` command.
