# Crosscutting concepts

The [threat model](concepts/threat-model.md) describes the assets, adversaries,
trust boundaries and invariants behind the security concepts below. Update it
with every new channel between the guest and the host, spec setting or host
file.

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
exits, the daemon removes terminal escape sequences and every control character
except newline and tab with `firebrick_utils::strip_control`. It removes whole
CSI, OSC and other `ESC` sequences with `console::strip_ansi_codes`, after it
rewrites the C1 forms of CSI and OSC to their `ESC` forms, then the remaining C0
controls, DEL and C1 controls
([ADR 0044](decisions/0044-strip-escape-sequences-with-console.md)). So a
workspace can't make `fbk` write terminal escape sequences, such as OSC 52 to
set the user's clipboard, and no remainders like `[1;32m` show. An unterminated
sequence is removed up to the end of the text. The text around the removed
sequences stays, so the message still shows what the guest printed. The output
of `fbk run` is the exception: like `ssh`, the CLI passes it to the terminal
unchanged, so TUIs keep working.

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
CLI's `approval` module keeps the `mounts`, `network` and `clipboard` settings
the user approved per workspace in `approved.yml` in the firebrick config
directory (`$XDG_CONFIG_HOME/firebrick`, falling back to
`$HOME/.config/firebrick`), keyed by the canonical workspace path
([ADR 0037](decisions/0037-approve-mounts-and-network-changes-in-the-spec.md)).
A workspace without an entry gets no mounts, the default network section and no
shared clipboard without asking. Any other settings are asked about once, and
listed in full instead of as a difference: a sandbox created before approvals
existed may have enforced rules that the agent has since removed from the file,
so `fbk` can't assume what was approved before.

| Command                    | Compares                                                                                                                        | Asks when                                                                                                 |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `fbk start`, `fbk run`     | The resolved mounts, with canonical host paths, the network section and the clipboard setting of a sandbox it's about to create | They differ from the approved ones.                                                                       |
| `fbk network <subcommand>` | The network section it's about to send to `fbkd`                                                                                | It differs from the approved section with the subcommand's change applied, and from the approved section. |

Mounts and network rules are compared as sets, and a missing `enabled` equals
`enabled: true`. When the settings differ, `fbk` prints each added (`+`) or
removed (`-`) mount and rule, each changed (`~`) network switch and `+
clipboard: shared with the host, both ways` or `- clipboard: shared with the
host, both ways` on stderr, then asks `Apply these changes? [y/N]`. Only `y` or
`yes` approves; any other answer, or EOF, aborts before the sandbox is created
or `UpdateNetwork` is sent, and `fbk network` keeps its edit to the file.
Without a terminal on stdin the command fails unless `--yes` is passed, which
approves without asking but still prints the difference. Approved settings are
recorded, so the next command with the same settings doesn't ask. `fbk network`
records the section it sends and keeps the approved mounts and clipboard
setting. `approved.yml` stores the approved clipboard as a mode, `both` for
`clipboard: true`, and leaves it out when it's off, so a file without the field
loads with it off. Files from before the sandbox could copy store `clipboard:
true`, which loads as the mode `paste`: what the user approved changed, so the
next `fbk start` of such a workspace asks again, with `- clipboard: shared with
the host, paste only` and `+ clipboard: shared with the host, both ways`. Older
`fbk` versions can't read a file that stores `clipboard: both`, and fail with
`isn't a valid approvals file` instead of treating it as approved.

`approved.yml` is written with mode `0600` through a temp file and a rename,
with `firebrick_utils::write_private`, which `secrets.yml` uses too. The CLI
turns the request's mounts into `MountSpec`s with the same `From<&Mount>`
conversion in `firebrick-proto` that the daemon uses to create them, so the
approved mounts are the ones the daemon applies. A file that can't be read or
isn't valid YAML fails the command, so a broken file never counts as approving
nothing. Starting an existing sandbox doesn't check anything, because
microsandbox only applies mounts, and `fbkd` the clipboard setting, when it
creates a sandbox.

## Outdated sandbox images

A sandbox keeps the image it was created with
([ADR 0005](decisions/0005-default-to-the-anvil-base-image-of-the-same-release.md)),
so after a firebrick update it still runs the `firebrick-base` image of the old
release. The daemon decides whether a sandbox is outdated, because it creates
sandboxes from its own `DEFAULT_IMAGE` and `fbk` can be another version:

- `sandboxes` records the resolved image reference in the `firebrick.image`
  label when it creates a sandbox, and sets it to the new image when it upgrades
  one. A network update keeps it, like every label. For a sandbox created before
  the label existed, it reads the OCI reference from the sandbox's microsandbox
  config; without one, the image is unknown.
- A sandbox is outdated when its image is another tag of the default image's
  repository, `ghcr.io/wmeints/firebrick-base`. Sandboxes with any other image
  never are, because a firebrick update doesn't change their image. `GetSandbox`
  returns the image as `image` and, for an outdated sandbox, the default image
  as `upgrade_image`.

`fbk start`, `fbk run`, the `fbk network` changes and `fbk port forward|rm`
print a warning to stderr for an outdated sandbox that points to `fbk upgrade`,
or to `fbk upgrade <name>` when the command got the sandbox's name. When the
`.firebrick.yml` in the sandbox's workspace names the image the sandbox runs,
`fbk upgrade` would keep it, so the warning asks to change or remove `image`
first. `fbk init` leaves `image` out of the spec for that reason, so a sandbox
follows the release's default image. `fbk run` prints it before the session
takes over the terminal. A failed lookup skips the warning, which never changes
the exit code. Commands that don't boot or change the sandbox, and `fbk
ssh-proxy`, whose stderr would end up in the SSH session, don't print it.

## Validating sandbox names

`fbkd` names files after sandboxes: the network log and clipboard sockets
(`<name>.sock`) and the network records (`<name>.json`). The name comes from
`.firebrick.yml`, which the agent in the sandbox can edit, and microsandbox only
checks it when it creates a sandbox, after `fbkd` has bound the sockets and
loaded the record. A name like `../../x` would make `fbkd` delete, create and
read files outside its directories, so the name is checked in three places, all
with microsandbox's own rule: 1 to 128 bytes, an ASCII letter or digit first,
then only ASCII letters, digits, `.`, `-` and `_`.

- `firebrick_spec` checks `name` while it parses the spec, so `fbk validate` and
  every other command report an invalid name at its line and column and never
  send it to the daemon. The rule lives in
  `firebrick_spec::validate_sandbox_name`, which the CLI and the daemon share.
- `server` checks the name at the top of every RPC that takes one, including the
  scope of a secret, and fails with `INVALID_ARGUMENT` before it touches the
  file system or microsandbox.
- `vsock::sandbox_file` returns no path for a name that isn't a single path
  component, so a name that slips past the other checks can't leave the socket
  or record directory.

Every name microsandbox accepts passes, so existing specs and sandboxes keep
working.

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
