---
name: sandbox-reference
description: "Look up how the microsandbox crate behaves before relying on it: its API for creating, starting, modifying and exec'ing into sandboxes, secrets, network policy, SSH, runtime setup and on-disk state. Use when changing code in crates/daemon that calls microsandbox, when designing a feature that depends on what microsandbox can or can't do, or when debugging a microsandbox error."
---

# Sandbox reference

`fbkd` drives sandboxes through the `microsandbox` crate. Its docs site doesn't
cover everything the code relies on, so answer questions from the crate source
of the exact version in `Cargo.lock`, and start from the facts below instead of
rediscovering them.

## Find the source

Read the version from the lockfile, never from `crates/daemon/Cargo.toml` (it
holds the minimum version) or from a version you remember:

```bash
V=$(grep -A1 '^name = "microsandbox"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p')
R=$(ls -d ~/.cargo/registry/src/index.crates.io-*/ | head -1)
M=${R}microsandbox-$V/lib            # the SDK
N=${R}microsandbox-network-$V/lib    # network policy, secrets proxy, TLS
```

Run `cargo fetch` if the directories don't exist. Search with `grep -rn` and
read with the Read tool. To answer a broad question, give an `Explore` agent the
paths above.

| Topic                           | Where                                                           |
| ------------------------------- | --------------------------------------------------------------- |
| `Sandbox` (get, list, remove)   | `$M/sandbox/mod.rs`                                             |
| Create-time settings            | `$M/sandbox/builder.rs` (`SandboxBuilder`)                      |
| Start, stop, status of a handle | `$M/sandbox/handle.rs` (`SandboxHandle`)                        |
| Change an existing sandbox      | `$M/sandbox/modify.rs`                                          |
| Exec and PTY sessions           | `$M/sandbox/exec.rs`, `exec_stream_with` in `$M/sandbox/mod.rs` |
| SSH server                      | `$M/sandbox/ssh.rs`                                             |
| Runtime install and version     | `$M/setup/`                                                     |
| Errors                          | `$M/error.rs` (`MicrosandboxError`)                             |
| Home dir, db and config paths   | `${R}microsandbox-utils-$V/lib/lib.rs`, `$M/config/`            |
| Network rules                   | `$N/model/policy/` (`builder.rs`, `types.rs`)                   |
| Secret substitution in requests | `$N/engine/secrets/handler.rs`                                  |
| Policy evaluation, DNS queries  | `$N/model/policy/types.rs`, `$N/engine/dns/forwarder.rs`        |
| Deny responses                  | `$N/engine/tcp/proxy.rs`, `$N/engine/netstack/poll.rs`          |

## How firebrick uses it

All calls live in `crates/daemon/src`. Reuse these before adding new ones:

- `sandboxes.rs` - `Sandbox::builder(..).image().cpus().memory().label()
  .volume()` and `.init(..)` to create, `Sandbox::get`, `Sandbox::list_with`
  (paged, filtered by label), `connect_or_start_detached`, `stop`, `remove`,
  `status_snapshot`, and `modify().label(..)`.
