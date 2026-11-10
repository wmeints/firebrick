# 59. Size the root disk and grow it on start

## Status

Accepted

## Context

Microsandbox keeps the writable layer of a sandbox's overlayfs root filesystem
on a sandbox-owned ext4 disk. `fbkd` never set its size, so every sandbox got
microsandbox's default of 4 GiB. That fills up as soon as an agent installs a
few toolchains with `apt` or mise, and the only way out was `fbk rm --force &&
fbk start`, which throws away everything installed in the sandbox.

Microsandbox 0.7.14 can set the size at create time with
`SandboxBuilder::root_disk(mib)` and grow the disk of an existing sandbox with
`modify().root_disk_size(mib)`. Growing works on a stopped sandbox and keeps the
disk's contents. It can't shrink a disk, and refuses a request for the size the
disk already has.

## Decision

The optional `volumes.root` field in `.firebrick.yml` sets the size of the root
disk, next to `volumes.docker` and in the same units. It defaults to `16 GiB`
and must be at least `4 GiB`, microsandbox's own default, which the images fit
in. The gRPC `SandboxVolumes` message mirrors it as `root`; an empty value means
the default, so older clients get 16 GiB.

`fbkd` creates sandboxes with that size. When `fbk start` starts an existing,
stopped sandbox whose root disk is smaller than the requested size, `fbkd` grows
it before the boot, like it applies `env` then. It never restarts a running
sandbox for it and never shrinks a disk. Instead, `SandboxStarted` carries a
`RootDiskNotice` when the disk didn't get the requested size: the sandbox runs
and grows on its next start, the requested size is smaller than the disk, or
growing failed. `fbk start` prints the notice; the start itself doesn't fail.

`fbk network` keeps the disk, because it recreates the sandbox from a snapshot
of it. Microsandbox records a disk restored from a snapshot without a size,
which reads as its 4 GiB default, so `fbkd` records the old size on the
recreated sandbox; otherwise the next `fbk start` would compare the requested
size with the wrong one. `fbk upgrade` gives the new root filesystem the old
size, or the default when that is larger.

## Consequences

- Sandboxes created by an older version grow from 4 GiB to 16 GiB on their next
  start from a stopped state, without being recreated and without losing what's
  installed in them.
- A larger `volumes.root` applies to an existing sandbox after `fbk stop && fbk
  start`, unlike the other resources, which apply when the sandbox is created.
- Making a root disk smaller still needs `fbk rm --force && fbk start`.
- The disk is a sparse file, so a larger size only takes the host space that the
  guest has written.
- A sandbox with a tmpfs or a user-supplied disk image as its root disk isn't
  resized; `fbkd` doesn't create such sandboxes.
