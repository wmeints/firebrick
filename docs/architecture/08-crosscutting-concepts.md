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

Text that a sandbox printed is untrusted: a workspace file controls it. Before
such guest output ends up in a gRPC status message or a log line, such as the
tail of mise's stderr in a failed `fbk start` or the URL relay's stderr when it
exits, the daemon removes every control character except newline and tab with
`firebrick_utils::strip_control`. That covers C0 controls, DEL and C1 controls,
so a workspace can't make `fbk` write terminal escape sequences, such as OSC 52
to set the user's clipboard. The text around the removed characters stays, so
the message still shows what the guest printed. The output of `fbk run` is the
exception: like `ssh`, the CLI passes it to the terminal unchanged, so TUIs keep
working.

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
the way of an administrator. The rule lives in
`firebrick_utils::is_trusted_peer`, which the daemon and the CLI share.

The CLI checks the other direction. Where the socket falls back to the shared
temp dir, another user can bind `fbkd.sock` first and receive every request,
including the values of `fbk secret set`. After `client` connects to the socket,
also to a daemon it just spawned, it reads the peer's credentials with tokio's
`UnixStream::peer_cred` and uses the connection only when the peer's UID is the
effective UID of `fbk` or `0`, for the same reason the daemon allows root.
Otherwise it fails with `refusing to use <socket path>: it's served by UID
<uid>, not by you` before it sends a request. On macOS the temp dir is a private
per-user directory, so the check matters on Linux without `XDG_RUNTIME_DIR`.

The daemon's data and logs never fall back to the temp dir. A user who creates
`/tmp/firebrick` first could read the secrets and SSH keys or plant an SSH
config with a `ProxyCommand`, which `~/.ssh/config` includes. `firebrick-utils`
returns no data directory when neither `XDG_DATA_HOME` nor `HOME` is an absolute
path, and no log directory when neither `XDG_STATE_HOME` nor `HOME` is, and
`fbkd` then exits at startup with `can't choose a data directory: set HOME or
XDG_DATA_HOME to an absolute path` or the matching message for the logs, as it
does for the microsandbox home
([ADR 0023](decisions/0023-keep-a-firebrick-microsandbox-home-in-the-xdg-state-directory.md)).
The CLI starts the daemon with its stderr discarded, so that message would never
reach the user. Before it starts the daemon, `client` asks
`firebrick_utils::missing_dir` whether the same environment has a data and a log
directory, and fails with the daemon's message instead.
