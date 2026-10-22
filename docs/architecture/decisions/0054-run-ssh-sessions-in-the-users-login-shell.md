# 54. Run SSH sessions in the user's login shell

## Status

Accepted

## Context

A sandbox has no `sshd`. microsandbox's SSH server runs every session with the
sandbox's shell, `spec.runtime.shell`, which defaults to `/bin/sh`: an
interactive session gets `/bin/sh` and a command runs as `/bin/sh -c <command>`.
It sets no `SHELL`, and the environment only holds the sandbox's variables and
those the client sends with `SetEnv`. `sshd` instead looks up the user's login
shell in `/etc/passwd`, sets `SHELL` to it and starts it.

Clients rely on that. The Codex desktop app refuses a host whose `SHELL` doesn't
point to an executable login shell (issue #222), and an interactive `ssh
<leaf>.fbk` landed in `dash` instead of the `agent` user's `bash`, without the
prompt, completion and `/etc/bash.bashrc` of the image.

Setting `SHELL=/bin/bash` in the `firebrick-base` image would satisfy Codex, but
not custom images or users with another login shell, and commands would still
run in `/bin/sh`. The SSH server has no option for the session's environment or
program.

## Decision

`fbkd` gives every sandbox it creates a `firebrick-login` script, kept in
`crates/daemon/src/firebrick-login.sh` and embedded with `include_str!`, which
microsandbox writes to `/.msb/scripts/firebrick-login` at every start, and sets
it as the sandbox's shell. The script looks up the session user's UID in
`/etc/passwd`, falls back to `/bin/sh` when there's no entry or its shell isn't
executable, exports the shell as `SHELL`, and execs it: with the session's
arguments for a command (`-c <command>`), or with `-l` for an interactive
session. It's POSIX `sh` and reads `/etc/passwd` itself, so it needs neither
`getent` nor `awk` in the image.

## Consequences

- SSH sessions behave like `sshd`'s: `SHELL` is set, commands run in the user's
  login shell, and an interactive session is a login shell.
- Commands sent over SSH run in `bash` instead of `dash` in `firebrick-base`.
  Editors that start their remote servers over SSH expect the login shell, as
  with `sshd`.
- `fbk run` and the daemon's own exec sessions don't use the sandbox's shell and
  are unchanged.
- microsandbox's `modify()` can't change the shell or the scripts of an existing
  sandbox, so sandboxes created before this change keep `/bin/sh` until they're
  recreated, for example with `fbk rm` and `fbk start`, or by `fbk upgrade` when
  their image has a new version.
- The fallback to `/bin/sh` is silent, unlike `sshd`, which refuses the login
  when the user's shell isn't executable. It keeps a sandbox with a broken
  passwd entry reachable, as before this change, so it can be repaired over SSH.
  A warning on stderr would end up in the output of commands that clients such
  as editors parse, and the guest has no channel to the daemon's log.
- An image that needs no SSH sessions still gets the script; it costs one small
  file.
