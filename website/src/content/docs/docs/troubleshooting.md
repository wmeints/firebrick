---
title: Troubleshooting
description: Fixes for the failures people hit most often when they install and run Firebrick.
---

Find the error you see below. Each entry names the symptom, the fix, and the
page that has the detail.

## KVM is missing or not usable

On Linux, or in a WSL distribution, `ls -l /dev/kvm` prints `No such file or
directory`, or `test -r /dev/kvm && test -w /dev/kvm && echo "KVM is usable"`
doesn't print `KVM is usable`.

- **`/dev/kvm` is missing**: enable hardware virtualization in the firmware, or
  load the KVM module for your CPU with `sudo modprobe kvm_intel` or `sudo
  modprobe kvm_amd`. On WSL, enable nested virtualization.
- **`/dev/kvm` exists, but KVM isn't usable**: add your user to the `kvm` group
  with `sudo usermod -aG kvm "$USER"`, then log out and in again. On WSL, run
  `wsl --shutdown` from PowerShell instead and open the distribution again.

See [System requirements](/docs/installation/#system-requirements) and
[Windows: run Firebrick in WSL 2](/docs/installation/#windows-run-firebrick-in-wsl-2).

## glibc is older than 2.35

The release binaries need glibc 2.35 or newer. On an older distribution, `fbk`
and `fbkd` don't start, and the dynamic loader prints lines like these, with the
versions your build needs:

```text
fbk: /lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.33' not found (required by fbk)
```

Check your version with `ldd --version | head -1`. Use a distribution with glibc
2.35 or newer, such as Ubuntu 22.04, or build the binaries on the machine
instead; see [Installing with Cargo](/docs/installation/#installing-with-cargo)
and [Building from source](/docs/installation/#building-from-source).

## mise install fails in the sandbox

`fbk start` or `fbk run` prints mise's error and exits with a non-zero status:

```text
Error: ... mise install failed in sandbox my-project: ...
mise ERROR Failed to install npm:@openai/codexx@latest: package not found: @openai/codexx
```

The sandbox keeps running, so you can connect and fix the config. There are two
common causes:

- **A wrong tool name**, such as the typo above. Correct the name in `mise.toml`
  and run `fbk start` again, or set `mise.enabled: false` in `.firebrick.yml` to
  skip the install.
- **A host the network policy blocks.** With `network.enforce: true`, mise can
  only download from the hosts you allow, and the error names the blocked host.
  Find the hosts with `fbk network log`, allow them with `fbk network allow`,
  and run `fbk start` again. Each failed attempt only reaches the first blocked
  host, so repeat this until the install succeeds.

See
[Installing an agent for the project](/docs/agents/#installing-an-agent-for-the-project)
and
[Installing with the network policy enforced](/docs/agents/#installing-with-the-network-policy-enforced).

## A host port is already in use

`fbk start` prints a warning, and the sandbox still starts without that forward:

```text
warning: couldn't forward localhost:3000: <reason>
```

`fbk port forward` fails with `couldn't forward localhost:<port>: <reason>` and
changes nothing. Free the port and run `fbk start` again.

See [Forwarding ports](/docs/networking/#forwarding-ports).

## SSH sessions start in /bin/sh and SHELL isn't set

SSH sessions run in `/bin/sh`, and clients that require `SHELL`, such as the
Codex desktop app, can't work. A sandbox created by Firebrick 0.9 or earlier
runs SSH sessions this way. Recreate it:

```sh
fbk rm --force
fbk start
```

See [SSH host names](/docs/editor-support/#ssh-host-names).

## The daemon doesn't restart after an upgrade

After you upgrade from Firebrick 0.9.0 or earlier, an `fbk` command prints:

```text
the firebrick daemon is older than fbk <version> and can't be restarted by fbk; stop the fbkd process and try again
```

Stop the old daemon once. Sandboxes keep running, and the next `fbk` command
starts the new daemon:

```sh
pkill -TERM -x fbkd
```

On Windows, run `Stop-Process -Name fbkd` instead. When the restarted daemon
still has another version, the `fbkd` next to `fbk`, or on your `PATH`, isn't
the one you installed; `fbk` names the binary it started, so you can replace it.

See
[Upgrading and uninstalling](/docs/installation/#upgrading-and-uninstalling).

## A paused sandbox can't be started or upgraded

`fbk` refuses to act on a paused sandbox:

```text
sandbox my-project is paused; try again once it has stopped
sandbox my-project is paused; resume or stop it before upgrading
```

There's no resume command. Stop the sandbox with `fbk stop`, then run `fbk
start` or `fbk upgrade` again.

See [`fbk upgrade`](/docs/command-line-reference/#fbk-upgrade).
