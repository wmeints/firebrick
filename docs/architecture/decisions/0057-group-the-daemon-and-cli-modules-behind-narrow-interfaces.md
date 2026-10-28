# 57. Group the daemon and CLI modules behind narrow interfaces

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

`crates/cli/src` had the same problem with 17 modules. `manage.rs` held 2,400
lines with the start, upgrade and list commands, the host paths of mounts, and
the helpers that `network`, `port`, `secret` and `session` reached into, such as
the spec file name and resolving the working directory's sandbox. Handling the
spec file was spread across `manage`, `validate` and `init`.

## Decision

Each crate keeps a few top-level modules, and each one hides its submodules
behind a narrow interface.

The daemon has eight:

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

The CLI has ten:

- `sandbox` - was `manage`: `start`, `stop`, `rm`, `upgrade` and `ls`, with
  private submodules for each command, the host paths of mounts and the sandbox
  of the working directory, and `git` for the host's git identity.
- `spec_file` - finds, migrates and loads `.firebrick.yml`, reports its problems
  and names the sandbox of a workspace without one, with `init` and `validate`
  as submodules.
- `network` - `update` and `log`, and the `presets`.
- `approval` - `Approver`, with the approvals file (`store`), the difference
  shown to the user (`diff`) and the question (`prompt`) in submodules.
- `remote` - `session` for `fbk run` and `ssh` for `fbk ssh-proxy`.
- `ui` - `table` and `progress`.
- `client`, `port` and `secret` stay as they were. The unit test fixtures live
  in `test_support`.

A submodule is private unless the binary or the integration tests use it.
Siblings share items with `pub(super)`. The tests of a private function live
next to it, and the tests of a module's interface stay in its `mod.rs`.

## Consequences

- The crate roots list the parts of each binary, and each part's `mod.rs` lists
  what it offers. A change to a submodule can't reach code outside its parent
  without the compiler noticing.
- The daemon's integration tests import `guest::clipboard`, `guest::open`,
  `guest::netlog` and `sandboxes::provision`, and the CLI's import
  `sandbox::git`, instead of top-level modules.
- `pub(super)` items are visible to the whole parent module, which is wider than
  strictly needed between two siblings.
