# 57. Group the daemon modules behind narrow interfaces

## Status

Accepted

## Context

`crates/daemon/src` had grown to 21 modules next to each other. `sandboxes.rs`
held 3,400 lines and imported 14 of the others, and `server.rs`, `clipboard.rs`,
`netlog.rs` and `forward.rs` each mixed two or more concerns, such as a wire
protocol and the host tools behind it. Most modules had only one or two users,
but the crate exported 17 of them, so nothing showed which ones were an
interface and which were details of another module. A reader had to read across
files to find where a behavior lived.

## Decision

The daemon keeps eight top-level modules, and each one hides its submodules
behind a narrow interface:

- `server` - the gRPC service, with `attach` and `tunnel` for its streams, and
  `convert`, `transport` and `shutdown` for the message conversions, the
  endpoint's connections and the signals that stop the daemon.
- `sandboxes` - `SandboxManager`, with private submodules for its errors, the
  status of sandboxes, their settings, ports, secrets and recreation, and the
  `provision` submodules (`env`, `git`, `mise`, `network`, `pull`) that apply a
  spec to a sandbox.
- `guest` - the channels a sandbox uses to make `fbkd` act on the host: `open`,
  `netlog` and `clipboard`, with `vsock` and `rate_limit` as their private
  plumbing.
- `forward` - `Forwards`, with the shared listeners, `CallbackForwards` and
  `SshConnector` in submodules.
- `host_config` - one function, `sync`, that rewrites the SSH config, the
  editors' Remote-SSH settings and Zed's remote projects from the sandboxes.
- `ssh`, `secrets` and `runtime` stay as they were, except that writing the SSH
  config moved from `ssh` to `host_config`.

A submodule is private unless `main.rs` or the integration tests in
`crates/daemon/tests` use it. Siblings share items with `pub(super)`, and the
tests of a submodule live next to it.

## Consequences

- The crate root lists the parts of the daemon, and each part's `mod.rs` lists
  what it offers. A change to a submodule can't reach code outside its parent
  without the compiler noticing.
- The integration tests import `guest::clipboard`, `guest::open`,
  `guest::netlog` and `sandboxes::provision` instead of top-level modules.
- `pub(super)` items are visible to the whole parent module, which is wider than
  strictly needed between two siblings.
