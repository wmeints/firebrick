# 33. Configure a system git credential helper for GitHub tokens

## Status

Accepted

## Context

Agents push and pull GitHub repositories over HTTPS, because SSH keys would have
to live in the sandbox where the agent can read them. `fbk secret set GH_TOKEN`
gives the sandbox a placeholder such as `$MSB_GH_TOKEN`, and microsandbox
replaces it in the Basic `Authorization` header of requests to the secret's
allowed hosts (see [ADR 0004](0004-store-secrets-in-a-private-file.md)). git
only sends the placeholder when a credential helper hands it over, so users had
to run a long `git config --global credential.https://github.com.helper ...`
command in every sandbox, or install `gh` and run `gh auth setup-git`.

The helper can live in the image's system config (`/etc/gitconfig`), in the
agent's global config (`/home/agent/.gitconfig`), or be written by `fbkd` when
it starts a sandbox.

## Decision

- The `firebrick-base` image installs `/usr/local/bin/firebrick-git-credential`,
  a POSIX `sh` script, and registers it with `git config --system` as
  `credential.https://github.com.helper`.
- On `get` it prints `username=x-access-token` and the value of `GH_TOKEN`, or
  of `GITHUB_TOKEN` when `GH_TOKEN` is unset or empty, as the password. Without
  either it prints nothing and exits 0. On `store` and `erase` it does nothing.
- It's in the system config rather than in `/home/agent/.gitconfig`, so the
  user's own git config isn't overwritten or merged with ours, and helpers there
  still run after it. `fbkd` doesn't write it either: the image is the one place
  that knows the guest has git, and custom images stay untouched.
- It reads the variables when git calls it, so it doesn't change when the
  secrets change; a sandbox picks up a new secret after a restart, as before.

## Consequences

- `git clone`, `pull` and `push` to github.com work in sandboxes that run
  `firebrick-base` once the user has set `GH_TOKEN` or `GITHUB_TOKEN`, without
  configuring git.
- The agent's `~/.gitconfig` stays the user's: helpers configured there, for
  example by `gh auth setup-git`, keep working. git asks the system helper
  first, so they answer only when neither variable is set.
- The helper doesn't know a secret's allowed hosts, which the guest can't see.
  When `GH_TOKEN` or `GITHUB_TOKEN` is scoped to other hosts, such as a GitHub
  Enterprise server, it still hands git the placeholder for github.com, and the
  user's own helper never answers. The README shows how to reset the helper list
  in the sandbox.
- The helper only covers `https://github.com`. Other forges and GitHub
  Enterprise hosts still need a helper of their own.
- Images that aren't based on `firebrick-base` don't get the helper; the docs
  show the equivalent `git config --global` command for them.
- Nothing changes in `fbkd`, the CLI or the secret defaults, which already allow
  `github.com` for `GH_TOKEN` and `GITHUB_TOKEN`.
