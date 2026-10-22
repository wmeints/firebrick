---
title: Editor support
description: Connect VS Code, Cursor, Zed or a JetBrains IDE to a sandbox over SSH.
---

Every sandbox is reachable over SSH under a host name ending in `.fbk`. Editors
with remote development over SSH can open the workspace in the sandbox, so the
editor's language servers, terminals and agents run in the sandbox instead of on
your host.

## SSH host names

`fbkd` generates an SSH key pair and an SSH config in
`~/.local/share/firebrick/ssh` (or `$XDG_DATA_HOME/firebrick/ssh`), and adds an
`Include` line for that config to the top of `~/.ssh/config`. It rewrites the
generated config whenever sandboxes are added or removed; don't edit it, because
your changes are overwritten.

Each sandbox gets a host name after its project directory: the directory's name
in lowercase, with every run of other characters than letters and digits
replaced by `-`, followed by `.fbk`. A project in `~/projects/My.App` gets
`my-app.fbk`. When two projects share a directory name, the second gets
`my-app-2.fbk`, the third `my-app-3.fbk`, and so on. `fbk ls` lists the host
name of each sandbox.

```sh
ssh my-app.fbk
```

The host entries log in as the `agent` user with Firebrick's own key and
`known_hosts` file, and run `fbk ssh-proxy` as their `ProxyCommand`. That hidden
command tunnels the connection through the daemon's socket to the SSH server in
the sandbox, so no SSH port is published on your host. Connecting to a stopped
sandbox starts it.

SSH sessions run in the login shell of the `agent` user, with `SHELL` set to it,
as with `sshd`. Clients such as the Codex desktop app require `SHELL`. A sandbox
created by Firebrick 0.9 or earlier runs SSH sessions in `/bin/sh` without
`SHELL`; recreate it with `fbk rm --force` and `fbk start` to fix that.

The sandbox's SSH server doesn't support agent forwarding (`ssh -A`), so your
SSH keys stay on the host. Use git over HTTPS with a secret instead; see
[Git over HTTPS](/docs/secrets/#git-over-https).

## VS Code, VS Code Insiders, Cursor and VSCodium

These editors connect with the
[Remote - SSH](https://code.visualstudio.com/docs/remote/ssh) extension, which
reads the hosts from `~/.ssh/config`. Remote - SSH asks for the platform of each
new host, so `fbkd` maps every `.fbk` host to `linux` in the
`remote.SSH.remotePlatform` setting of each of these editors that's installed.
It edits the editor's user `settings.json` and leaves your other settings,
comments and formatting as they are:

| Platform | Settings file                                               |
| -------- | ----------------------------------------------------------- |
| Linux    | `~/.config/<editor>/User/settings.json`                     |
| macOS    | `~/Library/Application Support/<editor>/User/settings.json` |

`<editor>` is `Code`, `Code - Insiders`, `Cursor` or `VSCodium`. On Linux,
`$XDG_CONFIG_HOME` replaces `~/.config` when it's set. `fbkd` skips an editor
whose `User` directory doesn't exist.

`fbk start` prints a command that opens the workspace in the sandbox:

```sh
$ fbk start
Connect with: ssh my-app.fbk
Open in VS Code: code --folder-uri vscode-remote://ssh-remote+my-app.fbk/workspaces/my-app
Open in Zed: zed ssh://my-app.fbk/workspaces/my-app
```

Run the printed `code` command, or pick the host in Remote - SSH and open
`/workspaces/my-app`. For Cursor, VS Code Insiders or VSCodium, replace `code`
with `cursor`, `code-insiders` or `codium`.

## Zed

Zed's remote development uses the system `ssh`, so the `.fbk` hosts work as they
are. `fbkd` adds one entry per sandbox, with its workspace as the project, to
`ssh_connections` in Zed's `settings.json`, so you can pick the sandbox in Zed's
Remote Projects dialog. It owns the entries whose `host` ends in `.fbk` and
leaves the rest of the file as you wrote it.

Zed's settings live in `~/.config/zed/settings.json`; on Linux,
`$XDG_CONFIG_HOME` replaces `~/.config` when it's set. `fbkd` does nothing when
the `zed` directory doesn't exist.

Run the `zed` command that `fbk start` prints:

```sh
zed ssh://my-app.fbk/workspaces/my-app
```

## JetBrains IDEs

JetBrains Gateway and the Remote Development feature of IntelliJ IDEA, PyCharm
and the other JetBrains IDEs connect over SSH, but Firebrick doesn't configure
them. Set up the connection by hand:

1. Start the sandbox with `fbk start`.
2. In Gateway, or under **File | Remote Development** in the IDE, choose **SSH**
   and create a connection to the host `my-app.fbk`. Use your SSH config
   (OpenSSH config and authentication agent), so the connection goes through the
   `ProxyCommand` that `fbk` sets up; the user is `agent`.
3. Pick the IDE to install in the sandbox, and open the project at
   `/workspaces/my-app`.

The IDE backend runs in the sandbox, so give the sandbox enough memory in
`resources.memory`; see [Configuration](/docs/configuration/).
