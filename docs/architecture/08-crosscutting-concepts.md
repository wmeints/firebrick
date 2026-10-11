# Crosscutting concepts

## Error reporting in the daemon

The daemon doesn't send the errors of microsandbox, the secret store or the SSH
server to the client. It replaces them with a short, generic message, such as
`failed to remove sandbox`, and a matching gRPC status code. Wherever it does
that, it first logs the original error with `tracing::error!(error = ?err,
"<message>")`, naming the sandbox, secret or host name involved. The error is
logged with its `Debug` representation, because the `Display` representation of
these errors leaves out their source, which is what's needed to debug an
internal error. The log is written to the console and to `fbkd.log` in the
daemon's log directory.

A failure the user can cause and fix, such as connecting to a stopped sandbox,
is logged as a warning instead, so it doesn't hide real internal errors.

Because the `Debug` representation includes every source, an error type must not
carry secret values anywhere in its chain. serde_yaml quotes the values it can't
parse, so a parse error of the secrets file only keeps the line and column of
the error, not the serde_yaml error itself.

Listing sandboxes is the one call where a missing sandbox isn't an error of the
request. microsandbox reloads each sandbox after it reads a page of them, so a
sandbox that another process removes in between fails the whole listing with
`SandboxNotFound`. `sandboxes::list_all` then starts the listing over, up to
five times, before `fbkd` reports the error.

The `vm-tests` in `crates/daemon/tests/grpc.rs` run the daemon in-process and
install a `tracing` subscriber that writes through libtest's output capture, at
level `info` unless `RUST_LOG` says otherwise. A failing test prints the
daemon's log lines, including the original error behind a generic status, next
to its panic message; a passing test prints nothing extra. The tests use the
current-thread runtime, so the daemon logs on the test's own thread and its
lines end up under the right test.

Concurrent test runs share the microsandbox home `/tmp/firebrick-msb`, so every
sandbox a test creates is named `fbk-it-<pid>-<test>` after the test process id,
and so are its host workspace and mount directories. Before the first test of a
process starts a daemon, the harness sweeps the home once: it kills and removes
every `fbk-it-<pid>-*` sandbox whose process no longer runs, with its host
directories, so a killed run doesn't leave VMs behind. Sandboxes of live
processes and names without a pid are left alone, and a failure to remove one
leftover is logged as a warning. When the home can't be listed at all, the test
fails with a message to remove the home and run the tests again.

## Secret scopes

Each secret in `secrets.yml` is global or belongs to one sandbox, through an
optional `sandbox` field
([ADR 0022](decisions/0022-scope-secrets-per-sandbox.md)). An entry without it
is global, so files from before scopes load unchanged. A name is unique within a
scope, but the same name may exist globally and for any number of sandboxes.

| Change                         | Sandboxes it applies to                                                                                                                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Set or remove a global secret  | Every sandbox firebrick created, except the ones with a sandbox-scoped secret with that name.                                                                                              |
| Set a sandbox-scoped secret    | Its sandbox only, replacing the global secret with that name there.                                                                                                                        |
| Remove a sandbox-scoped secret | Its sandbox only, which gets the global secret with that name back when there is one.                                                                                                      |
| Create or recreate a sandbox   | The global secrets, with the sandbox's own sandbox-scoped secrets in their place. A brand-new sandbox has none, because a sandbox-scoped secret can only be set for a sandbox that exists. |
| Remove a sandbox               | `fbkd` removes its sandbox-scoped secrets from `secrets.yml`, so a later sandbox with the same name doesn't inherit them.                                                                  |

So in a sandbox, a sandbox-scoped secret wins over the global secret with the
same name. Like every secret change, these apply the next time a sandbox starts.

## Approving spec changes

The agent can edit `.firebrick.yml`, because it lives in the workspace. So the
CLI's `approval` module keeps the `mounts` and `network` settings the user
approved per workspace in `approved.yml` in the firebrick config directory
(`$XDG_CONFIG_HOME/firebrick`, falling back to `$HOME/.config/firebrick`), keyed
by the canonical workspace path
([ADR 0037](decisions/0037-approve-mounts-and-network-changes-in-the-spec.md)).
A workspace without an entry counts as approved for no mounts and the default
network section.

| Command                    | Compares                                                                                                  | Asks when                                                                                                 |
| -------------------------- | --------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `fbk start`, `fbk run`     | The resolved mounts, with canonical host paths, and the network section of a sandbox it's about to create | They differ from the approved ones.                                                                       |
| `fbk network <subcommand>` | The network section it's about to send to `fbkd`                                                          | It differs from the approved section with the subcommand's change applied, and from the approved section. |

Mounts and network rules are compared as sets, and a missing `enabled` equals
`enabled: true`. When the settings differ, `fbk` prints each added (`+`) or
removed (`-`) mount and rule and each changed (`~`) switch on stderr, then asks
`Apply these changes? [y/N]`. Only `y` or `yes` approves; any other answer, or
EOF, aborts before the sandbox is created or `UpdateNetwork` is sent, and `fbk
network` keeps its edit to the file. Without a terminal on stdin the command
fails unless `--yes` is passed, which approves without asking but still prints
the difference. Approved settings are recorded, so the next command with the
same settings doesn't ask. `fbk network` records the section it sends and keeps
the approved mounts.

`approved.yml` is written with mode `0600` through a temp file and a rename. A
file that can't be read or isn't valid YAML fails the command, so a broken file
never counts as approving nothing. Starting an existing sandbox doesn't check
anything, because microsandbox only applies mounts when it creates a sandbox.

## Securing the daemon socket

Any process that can use the daemon socket can start sandboxes, read workspace
files through them and manage secrets, so the daemon restricts the socket to the
user it runs as, in two layers:

- After binding the socket, `server` sets its mode to `0600`, whatever the
  umask. This matters most when the socket falls back to the shared temp dir
  because `XDG_RUNTIME_DIR` isn't set. When setting the mode fails, the daemon
  removes the socket and exits instead of serving on an unprotected socket.
- For every accepted connection, `server` reads the peer's credentials with
  `SO_PEERCRED` and hands the connection to tonic only when the peer's UID is
  the daemon's effective UID or `0`. The daemon's UID is read from the owner of
  the socket it just bound, so it needs no extra dependency. Any other
  connection, and a connection whose credentials can't be read, is closed before
  it serves a request, and the daemon logs a warning with the peer's UID and PID
  or the I/O error. It keeps accepting other connections.

Root is allowed on purpose: root can bypass the check anyway, for example by
reading the daemon's memory, so rejecting it adds no security and only gets in
the way of an administrator.
