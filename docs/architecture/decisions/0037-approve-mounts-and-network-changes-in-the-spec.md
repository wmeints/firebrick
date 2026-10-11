# 37. Approve mounts and network changes in the spec

## Status

Accepted

## Context

`.firebrick.yml` lives in the workspace, which the sandbox mounts read/write, so
the agent can edit it. `fbk start` and `fbk run` applied its `mounts` and
`network` sections when they created a sandbox, and `fbk network` sent the whole
network section from the file
([ADR 0021](0021-recreate-sandboxes-from-a-disk-snapshot-to-change-their-network-rules.md)).
An agent could add `mounts: [{host: "~", guest: /mnt/home}]` or set
`network.enforce: false`, and the user's next `fbk rm --force && fbk start` or
`fbk network allow <rule>` gave the sandbox the user's home directory or removed
the egress policy.

Blocking edits to the file isn't possible. microsandbox 0.7.8 mounts a host
directory read-only or read/write as a whole, so the spec file can't be
read-only while the rest of the workspace stays writable. A read-only overmount
of the file inside the guest can be undone, because the `agent` user has
passwordless sudo and is in the `docker` group
([ADR 0006](0006-run-sandboxes-as-the-agent-user.md),
[ADR 0016](0016-run-docker-in-the-sandbox-vm.md)).

## Decision

`fbk` records the `mounts` and `network` settings the user approved for each
workspace in `$XDG_CONFIG_HOME/firebrick/approved.yml`, falling back to
`$HOME/.config/firebrick/approved.yml`. The file lives outside every directory a
sandbox mounts, unless the user approves a mount of the config directory itself.
It maps the canonical workspace path to the resolved mounts, with their
canonical host paths, and the network section. `fbk` writes it with mode `0600`
through a temp file and a rename. Without an absolute `XDG_CONFIG_HOME` or
`HOME`, `fbk` fails instead of falling back to a temp or working directory.

Before `fbk start` or `fbk run` creates a sandbox, and before `fbk network`
sends a network section, `fbk` compares the settings with the approved ones. It
prints every added or removed mount and every changed network field, and asks
`Apply these changes? [y/N]`. `fbk network` only asks when the section differs
from the approved one with the user's own change applied, so the change the user
typed needs no second confirmation. A workspace without an entry gets no mounts
and the default network section without asking; any other settings are listed in
full and asked about once, because `fbk` can't know what a sandbox created
before this change enforced. `--yes` approves without asking, and without a
terminal on stdin and without `--yes` the command fails.

## Consequences

- A hand edit to `mounts` or `network`, by the user or the agent, needs one
  confirmation before it reaches a sandbox. `fbk network` still sends the whole
  section, as ADR 0021 requires, so a hand edit still catches up.
- Scripts that create sandboxes from an edited spec need `--yes`, which approves
  whatever the file contains.
- The other fields of the spec (`image`, `init`, `mise`, `resources`, `volumes`,
  `ports` and `name`) still apply without approval.
- Starting an existing sandbox doesn't check anything, because it doesn't apply
  `mounts` or `network`.
- The approvals belong to the host user, not to the workspace, so a new clone or
  another machine asks again.
