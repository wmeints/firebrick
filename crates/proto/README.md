# firebrick-proto

Firebrick is a sandbox that runs coding agents inside microVMs, built on
[microsandbox](https://github.com/microsandbox/microsandbox), so an agent stays
away from your host. Each project gets its own lightweight VM with only the
project directory shared from the host.

`firebrick-proto` holds the gRPC client and server code that
[tonic](https://github.com/hyperium/tonic) generates from
[`proto/daemon.v1.proto`](https://github.com/wmeints/firebrick/blob/main/crates/proto/proto/daemon.v1.proto),
shared by `fbk` and `fbkd`.

This crate is a building block of Firebrick. It is published so that `firebrick`
can be installed from crates.io; it is not meant as a general-purpose library.

## Documentation

- [Firebrick documentation](https://firebrick-sandbox.dev/docs/)
- [Installation](https://firebrick-sandbox.dev/docs/installation/)
- [Quickstart](https://firebrick-sandbox.dev/docs/quickstart/)

## Source and license

- Repository: [wmeints/firebrick](https://github.com/wmeints/firebrick)
- License: [MIT](https://github.com/wmeints/firebrick/blob/main/LICENSE)
