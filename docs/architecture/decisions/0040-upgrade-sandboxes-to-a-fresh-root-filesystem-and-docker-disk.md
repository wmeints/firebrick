# 40. Upgrade sandboxes to a fresh root filesystem and Docker disk

## Status

Accepted

## Context

A sandbox keeps the image it was created with
([ADR 0005](0005-default-to-the-anvil-base-image-of-the-same-release.md)). To
get a new release's base image, or the image a changed `image` in
`.firebrick.yml` names, the user had to run `fbk rm` and `fbk start`, which also
drops the sandbox's secrets scope, ports and network rules and needs the
approval of its mounts and network settings again
([ADR 0037](0037-approve-mounts-and-network-changes-in-the-spec.md)).

`fbk network` already recreates a sandbox while it keeps its settings
([ADR 0021](0021-recreate-sandboxes-from-a-disk-snapshot-to-change-their-network-rules.md)),
so an upgrade could follow the same steps with a new image. Carrying the old
disks over to the new image doesn't work in microsandbox 0.7.8:

- A disk snapshot pins its image by manifest digest. A create from it refuses
  another image (`snapshot image digest mismatch`), `.image(..)` after
  `override_snapshot` drops the snapshot, and `RestoreBuilder` can't take an
  image. So the old root filesystem can't go onto a new image.
- The Docker disk ([ADR 0015](0015-give-each-sandbox-a-docker-data-disk.md)) is
  a sandbox-owned volume. microsandbox only formats owned volumes, empty, when
  it creates a sandbox, and restores them only together with the snapshot's root
  filesystem. A sandbox that `fbk network` recreated keeps its Docker disk as a
  qcow2 chain with a journal rather than a `disk.raw`. Moving the disk at the
  host would mean writing into microsandbox's private on-disk layout.
- Copying `/var/lib/docker` inside the guests would need a helper sandbox
  restored from the snapshot, `tar` with extended attributes in both images, and
  stopping `dockerd` in the new sandbox while the copy runs.
- The SDK has no call that only pulls an image: the pull happens inside a
  create. Its default pull policy, `IfMissing`, also never notices that a moving
  tag such as `node:22` now points to another digest.

## Decision

`fbk upgrade [NAME]` sends the sandbox's name and the `image` from the spec of
its workspace, or an empty image for the daemon's default, to a new streaming
`UpgradeSandbox` RPC. `fbkd`:

1. Refuses a paused sandbox with `FAILED_PRECONDITION`.
2. Pulls the image by creating a short-lived probe sandbox from it with
   `PullPolicy::Always` and no network, streaming the pull progress like
   `StartSandbox`
   ([ADR 0024](0024-stream-image-pull-progress-from-startsandbox.md)). It reads
   the manifest digest from the probe's stored config and removes the probe. A
   failed pull fails like `fbk start` and leaves the sandbox alone.
3. Leaves the sandbox alone when the digest matches the digest the sandbox was
   created from, which microsandbox keeps in the sandbox's config
   (`manifest_digest`).
4. Otherwise recreates it like ADR 0021: stop it when it runs, take a disk
   snapshot, remove it, and create it again with the same settings, secrets and
   egress rules, but from the new image with a fresh Docker disk of the same
   size. It installs the workspace's mise tools again when mise is enabled,
   stops the sandbox again when it was stopped, and deletes the snapshot.

The snapshot is only a recovery point: when the create fails, `fbkd` keeps it
and names its path in the error, as for `fbk network`.

## Consequences

- The upgrade keeps the workspace, extra mounts, `init`, resources, Docker disk
  size, host name, ports, network rules, mise setting and secrets, but not
  installed packages, files outside the workspace or Docker images, containers
  and volumes. Users rebuild or pull those again.
- `fbk upgrade` needs no approval, because it doesn't apply the spec's other
  settings.
- Each upgrade boots an extra probe VM for a moment, also when the sandbox turns
  out to be up to date. A probe that `fbkd` couldn't remove, for example because
  it crashed, stays listed as `fbk-upgrade-<pid>-<n>` until `msb rm` removes it.
- The digest comes from a field microsandbox doesn't expose in its API, so it is
  read from the config's JSON. The `vm-tests` (`upgrade_*` in
  `crates/daemon/tests/grpc.rs`) fail when that changes.
- A sandbox created before the `firebrick.network` label existed but with a
  network policy gets the deny-all rules `fbk network log` shows for it.
