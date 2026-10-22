#!/bin/sh
# Login shell of SSH sessions (ADR 0054). fbkd sets this script as the sandbox's shell, so
# microsandbox's SSH server runs it with `-c <command>` for a command and without arguments
# for an interactive session. It does what sshd does: it starts the session user's login
# shell from /etc/passwd with SHELL exported, falling back to /bin/sh.

uid=$(id -u)
SHELL=/bin/sh

# The fields of a passwd line: name, password, UID, GID, GECOS, home and shell.
while IFS=: read -r _ _ id _ _ _ shell; do
    if [ "$id" = "$uid" ] && [ -x "$shell" ]; then
        SHELL=$shell
        break
    fi
done </etc/passwd

export SHELL
[ "$#" -eq 0 ] && exec "$SHELL" -l
exec "$SHELL" "$@"
