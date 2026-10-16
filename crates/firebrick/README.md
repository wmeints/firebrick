# firebrick

Firebrick is a sandbox that runs coding agents inside microVMs, built on
[microsandbox](https://github.com/microsandbox/microsandbox), so an agent stays
away from your host. Each project gets its own lightweight VM with only the
project directory shared from the host.

The `firebrick` crate installs both Firebrick binaries: `fbk`, the CLI you run
to start, run, SSH into and manage sandboxes, and `fbkd`, the daemon that
manages the microsandbox VMs. `fbk` starts the daemon from its own directory, so
both binaries must be installed side by side.

## Installation

Building needs `protoc` 3.15 or newer, the Protocol Buffers compiler, on your
`PATH`:

```sh
cargo install firebrick
fbk --version
```

Cargo installs `fbk` and `fbkd` in `~/.cargo/bin`.

## Documentation

- [Firebrick documentation](https://firebrick-sandbox.dev/docs/)
- [Installation](https://firebrick-sandbox.dev/docs/installation/)
- [Quickstart](https://firebrick-sandbox.dev/docs/quickstart/)

## Source and license

- Repository: [wmeints/firebrick](https://github.com/wmeints/firebrick)
- License: [MIT](https://github.com/wmeints/firebrick/blob/main/LICENSE)
