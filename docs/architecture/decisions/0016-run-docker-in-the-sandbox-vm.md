# 16. Run Docker in the sandbox VM

## Status

Accepted

## Context

Coding agents often need to build images and run containers, for example to
start a database for integration tests or to check a `Dockerfile`. The
`firebrick-base` image has no container runtime, so agents can't do that in a
sandbox.

A sandbox is a microVM with its own kernel, not a container. The kernel supports
the namespaces, cgroups v2 and overlayfs that Docker needs, so the Docker engine
can run directly on the VM instead of nested in another container. `fbkd`
already attaches an ext4 disk at `/var/lib/docker` to every sandbox, so Docker's
overlayfs storage doesn't sit on the overlayfs root filesystem (see
[ADR 0015](0015-give-each-sandbox-a-docker-data-disk.md)).

Ubuntu's own `docker.io` package lags behind Docker's releases and doesn't ship
the `buildx` and `compose` plugins under the names Docker documents.

## Decision

The base image installs `docker-ce`, `docker-ce-cli`, `containerd.io`,
`docker-buildx-plugin` and `docker-compose-plugin` from Docker's apt repository
for Ubuntu, with the repository's key in `/etc/apt/keyrings/docker.asc`.

The image's `/sbin/init` starts `dockerd` as root in the background, after it
disables guest IPv6 and before it hands PID 1 to `tini`. It writes the output of
`dockerd` to `/var/log/dockerd.log` and doesn't wait for it to be ready. `/run`
sits on the sandbox's persistent root filesystem instead of a tmpfs, so before
it starts `dockerd`, the init removes the runtime state that Docker left there
in the previous boot (`/run/docker.pid`, `/run/docker` and `/run/containerd`).
Otherwise `dockerd` finds a stale PID file after `fbk stop` and `fbk start` and
refuses to start.

The `agent` user is a member of the `docker` group, so it can use the Docker
socket without `sudo`.

## Consequences

- Agents can run `docker`, `docker compose` and `docker buildx` as `agent` in
  every sandbox that runs the base image with `init` on.
- Membership of the `docker` group is root-equivalent inside the VM. `agent`
  already has passwordless `sudo`, and the VM is the security boundary, so this
  grants nothing new.
- With `init: false`, nothing starts `dockerd`. Agents can start it in the
  background with `sudo sh -c 'dockerd >/var/log/dockerd.log 2>&1 &'`.
- When `dockerd` fails to start, the sandbox still boots; the reason is in
  `/var/log/dockerd.log`. Nothing restarts `dockerd` when it dies.
- Commands that run right after the sandbox boots may find `dockerd` not ready
  yet.
- The image grows by the size of the Docker packages, and each image build
  downloads from `download.docker.com`.
- Images and containers live on the sandbox's data disk, so they survive `fbk
  stop` and `fbk start`.
- With TLS interception, containers trust the interception CA through the runc
  wrapper that `daemon.json` makes Docker's default runtime (see
  [ADR 0068](0068-trust-the-interception-ca-in-docker-containers-through-a-runc-wrapper.md)).
