# Changelog

The notable changes of each firebrick release, newest first. The
`create-release` skill adds a section for each release in its version bump pull
request, and the release workflow copies the section into the GitHub release,
above the generated list of pull requests. Each section starts with a `##
vX.Y.Z - YYYY-MM-DD` heading.

## v0.10.0 - 2026-10-23

This release makes sandboxes feel more like a regular machine: commits get the
host's author identity, SSH sessions run in the user's login shell, and `fbk`
replaces a daemon left over from an earlier install.

### Features

- `fbk start` and `fbk run` copy the host's git `user.name` and `user.email` for
  the workspace into the sandbox, so an agent can commit without configuring git
  first. A changed identity reaches the sandbox on the next start. (#225)
- `fbk` checks the daemon's version on every command and restarts a `fbkd` of
  another version when it's idle. When sessions are still open, it asks you to
  close them and try again. A daemon of 0.9.0 or earlier can't be restarted this
  way: stop it by hand once after upgrading. (#228)
- `fbkd --version` prints the version of the daemon. (#227)

### Fixes

- SSH sessions now run in the user's login shell with `SHELL` set, as `sshd`
  does. The Codex desktop app no longer rejects firebrick hosts, and `ssh
  <leaf>.fbk` opens `bash` instead of `sh` in `firebrick-base`. Sandboxes
  created by 0.9 or earlier keep `/bin/sh` until you recreate them with `fbk rm
  --force` and `fbk start`. (#223)

## v0.9.0 - 2026-10-21

This release sets plain environment variables in the sandbox from
`.firebrick.yml`, for configuration that doesn't need to be kept secret.

### Features

- An `env` map in `.firebrick.yml` sets environment variables, such as
  `NODE_ENV` or `RUST_LOG`, verbatim in every process in the sandbox, including
  `fbk run` and SSH shells. Unlike `fbk secret`, the agent sees the real value
  instead of a placeholder. Changes apply the next time a stopped sandbox
  starts; for a running sandbox, `fbk` tells you to run `fbk stop && fbk start`.
  A variable can't share its name with a secret the sandbox gets. (#219)

## v0.8.0 - 2026-10-15

This release runs `fbk` and `fbkd` natively on Windows, shares the host
clipboard with a sandbox when you opt in, and adds `fbk upgrade` and network
presets to keep sandboxes current and enforced networks easy to set up. It also
closes several paths where a workspace or guest output could reach the host.

> **Windows support is experimental for now.** It builds on microsandbox's
> preview support for the Windows Hypervisor Platform, and the Windows
> integration tests don't run in CI, so expect rough edges. You can still run
> the Linux release in WSL 2.

### Breaking changes

- `fbk init` no longer pins `image` in the new `.firebrick.yml`, so new
  sandboxes follow the release's `firebrick-base` image. Existing specs keep
  their `image`; remove it or point it at the new version to pick up updates.
  (#174)

### Features

- `fbk` and `fbkd` run natively on Windows 11 x64 through the Windows Hypervisor
  Platform (experimental, see above). The daemon keeps its data in
  `%LOCALAPPDATA%\firebrick`, and the generated SSH config works with Windows'
  OpenSSH. (#211)
- The release has an `x86_64-pc-windows-msvc` zip archive with `fbk.exe` and
  `fbkd.exe`, so Windows users no longer build from source. The binaries run
  without the Visual C++ Redistributable. (#215)
- `clipboard: true` in `.firebrick.yml` shares the host clipboard with the
  sandbox, both ways: programs can paste text and PNG images with `wl-paste` or
  `xclip`, and copy with `wl-copy` or `xclip -i`. It's off by default and needs
  your approval like `mounts` and `network`. (#193, #195)
- `fbk upgrade [NAME]` recreates a sandbox from the latest version of its image
  and keeps its name, workspace, mounts, ports, network rules and secrets.
  Docker data in the sandbox isn't carried over. (#172)
- `fbk start`, `fbk run`, `fbk network` and `fbk port` warn when a sandbox runs
  an older `firebrick-base` image, and point to `fbk upgrade`. (#174)
- `fbk network presets show` and `fbk network presets apply` add grouped allow
  rules for coding agents and toolchains, such as `claude`, `copilot`, `github`,
  `node`, `python` and `rust`, to an enforced network. (#173)
- `fbk start` shows the progress of `mise install` instead of staying silent
  while the tools install. (#171)

### Fixes

- Ctrl+C in a sandbox terminal now stops forked processes, such as the JVM of
  `mvn quarkus:dev`, and `exit` ends the session. (#167)
- `fbk ls` no longer shows a paused sandbox as running on a busy host, and `fbk
  network` and `fbk upgrade` no longer recreate a paused sandbox. A sandbox
  whose runtime doesn't answer is listed as `Unknown`. (#209)
- Escape sequences from guest output and from `.firebrick.yml` diagnostics are
  removed whole, so they can't change the host terminal or clipboard, and `fbkd`
  rejects invalid image references in the spec. (#182, #208)
- `fbkd` rejects sandbox names that aren't a single path component before it
  uses them in host paths. (#207)
- `fbk` refuses a `.firebrick.yml` that is a symlink or not a regular file
  instead of reading or writing through it. (#205)
- `fbkd` writes `~/.ssh/config` atomically, so an interrupted write can't leave
  it truncated. (#204)
- On macOS, `fbkd` keeps its network log sockets in a private directory. (#183)
- The bundled `rustls` is updated to fix RUSTSEC-2026-0346 to 0349. (#210)

## v0.7.0 - 2026-10-11

This release shows which domains and IP addresses an agent contacts, so you can
see which `fbk network allow` rules a task needs and spot hosts it shouldn't
reach.

### Features

- `fbkd` records the domains and IP addresses each sandbox contacts, including
  attempts the network rules deny. A logger in the `firebrick-base` image
  reports each DNS lookup and outgoing TCP and UDP destination. Sandboxes
  created by an older version have no record until you remove and start them
  again; custom images need to run the logger. (#156)
- `fbk network log` shows the record of the project's sandbox, newest first,
  with whether its current rules allow each destination. (#157)

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
