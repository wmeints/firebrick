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
init: true
mise: true
volumes:
  docker: 20 GiB
```

The file names no `image`, so the sandbox runs the `firebrick-base` image of the
installed Firebrick version, `ghcr.io/wmeints/firebrick-base:v<version>`, and
`fbk upgrade` moves it to the new release after you update Firebrick. `fbk init`
refuses to replace an existing file; `fbk init --force` overwrites it.

| Field               | Description                                                             | Default                                     |
| ------------------- | ----------------------------------------------------------------------- | ------------------------------------------- |
| `name`              | Name of the sandbox, following the rule below the table.                | Required                                    |
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
| `clipboard`         | Let programs in the sandbox read and write the host clipboard.          | `false`                                     |
| `env`               | Environment variables every process in the sandbox gets.                | Empty                                       |

The `name` is 1 to 128 characters long, starts with a letter or digit, and
contains only ASCII letters, digits, `.`, `-` and `_`, such as `my-project` or
`My.App_2`. This is the rule microsandbox uses, and Firebrick checks it before
the name is used, because the daemon names files after the sandbox.

The `image` is an OCI image reference: an optional registry host with an
optional `:port`, a repository name in lowercase letters, digits, `.`, `-`, `_`
and `/`, then an optional `:tag` and an optional `@<algorithm>:<digest>`, such
as `ubuntu:24.04`, `localhost:5000/my-sandbox:dev`,
`ghcr.io/wmeints/firebrick-base:v0.1.0` or `alpine@sha256:<digest>`. A tag may
also hold uppercase letters. An empty `image` means the default image, like a
missing one. Firebrick checks the reference with the grammar microsandbox pulls
images with, so `fbk validate` reports an image microsandbox can't use, and says
what is wrong with it.

When you set `resources`, set both `cpu` and `memory`. The `network` and `ports`
fields are described in [Networking](/docs/networking/), and `image` and `init`
in [Custom images](/docs/custom-images/), `clipboard` in
[Sharing the host clipboard](#sharing-the-host-clipboard), and `env` in
[Environment variables](#environment-variables).

Firebrick rejects fields it doesn't know, so a typo such as `memroy` is an error
rather than a setting that's silently ignored. Check the file with `fbk
validate`, which prints the line and column of each problem:

```sh
$ fbk validate
.firebrick.yml:4:3: error: resources: unknown field `memroy`, expected `cpu` or `memory`
```

`fbk start` and `fbk run` refuse to use an invalid file.

`.firebrick.yml` must be a regular file. The agent in the sandbox can replace
it, so a symlink could make `fbk` read or overwrite a file outside the
workspace. Every command refuses a symlinked spec, even one that points into the
workspace:

```sh
$ fbk start
Error: .firebrick.yml is a symlink; firebrick only uses a regular file in the workspace
```

A directory, a named pipe or another special file in its place fails the same
way. If you keep the spec in a dotfiles repository, copy it into the workspace
instead of linking it.

## Without a `.firebrick.yml`

Without the file, Firebrick uses the defaults and names the sandbox `firebrick-`
followed by the first 6 characters of the SHA-256 hash of the full path of the
working directory, such as `firebrick-d9f287`. A sandbox created by an older
version keeps its name, such as `home_user_my_project`.

Two directories can, rarely, hash to the same name. `fbk start` and `fbk run`
then refuse to use the first directory's sandbox from the second one, so an
agent can't reach the other project's files:

```text
Error: sandbox firebrick-d9f287 belongs to /home/user/my-project; give this directory its own name in .firebrick.yml to use a separate sandbox
```

Add a `.firebrick.yml` with its own `name` to the second directory to give it a
separate sandbox.

## One sandbox per directory

A sandbox belongs to the directory it was created in. The commands that act on
the working directory's sandbox check this before they change anything: `fbk
start` and `fbk run`, `fbk stop` and `fbk rm` without a name, `fbk network`,
`fbk port`, and `fbk secret set` and `fbk secret rm` with `--scope sandbox`.
When `.firebrick.yml` names another directory's sandbox, for example because an
agent changed its `name`, they refuse with the error above, so the agent can't
remove or reconfigure the other project's sandbox. Change `name` back to give
the directory its own sandbox. `fbk stop <name>` and `fbk rm <name>` act on the
sandbox you name and aren't checked.

## Applying changes

The image, init, mise setting, resources, volumes, network rules, mounts and
clipboard setting apply when the sandbox is created. To change them for an
existing sandbox, remove it and start it again:

```sh
fbk rm --force
fbk start
```

`fbk rm` deletes the sandbox's disk, including the tools installed in it and its
Docker data; your project files stay on the host. There are three exceptions:
`fbk start` applies `ports` to an existing sandbox, also while it runs, it
applies `env` the next time the sandbox starts (see
[Environment variables](#environment-variables)), and `fbk network` changes the
network rules without removing the sandbox. See [Networking](/docs/networking/).

The agent can edit `.firebrick.yml` like any other file in the workspace, so
review changes you didn't make before you apply them. See
[What the agent can change](/docs/#what-the-agent-can-change).

### Approving settings changes

The agent in the sandbox can edit `.firebrick.yml`, because it's part of the
project. To keep it from giving itself more access, `fbk` remembers the
`mounts`, `network` and `clipboard` settings you approved for each project, and
asks before it creates a sandbox with other ones:

```console
$ fbk start
.firebrick.yml changes settings you haven't approved for this workspace:
  + mount /home/me -> /mnt/home (read/write)
  ~ network.enforce: true -> false
  + network.allow: *.attacker.example
  + clipboard: shared with the host, both ways
