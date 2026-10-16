# Solution strategy

## Solutions for quality goals

- **Security:** Agents can safely run inside the sandbox without touching data
  on the host.

  We solve this by running the agent inside a microVM. The VM has no connections
  to the host other than the volume mounts we expose to the VM. We use secret
  proxying to ensure we don't expose any real secrets to the sandbox.

- **Security:** Agents cannot access outside website without express permission
  from the user.

  We'll inject a proxy with TLS interception into the VM so we can look at
  outgoing traffic. We'll allow users to provide policies to control what
  traffic is allowed and what isn't. We will need to provide a balanced list of
  allowed domains out of the box to ensure the user isn't frustrated with
  unnecessary network blockages.

- **Performance**: Sandboxes start within seconds so work can continue quickly.

  We'll use a MicroVM-based solution for the sandboxes as they start very
  quickly and support snapshotting + suspending guests. We can keep sandboxes
  alive and allow users to connect from multiple terminal windows to the same VM
  so the down time is zero if the sandbox is running.

- **Compatibility:** Sandboxes use OCI images to ensure developers can easily
  build sandboxes on the `firebrick-base` image or their own custom images. Each
  image provides an unprivileged `agent` user with UID and GID `1000`.

  We'll use container images as the basis because it's so well-known in the
  community. We'll extend this with a specific configuration format to allow the
  user to specify additional port mappings, volume mappings, and resources such
  as CPU and memory.

- **Reliability:** Sandboxes can be easily restarted and recreated should they
  fail.

  We'll use a dedicated daemon for managing the sandboxes. Before running a
  command, the CLI checks the sandbox and creates or starts it when needed. A
  broken sandbox can be removed and recreated from its spec.

## Technology choices

- [Microsandbox][MICROSANDBOX]: Sandboxes run as microVMs managed through
  microsandbox. It boots OCI images as lightweight VMs, which gives us the
  flexibility of container images with the isolation of a VM. Its built-in SSH
  server backs the `<name>.fbk` host names.

- [Clap][CLAP]: The `fbk` CLI and the `fbkd` command line are built with clap's
  derive API.

- [Tokio][TOKIO]: Both executables use tokio as their async runtime.

- [Tonic][TONIC]: The CLI talks to the daemon over gRPC using tonic. The API is
  defined in `crates/proto/proto/daemon.v1.proto` and compiled at build time
  with `tonic-prost-build`.

- [Serde YAML][SERDE_YAML]: The `.firebrick.yml` sandbox spec is parsed with
  serde_yaml.

- [Tracing][TRACING]: Both executables emit logs and diagnostics through
  tracing, so they can be exported as OpenTelemetry data.

## Solution structure

The application has two executables:

- `fbkd` - The daemon process managing the lifecycle of the sandboxes. It
  exposes the `SandboxManagementService` gRPC API on a unix socket and
  provisions the SSH keys and config used to reach the sandboxes.

- `fbk` - The CLI for working with sandboxes. It is a client of the daemon,
  starts the daemon when it isn't running, and houses the commands to start,
  stop, list, remove and validate sandboxes, run commands in them and tunnel SSH
  connections to them.

Both executables live in a single Cargo workspace. The `firebrick` crate builds
them from the `app::main` entry points of the CLI and daemon libraries. Each
crate has its own folder under `crates/`:

| Crate              | Folder             | Purpose                                                                     |
| ------------------ | ------------------ | --------------------------------------------------------------------------- |
| `firebrick`        | `crates/firebrick` | The `fbk` and `fbkd` binaries, which call the CLI and daemon libraries.     |
| `firebrick-cli`    | `crates/cli`       | The `fbk` library: commands, daemon client, terminal sessions, SSH proxy.   |
| `firebrick-daemon` | `crates/daemon`    | The `fbkd` library: gRPC server, sandbox lifecycle, SSH provisioning.       |
| `firebrick-proto`  | `crates/proto`     | Generated gRPC client and server code for the daemon API.                   |
| `firebrick-spec`   | `crates/spec`      | Parses and validates the `.firebrick.yml` sandbox spec.                     |
| `firebrick-utils`  | `crates/utils`     | Shared helpers, such as the paths of the daemon socket, logs and SSH files. |

The gRPC contract shared by the CLI and the daemon lives in
`crates/proto/proto/daemon.v1.proto`. The `firebrick-proto` crate generates the
client and server code from it in its `build.rs`, and both executables re-export
it as their `api` module. Keeping the proto file inside a crate lets the crates
be published to crates.io; see
[ADR 0013](decisions/0013-share-the-grpc-code-through-a-proto-crate.md).

[MICROSANDBOX]: https://docs.microsandbox.dev/getting-started/introduction
[CLAP]: https://docs.rs/clap/latest/clap/
[TOKIO]: https://tokio.rs/
[TONIC]: https://github.com/grpc/grpc-rust
[SERDE_YAML]: https://docs.rs/serde_yaml/latest/serde_yaml/
[TRACING]: https://github.com/tokio-rs/tracing
