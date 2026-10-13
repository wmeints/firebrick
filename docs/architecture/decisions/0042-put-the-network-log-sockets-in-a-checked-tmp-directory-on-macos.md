# 42. Put the network log sockets in a checked /tmp directory on macOS

## Status

Accepted

## Context

`fbkd` creates the network log sockets of sandboxes in
`$XDG_RUNTIME_DIR/firebrick/netlog`
([ADR 0038](0038-record-network-activity-with-a-guest-logger-on-vsock.md)), and
gives sandboxes no network log without `XDG_RUNTIME_DIR`, so the sockets never
end up in a directory another user can create first. macOS has no
`XDG_RUNTIME_DIR`, so on macOS no sandbox got a network log, and the `vm-tests`
that use the sockets panicked unless the developer exported the variable.

The directory has to meet these constraints:

- Another user must not be able to create it first and take over the sockets.
  [ADR 0023](0023-keep-a-firebrick-microsandbox-home-in-the-xdg-state-directory.md)
  and #130 fail closed instead of falling back to a shared directory.
- microsandbox refuses host paths that go through a symlink, and `/tmp` and
  `/var` are symlinks into `/private` on macOS, so the path must be canonical.
- The socket path `<dir>/<sandbox>.sock` must fit in a macOS unix socket address
  of 104 bytes, including its terminating zero. Sandbox names are DNS labels of
  up to 63 characters.

The candidates were:

- The canonical temp directory, `/private/var/folders/<..>/T/`, which macOS
  creates per user with mode `0700`. It's private by construction, but it's 49
  bytes long, which leaves about 40 bytes for the sandbox name. Longer names,
  which a long project directory name produces, would get no network log.
- A subdirectory of `~/Library/Caches`. It's private, but longer still, so
  sockets would need short names, such as a hash of the sandbox name, plus a map
  from the socket back to its sandbox that doesn't trust the guest.
- `/private/tmp/fbk-<uid>`, checked before use. It's at most 27 bytes, so every
  valid sandbox name fits, but it's in the shared `/tmp`.

## Decision

On macOS without an absolute `XDG_RUNTIME_DIR`,
`firebrick_utils::netlog_socket_dir` returns `/private/tmp/fbk-<uid>`, with the
effective UID of `fbkd`. An absolute `XDG_RUNTIME_DIR` still wins on every
platform, and Linux without it still gets no sockets.

`firebrick_utils::create_private_dir` creates the directory with mode `0700`
when it's missing and checks it with `lstat`: it must be a directory, not a
symlink, owned by the effective UID and without group or other access. `fbkd`
checks the socket directory, also `$XDG_RUNTIME_DIR/firebrick/netlog`, when it
starts and before it binds each socket. When the check fails, it logs a warning
that names the directory: at startup, sandboxes then get no route; at a bind,
the sandbox gets no listener.

The check makes the shared `/tmp` safe enough:

- Another user who creates `/private/tmp/fbk-<uid>` first owns it, or makes it a
  symlink, so `fbkd` refuses it. That denies the network log, which is a
  diagnostic aid, but never hands the sockets to the other user.
- Once the directory is `fbkd`'s, `/tmp` has the sticky bit, so only its owner
  and root can rename or remove it. Nobody else can swap it between the check
  and the bind. Its mode `0700` keeps other users from its sockets.
- macOS's daily cleanup of `/tmp` removes old regular files and empty
  directories, not sockets. When it removes an empty directory, the check before
  the next bind creates it again.

`firebrick-utils` gets the `nix` crate with its `user` feature, which `fbk`
already uses, to read the effective UID.

## Consequences

- On macOS every sandbox gets a network log without setting `XDG_RUNTIME_DIR`,
  and the `vm-tests` run without it.
- Another local user can deny the network log of a user on macOS by creating
  their directory first. `fbkd` then logs the directory and the reason, and the
  user can set `XDG_RUNTIME_DIR` to a private directory.
- A `$XDG_RUNTIME_DIR/firebrick/netlog` that isn't private is now refused too.
- `fbkd.sock` keeps its temp directory fallback.