Apply these changes? [y/N] n
Error: aborted: the changes to mounts, network and clipboard in .firebrick.yml weren't approved
```

Each line shows an added (`+`) or removed (`-`) mount, with the real directory a
symlink points to, or rule, a changed (`~`) network switch, or the clipboard
being shared (`+`) or no longer shared (`-`). Answer `y` to create the sandbox
and remember the settings, so the next `fbk start` with the same settings
doesn't ask. Any other answer creates nothing. The first time a project has
mounts, a `network` section or `clipboard: true`, `fbk` lists all of them under
`.firebrick.yml sets mounts, network or clipboard settings you haven't approved
for this workspace yet:` and asks once, also for a project you used before this
version of Firebrick. Check that `network.enforce` is `true` there when you
expect your rules to be enforced.

Review the changes before you approve them: when you didn't make them, the agent
did. Pass `--yes` to `fbk start` or `fbk run` to approve them without asking,
for example in a script; `fbk` still prints them. Without a terminal and without
`--yes`, the command fails. Starting an existing sandbox never asks, because it
doesn't apply `mounts`, `network` or `clipboard`. The other settings, such as
`image` and `resources`, apply without asking.

## Environment variables

Set plain configuration for the project, such as `NODE_ENV`, `RUST_LOG` or the
URL of a local service, in `env`. Every process in the sandbox gets the
variables, in `fbk run` commands and in `fbk ssh` and `ssh <leaf>.fbk` shells:

```yaml
env:
  NODE_ENV: development
  RUST_LOG: "debug,hyper=info"
  PORT: 3000 # becomes "3000"
  DEBUG: true # becomes "true"
  GREETING: "$HOME" # stays the literal string $HOME
