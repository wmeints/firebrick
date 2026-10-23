# 55. Restart a daemon of another version

## Status

Accepted

## Context

`fbk` reuses whatever `fbkd` listens on the endpoint. After an upgrade, the
daemon of the earlier install keeps running until the user stops it, and the new
CLI sends it requests that it may not understand, or understand differently. The
upgrade instructions asked users to stop the daemon by hand, which is easy to
forget.

`fbk` doesn't know the daemon's process id, and on Windows it starts `fbkd`
without a console (ADR 0050), so no signal can stop it cleanly. Stopping the
daemon also cuts the streams of other `fbk` commands: attached sessions, SSH
connections, and starts and upgrades in progress.

## Decision

The daemon API gets two RPCs. `GetVersion` returns the daemon's package version.
`Shutdown` completes the same graceful shutdown as SIGTERM, and fails with
`FAILED_PRECONDITION` while an `Attach`, `SshTunnel`, `StartSandbox` or
`UpgradeSandbox` stream or an `UpdateNetwork` request is in progress. The daemon
counts these sessions while they run; a start, upgrade or network update counts
until its task is done, also when its client is gone.

`fbkd` holds an exclusive lock on `fbkd.lock` in its data directory for as long
as its process runs, taken with the standard library's `File::try_lock`. The
operating system releases it when the process exits, after the daemon closed its
port forwards and listeners.

After it connects, `fbk` compares the daemon's version with its own. The
versions must match exactly. On a mismatch it asks the daemon to shut down,
waits until it can take the daemon's lock, starts the `fbkd` next to it as it
does when no daemon runs, and checks the version of the new daemon. It fails
when the daemon refuses to shut down, when the daemon predates `GetVersion` and
`Shutdown`, or when the new daemon runs another version, naming the `fbkd`
binary it started.

## Consequences

- Upgrading `fbk` and `fbkd` together no longer needs a manual daemon restart,
  unless sessions are open or the running daemon predates this change.
- `fbk` never cuts another command's session to restart the daemon; the user
  closes the sessions and runs the command again.
- Every command makes one extra request to the daemon.
- Development builds of the same version aren't told apart; restart the daemon
  by hand after changing it.
- A sandbox's port forwards and listeners move to the new daemon the same way as
  after any restart. Waiting for the lock instead of the endpoint matters here:
  the old daemon removes its endpoint before it closes its forwards and
  listeners, and a new daemon that started in between couldn't take them.
- A second `fbkd` now also exits when it can't take the lock, not only when the
  endpoint exists.