- `network.rs` - `NetworkPolicy::builder()` from the `microsandbox-network`
  crate (`microsandbox` doesn't re-export the rule builder or `Destination`),
  and `builder.network(|n| n.policy(..).tls(|t| t).http(..))` at create time.
- `secrets.rs` - `builder.secret(..)` at create time, and `modify().secret(..)`
  / `modify().remove_secret(..)` on existing sandboxes.
- `session.rs` - `exec_stream_with(command, |e| e.args(..).stdin_pipe()
  .tty(true))` for `fbk run`.
- `open.rs` - a long-running `exec_stream_with("sh", ..)` per sandbox as a
  guest-to-host channel: the relay echoes lines from a FIFO and `fbkd` reads its
  stdout, because the network policy and vsock routes are fixed at create time
  (ADR 0026).
- `tunnel.rs` / `ssh.rs` - `microsandbox::sandbox::ssh::SshServer` behind `fbk
  ssh-proxy`.
- `runtime.rs` - `setup::ensure_runtime`, `setup::install_runtime` and
  `setup::resolve_runtime_version` with the runtime embedded through the
  `embed-binaries` feature.

## Known behavior

Verified against microsandbox 0.7.8. Recheck an item in the source before you
build on it when `Cargo.lock` has a newer version.

**Lifecycle**

- `start_detached()` on a sandbox that is already running fails with
  `MicrosandboxError::SandboxStillRunning`. `connect_or_start_detached()`
  connects to a running sandbox, waits for a starting one, starts a stopped one,
  absorbs that race, and rejects draining and paused sandboxes.
- `SandboxHandle::pause()` suspends a running VM without a snapshot and its
  status becomes `Paused`; `resume()` continues the same processes
  (`$M/sandbox/pause.rs`). firebrick never pauses sandboxes itself, but `msb`
  can, so code that stops or recreates a sandbox has to expect `Paused`.
- `Sandbox::list_with` rejects an empty cursor. Only call `.cursor(..)` with the
  cursor of the previous page.
- `Sandbox::list_with` reads a page of rows and then reloads each sandbox
  (`$M/backend/local/sandbox/mod.rs`, `list_sandbox_handle_state`). When another
  process removes a sandbox in between, the whole listing fails with
  `SandboxNotFound`. `sandboxes::list_all` starts the listing over in that case.
- `.init(path)` hands PID 1 to an init binary in the image after agentd's setup.
  `.init("auto")` probes common init paths and refuses to boot an image that has
  none, so firebrick passes an explicit path (ADR 0007).

**Image pull progress**

- `SandboxBuilder::create_detached_with_pull_progress()` returns a
  `PullProgressHandle` and a `JoinHandle` of the create. Events are sent with
  `try_send` into a channel of 1024, so they can be dropped. A pull from cached
  image metadata sends only `Resolving`, `Resolved` and `Complete`; a layer
  whose tarball is cached sends `LayerDownloadComplete` with its full size and
  no `LayerDownloadProgress`. `Resolved.total_download_bytes` is `None` when the
  manifest has no layer sizes. `fbkd` turns them into progress in `pull.rs` (ADR
  0024).
- A failed pull returns `MicrosandboxError::Image(ImageError::Registry(..))`
  with an `oci_client::errors::OciDistributionError`. Docker Hub and GHCR answer
  an unknown repository with `UnauthorizedError`, an unknown tag with
  `RegistryError` whose envelope has `ManifestUnknown`, and an unreachable host
  gives `RequestError`. Neither type is re-exported by `microsandbox`, so the
  daemon depends on `microsandbox-image` and `oci-client` directly (ADR 0025). A
  failed pull leaves no sandbox behind.

**Volumes**

- `.volume(path, |m| m.owned_with(|v| v.disk().size(mib)))` attaches a
  sandbox-owned ext4 disk (`/dev/vdX`, virtio-blk) at `path`. Microsandbox
  formats it at create time as
  `$MSB_HOME/sandboxes/<name>/owned-volumes/<id>/disk.raw`, keeps it across
  stops and restarts and deletes it on `remove()`. The size must be positive.
  The sandbox's `config().spec.mounts` lists it as `VolumeMount::Owned` with
  `OwnedVolumeStorage::Disk { capacity_mib }`. `fbkd` puts Docker's data on such
  a disk because the root filesystem is overlayfs (ADR 0015).

**Guest filesystem**

- `/run` isn't a tmpfs: it sits on the overlayfs root, whose upper layer
  persists across `stop` and `start`. PID files, sockets and other runtime state
  from the previous boot are still there after a restart, and a stale PID can
  match an unrelated process in the new boot. An init that starts daemons must
  remove their runtime state first; `firebrick-base` removes `/run/docker.pid`,
  `/run/docker` and `/run/containerd` before it starts `dockerd` (ADR 0016).

**Changing existing sandboxes**

- `modify()` changes cpus, memory, disk size, env, labels, workdir and secrets.
  Use `.next_start()` or `.restart()` to choose when changes apply.
- The network policy has no `modify()` setting: it's fixed when the sandbox is
  created. Changing rules means recreating the sandbox, which `fbkd` does from a
  disk snapshot (ADR 0021).
- A label change on a running sandbox is "restart-required": `apply()` without a
  policy fails with `cannot apply modification: label requires restart`. With
  `.next_start()` it's stored right away and visible through `Sandbox::get(..)
  .config()`, without restarting the VM; `fbkd` stores `firebrick.ports` this
  way.
- Published ports (`SandboxBuilder::port`) are fixed at create time and target
  the guest's interface IP, not its loopback, so `fbkd` forwards ports through
  SSH `direct-tcpip` instead (ADR 0019).

**Snapshots**

- `Snapshot::builder(name).from_sandbox(sb).create()` takes a disk snapshot: the
  root disk's writable layer and the sandbox-owned volumes, such as the Docker
  disk. Stop the sandbox first. `.full()` also captures memory and devices;
  firebrick doesn't use it.
- The snapshot is filed in a group named after the source sandbox. A bare name
  selects a group head, not the snapshot, so `Snapshot::open`,
  `Snapshot::remove` and `override_snapshot` with the snapshot's own name fail
  with `SnapshotNotFound`. Use the path from `Snapshot::path()`.
- `SandboxBuilder::override_snapshot(snapshot)` (public, `#[doc(hidden)]`) takes
  the root filesystem from the snapshot and keeps every other builder setting:
  labels, init, workdir, mounts, resources, secrets and network. The snapshot's
  owned volumes replace builder mounts at the same guest path, so a
  `.volume(..owned..)` at the same path gets the captured disk back. Calling
  `.image(..)` afterwards drops the pending snapshot. The new sandbox copies the
  snapshot's payloads, so the snapshot can be removed right after it boots.
- `RestoreBuilder` also restores a snapshot, but has no setters for init,
  labels, env, workdir or secrets.
- `Sandbox::remove()` doesn't remove the sandbox's snapshots.
- `Snapshot::remove` takes an exclusive `flock` on the snapshot's entry in
  `<group>/.msb-leases/` and fails right away with
  `MicrosandboxError::Custom("snapshot is in use by an active operation: ..")`
  while any reader holds it, such as a listing or a create from the snapshot in
  another task or process (`$M/backend/local/snapshot/store.rs`). It never
  waits, so `remove_snapshot` in `sandboxes.rs` tries again.

**Secrets**

- The guest only sees a placeholder such as `$MSB_GH_TOKEN`. The host proxy
  swaps in the real value on requests to the secret's allowed hosts, including
  inside decoded `Authorization: Basic` credentials, so git over HTTPS works.
- `SecretSource::Store` (a host keyring) isn't implemented and returns an error.
  Use `.value(..)` or `SecretSource::Env` (ADR 0004).

**Network**

- Rules are an ordered list and the first match decides. Destinations are
  `.ip(..)` (stored as a /32 or /128 `Cidr`), `.cidr(..)`, `.domain(..)` and
  `.domain_suffix(..)`, which also matches the domain itself and its subdomains.
  There is no `*.example.com` wildcard syntax, and a suffix with a single label
  such as `com` fails to build.
- `NetworkPolicy::builder().default_deny()` also denies ingress; use
  `.default_egress(Action::Deny)` to deny only egress.
- Domain rules also match DNS queries to the sandbox's resolver: a domain deny
  rule makes the name return NXDOMAIN. `Rule::allow_dns()` allows UDP/TCP 53 to
  the gateway; without it, a default-deny policy resolves nothing.
- `SandboxBuilder::disable_network()` removes the guest's network device
  (`spec.network.enabled = false`) and sets `NetworkPolicy::none()`. Nothing in
  the guest resolves or connects. `builder.secret(..)` afterwards only turns TLS
  interception on and doesn't bring the device back, so creating such a sandbox
  with secrets works. Exec sessions, the SSH server and its `direct-tcpip`
  forwards still work, because they go through agentd, not the guest network.
  `fbkd` uses it for `network.enabled: false`.
- `builder.network(..)` starts from the network config set so far, so it can be
  combined with `builder.secret(..)`. `.tls(|t| t)` replaces the TLS settings
  with interception on port 443 enabled. Secrets turn interception on too.
- The guest trusts the interception CA in its system store
  (`/etc/ssl/certs/ca-certificates.crt`), so `curl` in a Debian image accepts
  intercepted HTTPS.
- `.http(|h| h.deny_response(true).deny_message(..))` answers a denied HTTP/1.x
  request, also over intercepted HTTPS, with `403 Forbidden` and the message
  (`{host}` is the denied host). It only does so when the walk ends at the
  default deny. A domain deny rule defers when the connection opens, and when no
  later rule can allow the flow microsandbox treats it as an explicit deny: the
  connection is reset without the page, for every denied host, as soon as the
  policy has any domain deny rule. IP and CIDR denies and non-HTTP traffic are
  reset or closed.
- Strict mode (`NetworkConfig::strict`) is on by default: a host allowed only by
  a domain rule is only reachable over HTTP(S) whose host name can be checked,
  so SSH to `github.com:22` needs an IP or CIDR rule. With interception on, QUIC
  (UDP/443) is dropped.
- microsandbox has no event API for denied connections; it only logs them at
  `debug` level.
- On hosts without IPv6 egress, the guest still gets IPv6 and its IPv6
  connections are reset. `firebrick-base` disables guest IPv6 (ADR 0007).

**SSH**

- The SSH server needs no `sshd` in the guest and supports exec, PTY, SFTP and
  `direct-tcpip` port forwarding. It doesn't support agent forwarding (see
  `docs/architecture/11-risks-and-technical-debt.md`).
