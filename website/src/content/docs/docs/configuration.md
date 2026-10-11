---
title: Configuration
description: Configure a sandbox with .firebrick.yml, and where Firebrick keeps its files.
---

Run `fbk` from your project directory. Firebrick derives the sandbox from that
directory, and reads its settings from a `.firebrick.yml` file there.

## The `.firebrick.yml` file

`fbk init` writes a `.firebrick.yml` with the defaults to the working directory,
named after the directory. In a directory called `My.App`, it writes:

```yaml
name: my-app
resources:
  cpu: 2
  memory: 4 GiB
image: ghcr.io/wmeints/firebrick-base:v<version>
init: true
mise: true
volumes:
  docker: 20 GiB
```

`<version>` is the installed Firebrick version. `fbk init` refuses to replace an
existing file; `fbk init --force` overwrites it.

| Field               | Description                                                             | Default                                     |
| ------------------- | ----------------------------------------------------------------------- | ------------------------------------------- |
| `name`              | Name of the sandbox.                                                    | Required                                    |
| `image`             | OCI image the sandbox runs.                                             | `ghcr.io/wmeints/firebrick-base:v<version>` |
| `init`              | Run the image's `/sbin/init` as PID 1.                                  | `true`                                      |
| `mise`              | Trust and install the project's mise tools when it starts.              | `true`                                      |
| `resources.cpu`     | Number of vCPUs.                                                        | `2`                                         |
| `resources.memory`  | Memory in `Mi`/`MiB` or `Gi`/`GiB`, such as `512 MiB` or `4Gi`.         | `4 GiB`                                     |
| `volumes.docker`    | Size of the Docker data disk, in the same units as `memory`.            | `20 GiB`                                    |
| `network.enabled`   | Give the sandbox a network device.                                      | `true`                                      |
| `network.enforce`   | Deny outgoing traffic unless a rule allows it.                          | `false`                                     |
| `network.allow`     | Destinations the sandbox may connect to.                                | Empty                                       |
| `network.deny`      | Destinations the sandbox may not connect to, even if allowed.           | Empty                                       |
| `ports`             | Host ports to forward to the sandbox.                                   | Empty                                       |
| `mounts[].host`     | Host directory: absolute, `~/...`, `~user/...` or relative to the spec. | Required per mount                          |
| `mounts[].guest`    | Absolute guest path to mount it at. Each path may appear only once.     | Required per mount                          |
| `mounts[].readonly` | Mount the directory read-only.                                          | `false`                                     |

When you set `resources`, set both `cpu` and `memory`. The `network` and `ports`
fields are described in [Networking](/firebrick/docs/networking/), and `image`
and `init` in [Custom images](/firebrick/docs/custom-images/).

Firebrick rejects fields it doesn't know, so a typo such as `memroy` is an error
rather than a setting that's silently ignored. Check the file with `fbk
validate`, which prints the line and column of each problem:

```sh
$ fbk validate
.firebrick.yml:4:3: error: resources: unknown field `memroy`, expected `cpu` or `memory`
```

`fbk start` and `fbk run` refuse to use an invalid file.

## Without a `.firebrick.yml`

Without the file, Firebrick uses the defaults and names the sandbox `firebrick-`
followed by the first 6 characters of the SHA-256 hash of the full path of the
working directory, such as `firebrick-d9f287`. A sandbox created by an older
version keeps its name, such as `home_user_my_project`.

Two directories can, rarely, hash to the same name. `fbk start` and `fbk run`
then refuse to use the first directory's sandbox from the second one, so an
agent can't reach the other project's files:

```text
Error: sandbox firebrick-d9f287 belongs to /home/user/my-project; add a .firebrick.yml with its own name to give this directory a separate sandbox
```

Add a `.firebrick.yml` with its own `name` to the second directory to give it a
separate sandbox.

## Applying changes

The image, init, mise setting, resources, volumes, network rules and mounts
apply when the sandbox is created. To change them for an existing sandbox,
remove it and start it again:

```sh
fbk rm --force
fbk start
```

`fbk rm` deletes the sandbox's disk, including the tools installed in it and its
Docker data; your project files stay on the host. There are two exceptions: `fbk
start` applies `ports` to an existing sandbox, also while it runs, and `fbk
network` changes the network rules without removing the sandbox. See
[Networking](/firebrick/docs/networking/).

## Extra mounts

The project directory is mounted read/write at `/workspaces/<leaf>`, where
`<leaf>` is the directory's name. To give the sandbox more host directories,
such as a library checked out next to the project or a dataset, list them under
`mounts`:

```yaml
mounts:
  - host: ../shared-lib
    guest: /workspaces/shared-lib
  - host: ~/datasets/images
    guest: /data/images
    readonly: true
```

A relative `host` resolves against the directory that holds `.firebrick.yml`,
`~` expands to `$HOME`, and `~user` to that user's home directory. `fbk start`
and `fbk run` refuse to create the sandbox when a `host` isn't an existing
directory, or when a `guest` is the workspace path (`/workspaces/<leaf>`) or
`/var/lib/docker`. A `guest` must not contain `..`, `:`, `;` or `,`. The agent
user owns the mounted files, like the workspace.

## mise tools

When the project pins its tools with [mise](https://mise.jdx.dev), Firebrick
installs them when `fbk start` or `fbk run` creates or starts the sandbox, but
not when an SSH connection starts it. It trusts the `mise.toml`, `.mise.toml`,
`mise/config.toml`, `.config/mise.toml` and `.tool-versions` files at the root
of the project and runs `mise install`, so `fbk start` returns with the tools
ready. When `mise install` fails, `fbk start` prints mise's error and the
sandbox keeps running, so you can connect and fix the config. Images without
mise skip this step. Set `mise: false` to turn it off.

## Docker data disk

Every sandbox gets a private ext4 disk mounted at `/var/lib/docker`, so Docker
can store images and containers inside the sandbox; Docker's storage doesn't
work on the sandbox's overlayfs root filesystem. `volumes.docker` sets its size.
The disk keeps its contents when the sandbox stops, and `fbk rm` deletes it with
the sandbox. Sandboxes created by an older version have no Docker data disk
until you recreate them.

## File locations

Firebrick follows the XDG base directories. When an `XDG_*` variable isn't set,
it uses the default under your home directory:

| Path                                                            | Contents                                                                                                         |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `$XDG_RUNTIME_DIR/fbkd.sock`                                    | The socket the CLI and daemon talk over; `fbkd.sock` in the temp directory when `XDG_RUNTIME_DIR` isn't set.     |
| `$XDG_STATE_HOME/firebrick/` (`~/.local/state/firebrick`)       | The daemon's logs, one `fbkd.log.<date>` file per day.                                                           |
| `~/.local/state/firebrick/msb`                                  | Firebrick's microsandbox home: the runtime, images and sandbox disks. `MSB_HOME` overrides it.                   |
| `$XDG_DATA_HOME/firebrick/ssh` (`~/.local/share/firebrick/ssh`) | The SSH keys, `known_hosts` and the generated SSH config. See [Editor support](/firebrick/docs/editor-support/). |
| `~/.local/share/firebrick/secrets.yml`                          | The secrets. See [Secrets](/firebrick/docs/secrets/).                                                            |

The daemon logs at the `info` level. Set `RUST_LOG` in the environment that
starts `fbkd` to change it, for example `RUST_LOG=debug`. The CLI starts `fbkd`
with its own environment, so stop the daemon and run the next `fbk` command with
the variable set:

```sh
pkill -TERM -x fbkd
RUST_LOG=debug fbk ls
```
