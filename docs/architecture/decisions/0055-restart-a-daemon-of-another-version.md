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
`UpgradeSandbox` stream is open. The daemon counts these sessions while they
run; a start or upgrade counts until its task is done, also when its client is
gone.

After it connects, `fbk` compares the daemon's version with its own. The
versions must match exactly. On a mismatch it asks the daemon to shut down,
waits until the endpoint is gone, starts the `fbkd` next to it as it does when
no daemon runs, and checks the version of the new daemon. It fails when the
daemon refuses to shut down, when the daemon predates `GetVersion` and
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
  after any restart. The old daemon closes its listeners after it removed its
  endpoint, so for a moment both may hold them.