- `direct-tcpip` connects through agentd from inside the guest, so
  `127.0.0.1:<port>` is the guest's loopback; the login user doesn't matter. A
  rejected channel (nothing listening) is logged by microsandbox at `debug`.
- `server_with(|o| o.host_key(key).authorized_key(base64))` takes in-memory
  keys, so an in-process client needs no key files. `sb.ssh().connect()` returns
  an `SshClient` without access to its `russh` handle, so it can't open
  `direct-tcpip` channels; `forward.rs` connects its own `russh` client.

**Terminals**

- A TTY exec without a `TERM` env gets the host process's `TERM` (or `xterm`
  when it's unset or `dumb`), so `fbkd` passes on the terminal it was started
  from. `session.rs` sets `TERM` explicitly.
- The SSH server sets `TERM` in the guest from the client's PTY request
  (`ssh.rs`), so the host side can't choose it for SSH sessions.
- An exec stream ends at `ExecExited`: the reader in `$M/sandbox/exec.rs` stops
  there and drops anything after it. On a TTY exec the guest agent can report
  the exit before it has read the last terminal output, so output written as the
  process exits, such as curl's `-w` status code, is sometimes lost. The
  `vm-tests` wait in the shell after such a command (`fetch` in
  `tests/grpc.rs`).

**Host state**

