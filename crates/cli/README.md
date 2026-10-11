# firebrick-cli

Firebrick is a sandbox that runs coding agents inside microVMs, built on
[microsandbox](https://github.com/microsandbox/microsandbox), so an agent stays
away from your host. Each project gets its own lightweight VM with only the
project directory shared from the host.

`firebrick-cli` provides the `fbk` binary. You run `fbk` to start, run, SSH into
and manage sandboxes; it talks to the `fbkd` daemon over gRPC. `fbk` starts the
daemon from its own directory, so both binaries must be installed side by side.

## Installation

Install both binaries, because the CLI needs the daemon next to it:

```sh
cargo install firebrick-cli firebrick-daemon
```

## Documentation

- [Firebrick documentation](https://firebrick-sandbox.dev/docs/)
- [Installation](https://firebrick-sandbox.dev/docs/installation/)
- [Quickstart](https://firebrick-sandbox.dev/docs/quickstart/)

## Source and license

- Repository: [wmeints/firebrick](https://github.com/wmeints/firebrick)
- License: [MIT](https://github.com/wmeints/firebrick/blob/main/LICENSE)
