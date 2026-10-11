---
title: Custom images
description: Build a sandbox image on firebrick-base, or bring your own.
---

Sandboxes run an OCI image. By default that's `firebrick-base` of the installed
Firebrick version, `ghcr.io/wmeints/firebrick-base:v<version>`. Point `image` in
`.firebrick.yml` at another image to give a project the tools it needs from the
start.

## What `firebrick-base` contains

The base image is built from the
[`Dockerfile`](https://github.com/wmeints/firebrick/blob/main/Dockerfile) in the
repository, for `linux/amd64` and `linux/arm64`:

- Ubuntu 26.04 with `build-essential`, `curl`, `git`, `gpg`, `libicu78` (ICU for
  .NET apps such as the Aspire CLI), `libssl-dev`, `pkg-config`, `procps`,
  `python3`, `sudo`, `tcpdump` and [tini](https://github.com/krallin/tini).
- [mise](https://mise.jdx.dev), activated for bash, with its shims on the
  `PATH`.
- The Docker engine with the `buildx` and `compose` plugins.
- An `agent` user with UID `1000` and GID `1000`, home directory `/home/agent`,
  passwordless `sudo` and membership of the `docker` group. The image's `ubuntu`
  user is removed.
- An `/sbin/init` that disables guest IPv6 (see
  [Networking](/docs/networking/#ipv6)), starts `dockerd` and the network logger
  in the background and hands PID 1 to tini, which reaps zombie processes.
- `firebrick-netlog`, the network logger, which reports the domains and IP
  addresses the sandbox contacts to `fbkd`. It logs to
  `/var/log/firebrick-netlog.log`. See
  [Recording network activity](/docs/networking/#recording-network-activity).
- `xdg-open` and `$BROWSER` pointing at `firebrick-open`, which opens URLs in
  the browser on your host.
- `firebrick-git-credential`, the git credential helper for github.com that
  hands git the `GH_TOKEN` or `GITHUB_TOKEN` placeholder. See
  [Git over HTTPS](/docs/secrets/#git-over-https).

Docker runs directly on the sandbox VM and keeps its images and containers on
the sandbox's own disk at `/var/lib/docker`, so they survive `fbk stop` and `fbk
start`. `agent` can run `docker`, `docker compose` and `docker buildx` without
`sudo`. When `dockerd` fails to start, the reason is in `/var/log/dockerd.log`.

## Building on `firebrick-base`

The simplest custom image starts from the base image and adds a toolchain. This
`Dockerfile` installs Python and uv with mise, for every shell of the `agent`
user:

```dockerfile
FROM ghcr.io/wmeints/firebrick-base:v0.6.0

RUN mise use -g python@3.13 uv@latest

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql-client \
    && rm -rf /var/lib/apt/lists/*
USER agent
```

The base image's user is `agent`, so switch to `root` for system packages and
back to `agent` at the end.

Build the image and push it to a registry the sandbox can pull from, such as the
GitHub Container Registry:

```sh
docker build -t ghcr.io/<you>/my-sandbox:1 .
docker push ghcr.io/<you>/my-sandbox:1
```

Then point the project's `.firebrick.yml` at it:

```yaml
name: my-project
image: ghcr.io/<you>/my-sandbox:1
```

The image applies when the sandbox is created, so recreate an existing sandbox:

```sh
fbk rm --force
fbk start
```

## Bringing your own image

Firebrick runs everything in a sandbox as the `agent` user. An image that isn't
based on `firebrick-base` must:

- Have a user named `agent` with UID `1000` and GID `1000` and a home directory,
  such as `/home/agent`. Images based on Ubuntu ship an `ubuntu` user with UID
  1000; remove it first.
- Set `USER agent`. `fbk run` runs commands as the image's user, while SSH
  always logs in as `agent`.
- Install `sudo` and allow `agent` to use it without a password, if agents
  should be able to install system packages.
- Provide an executable `/sbin/init`, or set `init: false` in `.firebrick.yml`.
  With `init` on, which is the default, Firebrick runs `/sbin/init` as PID 1,
  and `fbk start` fails with a hint when the image has none.
- Add `agent` to the `docker` group and start `dockerd` from `/sbin/init`, if
  agents should be able to run `docker` without `sudo`. Firebrick attaches a
  disk for Docker's data at `/var/lib/docker` to every sandbox.
- Copy `/usr/local/sbin/firebrick-netlog` from the base image's `Dockerfile`,
  install `python3` and `tcpdump`, and start the logger as root from
  `/sbin/init`, if Firebrick should record the network activity of the sandbox.
  Without it, the sandbox's network record stays empty.

The workspace is mounted at `/workspaces/<leaf>`, and its files show up as owned
by `agent`, whatever the UID of your user on the host is.

For another distribution, create the user yourself, and set `init: false` in
`.firebrick.yml` or add an init like the base image's:

```dockerfile
FROM alpine:3.22

RUN apk add --no-cache bash git sudo \
    && addgroup -g 1000 agent \
    && adduser -D -u 1000 -G agent -s /bin/bash agent \
    && echo "agent ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/agent

USER agent
WORKDIR /home/agent
```

```yaml
name: my-project
image: ghcr.io/<you>/my-alpine-sandbox:1
init: false
```

With `init: false`, nothing disables guest IPv6, so on hosts without IPv6
internet access the sandbox can't download from servers that have an IPv6
address. Nothing starts `dockerd` either, and `docker` reports `Cannot connect
to the Docker daemon`; start it in the background with `sudo sh -c
'dockerd >/var/log/dockerd.log 2>&1 &'`. Nothing starts the network logger
either, so Firebrick records no network activity for the sandbox.

Sandboxes created by an older version of Firebrick run `ubuntu:26.04`, which has
no `agent` user, so SSH can no longer log in to them. Recreate them with `fbk
rm` and `fbk start`, or connect with `ssh root@<name>.fbk`.
