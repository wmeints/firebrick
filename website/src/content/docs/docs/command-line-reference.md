---
title: Command-line reference
description: Every fbk command with its arguments, flags and defaults.
---

Firebrick has two binaries. You only run `fbk`. It starts the daemon, `fbkd`, on
demand from its own directory when it isn't running, so keep both in the same
directory. `fbkd` takes no arguments.

Run the commands from your project directory: Firebrick derives the sandbox from
the working directory and its `.firebrick.yml`. `start`, `stop` and `rm` also
take a sandbox name from `fbk ls` to manage another sandbox from any directory.

`fbk --help` lists the commands, and `fbk <command> --help` describes one. `fbk
--version` prints the installed version.

## `fbk start`

```sh
fbk start [NAME]
```

Starts the sandbox for the working directory, and creates it first when it
doesn't exist. Prints how to connect to it with SSH, VS Code and Zed, and the
ports it forwards.

| Argument | Description                                                                        | Default                                |
| -------- | ---------------------------------------------------------------------------------- | -------------------------------------- |
| `NAME`   | Name of an existing sandbox as listed by `fbk ls`, to start it from any directory. | The sandbox for the working directory. |

```sh
fbk start
fbk start firebrick-d9f287
```

## `fbk stop`

```sh
fbk stop [NAME]
```

Stops a running sandbox. Files on its disk are kept. A sandbox that doesn't shut
down within 30 seconds is killed.

| Argument | Description                                | Default                                |
| -------- | ------------------------------------------ | -------------------------------------- |
| `NAME`   | Name of the sandbox as listed by `fbk ls`. | The sandbox for the working directory. |

```sh
fbk stop
```

## `fbk ls`

```sh
fbk ls [--format <FORMAT>]
```

Lists all sandboxes with their name, status and SSH host name.

| Flag                | Description                                    | Default |
| ------------------- | ---------------------------------------------- | ------- |
| `--format <FORMAT>` | Output format: `table`, or `json` for scripts. | `table` |

```sh
$ fbk ls --format json
[
  {
    "name": "firebrick-d9f287",
    "status": "running",
    "hostname": "my-project.fbk"
  }
]
```

## `fbk rm`

```sh
fbk rm [--force] [NAME]
```

Removes a sandbox with its disks and its sandbox-scoped secrets. Your project
files stay on the host. Refuses to remove a running sandbox unless you pass
`--force`.

| Argument / flag | Description                                | Default                                |
| --------------- | ------------------------------------------ | -------------------------------------- |
| `NAME`          | Name of the sandbox as listed by `fbk ls`. | The sandbox for the working directory. |
| `--force`       | Stop the sandbox first when it's running.  | Off                                    |

```sh
fbk rm --force
```

## `fbk run`

```sh
fbk run <COMMAND> [ARGS]...
```

Runs a command in the working directory's sandbox with your terminal attached,
and exits with the command's exit code. Starts the sandbox first when needed,
and creates it when it doesn't exist. The sandbox keeps running after the
command exits.

| Argument  | Description                                                                        |
| --------- | ---------------------------------------------------------------------------------- |
| `COMMAND` | Command to run in the sandbox.                                                     |
| `ARGS`    | Arguments for the command. Flags after `COMMAND` are passed on, not read by `fbk`. |

```sh
fbk run bash
fbk run -- curl -sI https://github.com
```

## `fbk validate`

```sh
fbk validate
```

Checks the `.firebrick.yml` file in the working directory without starting the
daemon. Prints `.firebrick.yml is valid`, or each problem with its line and
column and exits with code 1. See
[Configuration](/firebrick/docs/configuration/).

## `fbk init`

```sh
fbk init [--force]
```

Writes a `.firebrick.yml` with the default settings to the working directory,
named after the directory.

| Flag      | Description                             | Default |
| --------- | --------------------------------------- | ------- |
| `--force` | Overwrite an existing `.firebrick.yml`. | Off     |

```sh
fbk init
```

## `fbk secret`

Manages the secrets sandboxes use without seeing their values. See
[Secrets](/firebrick/docs/secrets/).

### `fbk secret set`

