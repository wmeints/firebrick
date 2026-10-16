# firebrick-cli

Firebrick is a sandbox that runs coding agents inside microVMs, built on
[microsandbox](https://github.com/microsandbox/microsandbox), so an agent stays
away from your host. Each project gets its own lightweight VM with only the
project directory shared from the host.

`firebrick-cli` is the library behind the `fbk` binary, which the `firebrick`
crate installs. You run `fbk` to start, run, SSH into and manage sandboxes; it
talks to the `fbkd` daemon over gRPC. `fbk` starts the daemon from its own
directory, so both binaries must be installed side by side.

## Installation

Install the `firebrick` crate, which puts both binaries side by side:

```sh
cargo install firebrick
```

## Documentation

- [Firebrick documentation](https://firebrick-sandbox.dev/docs/)
- [Installation](https://firebrick-sandbox.dev/docs/installation/)
- [Quickstart](https://firebrick-sandbox.dev/docs/quickstart/)

## Source and license

- Repository: [wmeints/firebrick](https://github.com/wmeints/firebrick)
- License: [MIT](https://github.com/wmeints/firebrick/blob/main/LICENSE)