- State lives in `$MSB_HOME`: `bin/msb`, `db/msb.db`, `config.json`, `tls/` (the
  interception CA) and `sandboxes/<name>/logs/`. microsandbox falls back to
  `~/.microsandbox`, but `fbkd` sets `MSB_HOME` to firebrick's own home,
  `$XDG_STATE_HOME/firebrick/msb` (`~/.local/state/firebrick/msb`), when it's
  unset or empty (ADR 0023). Sandboxes from firebrick 0.3.0 and earlier stay in
  `~/.microsandbox`.
- `LocalBackend::builder().home(..)` with `set_default_backend` only moves the
  home of the SDK process. The `msb` VM processes it spawns only inherit the
  environment and resolve paths such as the TLS interception CA (`tls/`) from
  `MSB_HOME`, so changing the home means setting `MSB_HOME`.
- Unix sockets live under `MSB_HOME`, and their paths can't be longer than 108
  bytes, so an isolated `MSB_HOME` needs a short path under `/tmp`. The session
  scratchpad is too long.
- A newer `msb` migrates `db/msb.db` in place, and an older crate then fails
  every call with "database schema is newer than this msb binary". In firebrick
  that surfaces as `Internal: failed to list sandboxes`. `.cargo/config.toml`
  sets `MSB_HOME=/tmp/firebrick-msb` for cargo commands so the tests don't use
  the user's database; never move or change `~/.microsandbox` to make them pass.
- To run firebrick or `msb` against real VMs, use the `smoke-test` skill. It
  isolates `MSB_HOME` and the daemon from the user's, and loads locally built
  images with `msb load`, so no registry is needed.

**Debugging**

- `fbkd` maps many errors to a generic gRPC `Internal` status. Read the daemon
  log (`$XDG_STATE_HOME/firebrick/`, by default `~/.local/state/firebrick/`) or
  call the crate directly to see the real `MicrosandboxError`.
- `msb` reproduces behavior without firebrick, for example `msb run <image> --
  <cmd>`, `msb create --name <n> --log-level debug <image>` and `msb exec <n> --
  <cmd>`. Run it as `$MSB` inside a `smoke-test` environment, and remove what
  you create with `msb stop` and `msb rm`.

## Keep this skill current

When you confirm a new behavior or limitation that a later change could depend
on, add it under **Known behavior** in the same change. When a dependency update
changes one of the items, update or remove it.