```sh
fbk secret set <NAME> [VALUE] [--from-stdin] [--allow-host <HOST>]... [--scope <SCOPE>]
```

Sets a secret for all sandboxes, or for the working directory's sandbox only.

| Argument / flag       | Description                                                                                                       | Default                               |
| --------------------- | ----------------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| `NAME`                | Environment variable that exposes the secret in sandboxes, such as `GH_TOKEN`.                                    | Required                              |
| `VALUE`               | Value of the secret. Prefer `--from-stdin` to keep it out of your shell history.                                  | Required unless `--from-stdin` is set |
| `--from-stdin`        | Read the value from stdin.                                                                                        | Off                                   |
| `--allow-host <HOST>` | Host that may receive the value, such as `api.example.com` or `*.example.com`. Repeat it for more hosts.          | The default hosts of well-known names |
| `--scope <SCOPE>`     | `global` for all sandboxes, or `sandbox` for the working directory's sandbox, where it overrides a global secret. | `global`                              |

```sh
gh auth token | fbk secret set GH_TOKEN --from-stdin
fbk secret set MY_TOKEN --from-stdin --allow-host api.example.com --scope sandbox
```

### `fbk secret ls`

```sh
fbk secret ls [--format <FORMAT>]
```

Lists the secrets of all scopes with their allowed hosts, without their values.

| Flag                | Description                       | Default |
| ------------------- | --------------------------------- | ------- |
| `--format <FORMAT>` | Output format: `table` or `json`. | `table` |

### `fbk secret rm`

```sh
fbk secret rm <NAME> [--scope <SCOPE>]
```

Removes a secret from all sandboxes, or from the working directory's sandbox
only.

| Argument / flag   | Description                                                                     | Default  |
| ----------------- | ------------------------------------------------------------------------------- | -------- |
| `NAME`            | Name of the secret, such as `GH_TOKEN`.                                         | Required |
| `--scope <SCOPE>` | `global` to remove the global secret, or `sandbox` for the working directory's. | `global` |

```sh
fbk secret rm GH_TOKEN
```

## `fbk network`

Changes the network settings in `.firebrick.yml` and applies them to the working
directory's sandbox. See [Networking](/firebrick/docs/networking/).

| Command                       | Description                                                                                |
| ----------------------------- | ------------------------------------------------------------------------------------------ |
| `fbk network allow <RULE>...` | Allow the sandbox to connect to the destinations, and stop denying them.                   |
| `fbk network deny <RULE>...`  | Deny the sandbox to connect to the destinations, and stop allowing them.                   |
| `fbk network policy enable`   | Deny outgoing traffic unless a rule allows it (`network.enforce: true`).                   |
| `fbk network policy disable`  | Allow all outgoing traffic; the rules are kept but not enforced.                           |
| `fbk network disable`         | Remove the sandbox's network device, so it works fully offline (`network.enabled: false`). |
| `fbk network enable`          | Give the sandbox a network device again (`network.enabled: true`).                         |

A `RULE` is a host name, `*.` plus a domain, an IP address or a CIDR range, such
as `example.org`, `*.npmjs.org` or `10.0.0.0/8`. `allow` and `deny` need at
least one rule.

```sh
fbk network allow example.org "*.npmjs.org"
fbk network policy enable
```

## `fbk port`

Forwards host ports to the working directory's sandbox and records them in the
`ports` list of `.firebrick.yml`. See
[Networking](/firebrick/docs/networking/#forwarding-ports).

| Command                   | Description                                                                                                                                                                        |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fbk port forward <PORT>` | Forward `localhost:<PORT>` to the same port in the sandbox, or `<HOST>:<GUEST>` to forward `localhost:<HOST>` to sandbox port `<GUEST>`. Applies right away when the sandbox runs. |
| `fbk port rm <PORT>`      | Stop forwarding the host port `<PORT>`, from 1 to 65535.                                                                                                                           |

```sh
fbk port forward 8080:5173
fbk port rm 8080
```

## `fbk ssh-proxy`

A hidden command that the generated SSH config uses as the `ProxyCommand` of
each `.fbk` host. You don't run it yourself. See
[Editor support](/firebrick/docs/editor-support/).
