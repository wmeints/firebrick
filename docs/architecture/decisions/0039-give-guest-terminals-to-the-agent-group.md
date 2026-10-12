# 39. Give guest terminals to the agent group

## Status

Accepted

## Context

microsandbox's guest agent, agentd, opens the terminal of a TTY exec session as
root and drops to the session user in the child without changing the owner of
the terminal. It mounts devpts without `gid` or `mode` options, so every
`/dev/pts/N` is `root:root` with mode `600`. The session still works through the
file descriptors it inherits, but the `agent` user can't open its own terminal
by path. microsandbox 0.7.8 and its `main` branch behave this way.

Interactive consoles that configure the terminal with `stty -F $(tty)` break.
`mvn quarkus:dev` falls back to its basic console with `stty: Permission
denied`. Ctrl+C then stops Maven but leaves the forked Quarkus JVM running. That
JVM keeps the terminal open, and agentd only reports a session as exited once
every process has closed the terminal, so `exit` in the shell doesn't end `fbk
run` either.

## Decision

The `/sbin/init` of the `firebrick-base` image remounts devpts with `gid=1000`
(the `agent` group) and `mode=0660` before it starts anything else, so every
terminal opened after boot is readable and writable by the `agent` user.

## Consequences

- Programs in a session can open their own terminal by path, as they can on a
  normal Linux host, and Quarkus dev mode stops on Ctrl+C.
- Every process in the `agent` group can open every session terminal, also those
  of root sessions. That grants nothing new: the sandbox has a single user, and
  `agent` has passwordless `sudo`
  ([ADR 0006](0006-run-sandboxes-as-the-agent-user.md)).
- It only works with `init` on. With `init: false`, or a custom image with its
  own init, terminals stay `root:root` `600`. Sandboxes created from an earlier
  image keep the old `/sbin/init` until they're recreated with `fbk rm` and `fbk
  start`.
- agentd accepts sessions before the init runs, so a session opened in the first
  moments of a boot can still get a `root:root` `600` terminal.
- A session still doesn't end while a process it started keeps the terminal
  open, because that's how agentd decides a session has exited.
- Remove the remount once agentd gives session terminals to the session user.
