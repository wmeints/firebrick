# 47. Run development tasks with mise

## Status

Accepted

## Context

`.cargo/config.toml` defined cargo aliases for the common commands: `lint`,
`unit-tests`, `integration-tests`, `install-cli` and `install-daemon`. An alias
expands to one cargo command, so it can't chain steps, call other tools or
branch. Running a development build took more than that: install both binaries,
start a local image registry when it isn't running, then build and push the
`firebrick-base` image to it, as a list of commands in the README.

The alternatives were the `cargo xtask` pattern, a task runner such as `just` or
`cargo-make`, and mise tasks.

## Decision

- The development tasks are mise tasks in `mise.toml`: `lint`, `unit-tests` and
  `integration-tests` run the commands the aliases ran, `registry` starts the
  `registry:2` container on `127.0.0.1:5000` when it isn't running, and
  `install-dev` depends on `registry`, installs `fbk` and `fbkd` and pushes the
  image as `localhost:5000/firebrick-base:dev`.
- mise already installs the toolchain (`mise install`), so its tasks need no
  extra tool, unlike `just` or `cargo-make`. The tasks are chains of commands
  with little logic, which a shell script does in a few lines; an `xtask` crate
  would be more code for the same steps.
- The cargo aliases are removed, so each command has one name. The `[env]` block
  in `.cargo/config.toml` stays, and applies to the tasks because they run
  cargo.
- The install task is called `install-dev`, not `install`, because `mise
  install` is mise's own command to install the toolchain.

## Consequences

- `mise tasks` lists the tasks with their descriptions.
- Lefthook, the Claude Code hooks and CI keep calling cargo directly, so they
  don't depend on the tasks.
- mise renders task scripts as Tera templates, so a script can't contain `{{`,
  such as a docker `--format` template.
- `install-dev` and `registry` need Docker, which mise doesn't install. The user
  docs build from source with plain `cargo install`, which doesn't.
