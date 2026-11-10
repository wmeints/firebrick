---
title: Sandbox lifecycle
description: What fbk stop, fbk rm and fbk upgrade each keep and delete, so you can choose between stopping, recreating and upgrading a sandbox.
---

A sandbox keeps its state on its own disks, apart from your project files, which
stay on the host. `fbk stop`, `fbk rm` and `fbk upgrade` each treat that state
differently:

- `fbk stop` shuts the sandbox down and keeps everything for the next `fbk
  start`.
- `fbk rm` deletes the sandbox. The next `fbk start` creates a new one from
  `.firebrick.yml`.
- `fbk upgrade` recreates the sandbox from the latest version of its image. It
  keeps the sandbox's settings but replaces its disks.

| State                                                                   | `fbk stop`                      | `fbk rm`                                                                                                                            | `fbk upgrade`                                                                                                       |
| ----------------------------------------------------------------------- | ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Sandbox disk: packages and files outside the mounted directories        | Kept                            | Deleted                                                                                                                             | Replaced by a fresh root filesystem from the new image                                                              |
| Installed tools and agents                                              | Kept                            | Deleted; unless `mise.enabled` is off, the next `fbk start` installs the tools in the project's mise config and your dotfiles again | Deleted; unless `mise.enabled` is off, the tools in the project's mise config and your dotfiles are installed again |
| Docker images, containers and volumes                                   | Kept                            | Deleted                                                                                                                             | Deleted; the sandbox gets a fresh Docker disk                                                                       |
| [Sandbox-scoped secrets](/docs/secrets/#global-and-sandbox-secrets)     | Kept                            | Deleted                                                                                                                             | Kept                                                                                                                |
| [Network activity record](/docs/networking/#recording-network-activity) | Kept                            | Deleted                                                                                                                             | Kept                                                                                                                |
| [Agent logins](/docs/agents/#logging-in)                                | Kept                            | Deleted                                                                                                                             | Deleted                                                                                                             |
| [Port forwards](/docs/networking/#forwarding-ports)                     | Closed; reopened by `fbk start` | Closed; the `ports` list in `.firebrick.yml` stays and applies to the next sandbox                                                  | Kept; reopened when the sandbox runs again                                                                          |
| [Extra mounts](/docs/configuration/#extra-mounts)                       | Kept                            | Removed with the sandbox; the `mounts` list in `.firebrick.yml` stays and applies to the next sandbox                               | Kept                                                                                                                |
| [Network rules](/docs/networking/#changing-the-rules-with-fbk-network)  | Kept                            | Removed with the sandbox; the rules in `.firebrick.yml` stay and apply to the next sandbox                                          | Kept                                                                                                                |

None of the commands touch your project files, the host directories of extra
mounts or global secrets.

## Choosing a command

- To pause work and pick it up later, run `fbk stop`. The installed tools,
  logins and Docker data are still there after `fbk start`.
- To start over from a clean sandbox, or to apply settings in `.firebrick.yml`
  that only take effect when the sandbox is created (see
  [Applying changes](/docs/configuration/#applying-changes)), run `fbk rm` and
  `fbk start`. Set the sandbox-scoped secrets again afterwards.
- To move to a newer image, run `fbk upgrade`. Log in to the agents again, and
  pin the tools you want back in the project's mise config or your dotfiles (see
  [Installing an agent for the project](/docs/agents/#installing-an-agent-for-the-project)).

See the [command-line reference](/docs/command-line-reference/) for the flags of
[`fbk stop`](/docs/command-line-reference/#fbk-stop),
[`fbk rm`](/docs/command-line-reference/#fbk-rm) and
[`fbk upgrade`](/docs/command-line-reference/#fbk-upgrade).
