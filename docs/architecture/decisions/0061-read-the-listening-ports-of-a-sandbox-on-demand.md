# 61. Read the listening ports of a sandbox on demand

## Status

Accepted

## Context

When an agent starts a dev server or a debugger in a sandbox, the user has to
know its port before running `fbk port forward`. `fbk port ls` should show the
TCP ports listening in a running sandbox, with the process that owns each one.
It has to work on sandboxes that already run, without a restart, a recreate or a
new image.

There are three ways for `fbkd` to learn about the guest's listeners:

- **A guest agent on vsock**, like the network logger
  ([ADR 0038](0038-record-network-activity-with-a-guest-logger-on-vsock.md)).
  vsock routes are fixed when a sandbox is created, so existing sandboxes would
  need a recreate, and the agent would have to ship in the `firebrick-base`
  image, so it wouldn't work in other images.
- **Polling with an exec** every few seconds per running sandbox, keeping the
  list in `fbkd`. That costs an exec per sandbox per interval, even when nobody
  asks, and is only needed for notifications or forwarding ports automatically,
  which aren't planned.
- **Reading on request.** `exec_stream_with` works on every running sandbox, and
  the guest kernel exposes the listening sockets in `/proc/net/tcp` and
  `/proc/net/tcp6` and their owners through the `socket:[<inode>]` links in
  `/proc/<pid>/fd`.

## Decision

- **Read on demand.** A new `ListPorts` RPC returns the stored forwards of a
  sandbox and, when it runs, the TCP ports listening in its guest right then.
  `fbkd` keeps no state about them between calls.
- **Read procfs, not `ss` or `netstat`.** `fbkd` runs a `sh -c` script as root
  that prints both socket tables and, per process, its `comm` and the `ls -l`
  output of its `fd` directory, using only `cat` and `ls`, which busybox has
  too. Root is needed to read other users' descriptors, such as Docker's.
- **Parse in `fbkd` and treat the output as untrusted.** The agent has root in
  the guest and can change what the script prints. `fbkd` reads at most 1 MiB,
  waits at most 5 seconds, skips lines that don't parse, keeps at most 1024
  ports and 64 characters of a process name, and `fbk` removes control
  characters from the names before printing them. When it drops part of the
  output, it says so in `listeners_truncated`, so `fbk` can warn that the list
  may be incomplete instead of showing a forwarded port as `not listening`.
- **Don't fail the listing.** When the script fails, the forwards are still
  returned, with the reason in `listeners_error`.

## Consequences

- `fbk port ls` works on every running sandbox and image with a POSIX shell,
  including sandboxes created before this change.
- The list is a snapshot. A port that starts listening later shows up on the
  next call. Notifications and forwarding ports automatically would need a
  polling loop or a guest agent.
- Each call runs a short exec in the guest. A guest with many processes forks
  one `ls` per process, which still finishes well within the timeout.
- The process names and ports come from the guest, so an agent can hide or fake
  them. The list is a convenience, not a security control.
- UDP listeners aren't listed.
