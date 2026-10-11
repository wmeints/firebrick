---
title: Introduction
description: What Firebrick is and how it keeps coding agents in a microVM.
---

Firebrick (`fbk`) is an agentic sandbox: it runs coding agents safely inside a
microVM on your own machine. Each project gets its own lightweight VM, built
from an OCI image, with only the project directory shared from the host. You
don't need a cloud account, a commercial license or root permissions.

Firebrick supports two ways of working:

- **Terminal agents** such as [Claude Code](https://claude.ai/code),
  [OpenCode](https://opencode.ai) and [Oh-my-pi](https://omp.sh): run them in
  the sandbox with `fbk run`.
- **IDE-integrated agents** such as GitHub Copilot: connect your editor to the
  sandbox over SSH. See [Editor support](/firebrick/docs/editor-support/).

Firebrick is early in development.

## Why a microVM

A coding agent runs commands you haven't reviewed: it installs packages,
executes build scripts and tests, and follows instructions it finds in files and
web pages. In a container those commands share the host's kernel, so a kernel
exploit or a misconfigured mount reaches your machine. A microVM gives each
sandbox its own kernel behind a hardware virtualization boundary, and is light
enough to run one per project.

Inside the sandbox the agent sees the project directory, and nothing else from
your home directory: no SSH keys, no cloud credentials, no browser profile.
Tokens the agent needs reach it as placeholders that only work for the hosts you
allow (see [Secrets](/firebrick/docs/secrets/)), and you can limit where the
sandbox may connect to (see [Networking](/firebrick/docs/networking/)).

## How it works

Firebrick consists of two executables:

- `fbk` - the CLI you use to manage sandboxes and run commands in them.
- `fbkd` - a daemon that manages the sandboxes through
  [microsandbox](https://docs.microsandbox.dev). The CLI starts it automatically
  when it isn't running.

The CLI and daemon talk gRPC over a unix socket (`$XDG_RUNTIME_DIR/fbkd.sock`,
or `fbkd.sock` in the temp directory when `XDG_RUNTIME_DIR` isn't set). The
daemon mounts the working directory read/write in the sandbox at
`/workspaces/<leaf>`, where `<leaf>` is the name of the directory.

`fbkd` embeds the microsandbox runtime and installs it in its own microsandbox
home, `~/.local/state/firebrick/msb` (or `$XDG_STATE_HOME/firebrick/msb`), on
first start. It never touches `~/.microsandbox`, so a separately installed `msb`
keeps its own runtime and database. Set `MSB_HOME` to use another directory.
Sandboxes created by firebrick 0.3.0 and earlier stay in `~/.microsandbox`;
remove them with `msb` if you no longer need them.

## Supported platforms

- Linux with KVM, on x86_64 or ARM64. The release binaries need glibc 2.35 or
  newer.
- macOS on Apple Silicon.

Windows isn't supported.

## Next steps

- [Install Firebrick](/firebrick/docs/installation/).
- Follow the [Quickstart](/firebrick/docs/quickstart/) to run an agent in a
  sandbox.