```

```sh
$ fbk run -- printenv GREETING
$HOME
```

The sandbox gets each value as you wrote it: `$VAR` and `${VAR}` aren't
expanded, neither from the host nor in the sandbox. Strings, numbers and
booleans are allowed, and a number or boolean becomes its text. YAML reads a
number such as `1.50` or `0x10` as a number, so it becomes `1.5` or `16`; quote
a value to keep it exactly as written. An empty string is allowed; a list, a map
or an empty value isn't. A name contains ASCII letters, digits and `_`, doesn't
start with a digit, and doesn't start with `MSB_`, which microsandbox reserves
for secret placeholders. `fbk validate` reports a problem with its line and
column:

```sh
$ fbk validate
.firebrick.yml:3:8: error: env.FOO: expected a string, number or boolean
```

**Don't put secrets in `env`.** The agent and every program in the sandbox can
read the values and send them anywhere the network rules allow. Use
[`fbk secret`](/docs/secrets/) for tokens: the sandbox then only sees a
placeholder. A variable can't have the name of a secret the sandbox gets; `fbk
start` then fails with `env variable <NAME> conflicts with secret <NAME>; remove
one of them` and changes nothing. The other way around, `fbk secret set` refuses
a secret whose name a sandbox it would go to uses in `env`.

`fbk start` and `fbk run` apply changes to `env` the next time the sandbox
starts: they set added and changed variables and remove the ones you deleted
from `env`, and leave the image's variables and secret placeholders alone. A
variable with the name of an image variable, such as `PATH`, replaces it, and
deleting it from `env` removes the variable. A running sandbox keeps its
environment, and `fbk` reminds you to restart it:

```sh
$ fbk start
env changes in .firebrick.yml apply after the sandbox restarts: fbk stop && fbk start
```

An SSH connection that starts a stopped sandbox boots it with the variables it
had when it last stopped; run `fbk start` to apply changes to `env`. `fbk
network` and `fbk upgrade` keep the variables. `env` isn't one of the settings
you approve: variables only affect processes in the sandbox and can't give it
more access to the host or the network.

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

Before they create a sandbox with new or changed mounts, `fbk start` and `fbk
run` show them with the directories they resolve to and ask you to approve them.
See [Approving settings changes](#approving-settings-changes).

## mise tools

When the project pins its tools with [mise](https://mise.jdx.dev), Firebrick
installs them when `fbk start` or `fbk run` creates or starts the sandbox, but
not when an SSH connection starts it. It trusts the `mise.toml`, `.mise.toml`,
`mise/config.toml`, `.config/mise.toml` and `.tool-versions` files at the root
of the project and runs `mise install`, so `fbk start` returns with the tools
ready. While mise installs the tools, `fbk start` shows how long it has been
running and the latest line mise wrote:

```sh
$ fbk start
Creating sandbox firebrick...
⠹ Installing mise tools (1m12s): mise node@22.11.0 download node-v22.11.0-linux-x64.tar.gz
```

The line disappears once the tools are installed. When the output isn't a
terminal, for example in CI, `fbk start` prints `Installing mise tools...` and
`Installed mise tools` instead. When `mise install` fails, `fbk start` prints
mise's error and the sandbox keeps running, so you can connect and fix the
config. Images without mise skip this step. Set `mise: false` to turn it off.

## Git identity

Commits the agent makes in the sandbox get the same author as your commits on
the host. Each time `fbk start` or `fbk run` starts the sandbox or finds it
running, `fbk` reads `user.name` and `user.email` the way `git` does in the
project directory, including `includeIf` and a repository-local identity, and
sets them in the global git config of the `agent` user:

```sh
$ git config user.email
ada@example.com
$ fbk start
$ fbk run -- git config --global --get user.email
ada@example.com
```

When you change your identity on the host, the sandbox gets it on the next `fbk`
command. A key you haven't set on the host stays as it is in the sandbox, and so
does the identity when `fbk start <name>` starts the sandbox, because it has no
project directory to read it in. A value with a line break isn't copied, and
`fbk` says so. To commit as someone else in one repository inside the sandbox,
set a repository-local identity there; it wins over the global one:

```sh
fbk run -- git -C /workspaces/my-project config user.email agent@example.com
```

Only the name and email are copied. Your other git settings, such as aliases,
hooks and signing keys, stay on the host, and nothing in the sandbox's git
config is written back to the host. There's no setting to turn this off; images
without git skip this step.

## Sharing the host clipboard

The sandbox has no display, so programs that use the clipboard themselves, such
as Claude Code when you paste an image, or a CLI that copies a URL or a code,
can't reach it. Pasting text into the terminal works without this setting. To
share the host clipboard with the sandbox, both ways, set `clipboard`:

```yaml
name: my-project
clipboard: true
```

**Any process in the sandbox can read and overwrite your clipboard.** While
`clipboard` is on, the agent and every program it runs can read your host
clipboard at any time, without asking, including passwords and tokens you copied
for something else. They can also replace what's on it, so what you paste next
on the host, into a chat, a document or a terminal, may be something the agent
put there: data leaving the sandbox past your network rules, or a command other
than the one you copied. Only turn it on for projects whose agent you'd trust
with what you copy and paste.

`fbk start` asks you to approve the setting before it creates the sandbox, like
mounts (see [Approving settings changes](#approving-settings-changes)). It lists
it as `+ clipboard: shared with the host, both ways`. If you approved
`clipboard: true` before Firebrick could copy to the host clipboard, `fbk` asks
once more, because what the setting allows changed. The setting applies when the
sandbox is created, so recreate an existing one with `fbk rm --force && fbk
start`. In the `firebrick-base` image, `wl-paste` and `xclip -o` then read the
host clipboard's text and PNG images, and `wl-copy` and `xclip` without `-o`
write them:

```console
agent@sandbox:~$ wl-paste --list-types
text/plain;charset=utf-8
image/png
agent@sandbox:~$ wl-paste --no-newline
hello from the host
agent@sandbox:~$ xclip -selection clipboard -t image/png -o > shot.png
agent@sandbox:~$ echo "copied in the sandbox" | wl-copy
agent@sandbox:~$ wl-copy --type image/png < chart.png
agent@sandbox:~$ xclip -selection clipboard -i < notes.txt
```

They support `wl-paste [--no-newline|-n] [--type|-t <type>]`, `wl-paste
--list-types|-l`, `xclip -selection clipboard|c -o [-t <target>]`, `wl-copy
[--type|-t <type>] [text...]` and `xclip -selection clipboard|c [-i] [-t
<target>]`, for text and `image/png`. `wl-copy` copies its arguments, or else
its input. The primary selection, other types and options such as `wl-copy
--clear` don't work. Text is limited to 1 MiB and images to 10 MiB, text you
copy must be UTF-8 and images PNG, and a sandbox can copy at most 10 times
within 10 seconds. A sandbox created before copying was supported can only
paste: `wl-copy` asks you to recreate it.

`fbkd` uses the clipboard with `wl-paste` and `wl-copy` on Wayland, `xclip` on
X11, `pbpaste` and `osascript` on macOS, and Windows PowerShell on Windows, so
install `wl-clipboard` or `xclip` on Linux. It uses the display of the terminal
where you first ran `fbk`. When `fbk` started it from an SSH session without a
display, `wl-paste` in the sandbox prints `the host clipboard isn't available;
fbkd has no display`; stop `fbkd` and run `fbk` from your desktop. On Linux,
`fbkd` needs `XDG_RUNTIME_DIR` set to a private directory for the clipboard;
without it, `fbk start` refuses to create a sandbox with `clipboard: true`. In a
sandbox without the setting, `wl-paste` and `wl-copy` print `clipboard sharing
is off for this sandbox`.

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

| Path                                                                           | Contents                                                                                                                                                                                                                     |
| ------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `$XDG_RUNTIME_DIR/fbkd.sock`                                                   | The socket the CLI and daemon talk over; `fbkd.sock` in the temp directory when `XDG_RUNTIME_DIR` isn't set.                                                                                                                 |
| `$XDG_STATE_HOME/firebrick/` (`~/.local/state/firebrick`)                      | The daemon's logs, one `fbkd.log.<date>` file per day.                                                                                                                                                                       |
| `~/.local/state/firebrick/msb`                                                 | Firebrick's microsandbox home: the runtime, images and sandbox disks. `MSB_HOME` overrides it.                                                                                                                               |
| `~/.local/state/firebrick/network/<sandbox>.json`                              | The domains and IP addresses each sandbox contacted. See [Recording network activity](/docs/networking/#recording-network-activity).                                                                                         |
| `$XDG_RUNTIME_DIR/firebrick/netlog/<sandbox>.sock`                             | The sockets sandboxes report their network activity to. On macOS without `XDG_RUNTIME_DIR`, they're in `/private/tmp/fbk-<uid>`, which must be yours with mode `0700`. On Linux without it, sandboxes get no network record. |
| `$XDG_DATA_HOME/firebrick/ssh` (`~/.local/share/firebrick/ssh`)                | The SSH keys, `known_hosts` and the generated SSH config. See [Editor support](/docs/editor-support/).                                                                                                                       |
| `~/.local/share/firebrick/secrets.yml`                                         | The secrets. See [Secrets](/docs/secrets/).                                                                                                                                                                                  |
| `$XDG_CONFIG_HOME/firebrick/approved.yml` (`~/.config/firebrick/approved.yml`) | The mounts, network and clipboard settings you approved per project. See [Approving settings changes](#approving-settings-changes).                                                                                          |

`fbkd` never falls back to the temp directory for its data, logs and
microsandbox home: it refuses to start when neither `HOME` nor the matching
`XDG_*` variable is an absolute path (or `MSB_HOME` is set, for the microsandbox
home). `fbk` only uses a socket served by your own user or by root.

On Windows, an `XDG_*` variable that is an absolute path still wins, but the
defaults follow Windows instead, and `HOME` doesn't count:

| Path                                        | Contents                                                                                        |
| ------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `\\.\pipe\firebrick-<SID>-fbkd`             | The named pipe the CLI and daemon talk over. Only your user and `LocalSystem` can open it.      |
| `\\.\pipe\firebrick-<SID>-netlog-<sandbox>` | The pipes sandboxes report their network activity to; `-clipboard-<sandbox>` for the clipboard. |
| `%LOCALAPPDATA%\firebrick\state`            | The daemon's logs, the microsandbox home (`msb`) and the network records (`network`).           |
| `%LOCALAPPDATA%\firebrick\data`             | The SSH keys and config (`ssh`) and the secrets (`secrets.yml`).                                |
| `%APPDATA%\firebrick\approved.yml`          | The settings you approved per project.                                                          |

`<SID>` is your user's security identifier, such as `S-1-5-21-...`.

The daemon logs at the `info` level. Set `RUST_LOG` in the environment that
starts `fbkd` to change it, for example `RUST_LOG=debug`. The CLI starts `fbkd`
with its own environment, so stop the daemon and run the next `fbk` command with
the variable set:

```sh
pkill -TERM -x fbkd
RUST_LOG=debug fbk ls
```
