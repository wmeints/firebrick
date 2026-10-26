# 56. Apply mise dotfiles from a read-only mount

## Status

Accepted

## Context

Developers keep their shell, git and editor setup in a dotfiles repository, and
[mise dotfiles](https://mise.jdx.dev/dotfiles.html) can deploy it: `mise dot
apply` links or copies the entries of the `[dotfiles]` table in a trusted
`mise.toml` into the home directory. Every new sandbox starts with the image's
defaults, so developers set their environment up by hand in each one.

The dotfiles live on the host, outside the workspace. mise's default mode
symlinks each target to its source, so the source has to stay reachable in the
guest for as long as the sandbox lives. A copy made at create time would leave
symlinks that point nowhere after an upgrade gives the sandbox a fresh root
filesystem. The agent controls the guest and `.firebrick.yml`, so the host
directory must not become writable for it, and adding it must need the user's
approval, like any other host directory
([ADR 0037](0037-approve-mounts-and-network-changes-in-the-spec.md)).

The spec had a boolean `mise` that turned the mise step off. The new setting
belongs next to it.

## Decision

`mise` in `.firebrick.yml` becomes a map with `enabled` (default `true`) and
`config`, a host directory resolved like a mount's `host`. When the directory
has a `mise.toml`, `fbk` sends its canonical path as `mise_config`, approves it
as a read-only mount, and `fbkd` bind mounts it read-only at the fixed guest
path `/opt/firebrick/dotfiles` when it creates the sandbox. A fixed path outside
the home directory works for images with other users, and `fbkd` rejects a user
mount at that path.

On every start, and after an upgrade, `fbkd` runs as the image's default user:
`mise trust` on `/opt/firebrick/dotfiles/mise.toml` and on each workspace
config, `mise -C /opt/firebrick/dotfiles dot apply --yes --force`, and then
`mise install --yes` in the workspace. Applying before installing lets a global
mise config from the dotfiles bring its tools in the same start. `--force` lets
the dotfiles replace files the image ships, such as `~/.bashrc`. Trusting the
dotfiles config inside the sandbox is safe for the reason
[ADR 0017](0017-trust-the-workspaces-mise-config-inside-the-sandbox.md) gives:
its code runs on the dirty side.

Dotfiles are a convenience, so a missing `mise.toml` and a failing trust or
apply only warn: the sandbox starts and the tools are installed. A failure of
the workspace's own trust or install still fails the start.

`fbk` migrates a boolean `mise` in the working directory's spec to `mise:` with
`enabled: <bool>`, editing only that line, and the parser reports the boolean
form with a message naming `mise.enabled`.

## Consequences

- A sandbox comes up with the developer's dotfiles, and symlinked entries keep
  working for the life of the sandbox, also after `fbk upgrade`.
- The agent can read every file in the dotfiles directory, so it must hold no
  secrets. It can't change them.
- Changing `mise.config` only applies to new sandboxes, like other mounts.
- A spec with `mise: false` is rewritten the first time a new `fbk` reads it.
  Older `fbk` versions can't read the migrated file.
