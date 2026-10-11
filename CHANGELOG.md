# Changelog

The notable changes of each firebrick release, newest first. The
`create-release` skill adds a section for each release in its version bump pull
request, and the release workflow copies the section into the GitHub release,
above the generated list of pull requests. Each section starts with a `##
vX.Y.Z - YYYY-MM-DD` heading.

## v0.6.0 - 2026-10-11

This release makes .NET tools such as the Aspire CLI work in a sandbox out of
the box, and the docs now explain how to run firebrick on Windows through WSL 2.

### Features

- The `firebrick-base` image includes the ICU library (`libicu78`), so the
  Aspire CLI and other .NET apps run in a sandbox without installing it first.
  (#151)
- The documentation explains how to run firebrick on Windows through WSL 2,
  including where `fbkd` writes the SSH config. (#145)

## v0.5.0 - 2026-10-11

This release hardens firebrick against an agent that edits its own workspace:
`fbk` asks before it applies mounts or network changes from `.firebrick.yml`,
and several paths that trusted agent-controlled input are closed. Browser logins
from inside a sandbox now complete, git can push to GitHub with a `GH_TOKEN`
secret, and the crates are published to crates.io.

### Breaking changes

- `fbk start`, `fbk run` and `fbk network` ask before they apply `mounts` or
  `network` settings from `.firebrick.yml` that you haven't approved, and show
  the changes. Approvals are stored per workspace in
  `~/.config/firebrick/approved.yml`. Without a terminal the command fails; pass
  `--yes` (`fbk run --yes <command>`) in scripts to approve without asking.
  (#140)
- `fbkd` no longer falls back to `/tmp/firebrick` for its secrets, SSH keys and
  logs. When `HOME` and the XDG variables aren't absolute paths it refuses to
  start, and `fbk` reports why. (#139)
- `fbk secret set --allow-host` rejects single-label wildcards such as `*.com`,
  following the same rules as the network rules in `.firebrick.yml`. (#141)

### Features

- OAuth browser logins in a sandbox, such as `gh auth login --web` or Claude
  Code, complete: `fbkd` forwards the callback port into the sandbox before it
  opens the URL in the host browser. (#118)
- The `firebrick-base` image configures a git credential helper, so `git clone`,
  `pull` and `push` over HTTPS to github.com work once `fbk secret set GH_TOKEN`
  is set. (#123)
- Install firebrick from crates.io with `cargo install firebrick-cli
  firebrick-daemon`. (#120)
- The user documentation moved to https://firebrick-sandbox.dev. (#122, #123)

### Fixes

- `fbk` only talks to a daemon socket served by your own user or root, so
  another local user can't intercept requests such as `fbk secret set`. (#139)
- `fbk stop`, `fbk rm`, `fbk network`, `fbk port` and `fbk secret --scope
  sandbox` without a name refuse to act on a sandbox that belongs to another
  workspace, like `fbk start` already did. (#138)
- Terminal control characters in guest output, for example from mise when a
  sandbox starts, are stripped before they reach your terminal or the daemon
  log. (#137)

## v0.4.0 - 2026-10-10

This release gives you control over what a sandbox can reach and see: network
allow/deny rules, extra mounts, port forwards and per-sandbox secrets, all
configurable from `.firebrick.yml` or the new `fbk` subcommands. Sandboxes also
come up ready to work, with Docker and the project's mise tools installed.

### Breaking changes

- `fbkd` keeps its microsandbox runtime, images, database and sandboxes in
  `$XDG_STATE_HOME/firebrick/msb` (`~/.local/state/firebrick/msb`) instead of
  `~/.microsandbox`, so a separately installed `msb` can no longer break it.
  Sandboxes created by earlier versions stay in `~/.microsandbox` and no longer
  show up in `fbk ls`; recreate them after upgrading. Images are pulled again on
  first start. A non-empty `MSB_HOME` still takes precedence. (#101)

### Features

- `fbk init` writes a default `.firebrick.yml` for the current directory. (#89)
- Restrict a sandbox's outgoing traffic with a `network` section in
  `.firebrick.yml`: with `enforce: true`, only hosts, domains, IPs and CIDR
  ranges in `allow` are reachable, and `deny` rules win. Blocked HTTP and HTTPS
  requests get a page that names the host. (#92)
- `fbk network allow`, `fbk network deny` and `fbk network policy
  enable|disable` change the rules of an existing sandbox and update
  `.firebrick.yml`. (#97)
- `fbk network disable` removes a sandbox's network device entirely; `fbk
  network enable` restores it. (#103)
- Mount extra host directories into the sandbox with a `mounts` section in
  `.firebrick.yml`, optionally read-only. Host paths may start with `~` or
  `~user`. (#95)
- Forward host ports to servers in the sandbox with a `ports` section in
  `.firebrick.yml`, or live with `fbk port forward <host>:<guest>` and `fbk port
  rm`. (#96, #99)
- Secrets can be scoped to one sandbox with `fbk secret set --scope sandbox`; a
  sandbox-scoped secret overrides the global one with the same name in that
  sandbox only. (#100)
- `xdg-open` and `$BROWSER` inside the sandbox open http and https URLs in the
  host browser, for example for device login flows. Local addresses are refused,
  and enforced network rules apply. (#104)
- `fbk` shows a progress bar while it pulls the sandbox image. (#102)
- The `firebrick-base` image includes Docker, and every sandbox gets a dedicated
  disk for `/var/lib/docker`, sized with `volumes.docker` in `.firebrick.yml`.
  (#83, #84)
- `fbkd` trusts the workspace's mise config and runs `mise install` when a
  sandbox starts, so the project's tools are ready. Set `mise: false` in
  `.firebrick.yml` to turn it off. (#85)

### Fixes

- `fbk start` and `fbk run` refuse to reuse a sandbox that mounts a different
  working directory, instead of silently giving the agent access to another
  project's files when two directories hash to the same name. (#87)
