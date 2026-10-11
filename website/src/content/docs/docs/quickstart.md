---
title: Quickstart
description: Go from the installed binaries to an agent running in a sandbox.
---

This page takes you from the installed `fbk` and `fbkd` binaries to a coding
agent running in a sandbox. Install them first; see
[Installation](/firebrick/docs/installation/).

## 1. Start a sandbox

Run `fbk start` in your project directory:

```sh
cd ~/projects/my-project
fbk start
```

The first start pulls the base image, so it takes a while. When the sandbox
runs, `fbk start` prints how to connect to it:

```text
Connect with: ssh my-project.fbk
Open in VS Code: code --folder-uri vscode-remote://ssh-remote+my-project.fbk/workspaces/my-project
Open in Zed: zed ssh://my-project.fbk/workspaces/my-project
```

The project directory is mounted in the sandbox at `/workspaces/my-project`.
Without a `.firebrick.yml`, the sandbox uses the defaults; see
[Configuration](/firebrick/docs/configuration/) to change them.

## 2. Give the agent its API key

Store the key as a secret, reading it from stdin so it stays out of your shell
history:

```sh
fbk secret set ANTHROPIC_API_KEY --from-stdin < ~/anthropic-key.txt
```

The sandbox never sees the real key: its `ANTHROPIC_API_KEY` holds a
placeholder, which the host replaces with the key only in requests to
`api.anthropic.com`. A running sandbox gets a new secret after a restart:

```sh
fbk stop
fbk start
```

See [Secrets](/firebrick/docs/secrets/) for other tokens and hosts.

## 3. Run the agent

Open a shell in the sandbox:

```sh
fbk run bash
```

The base image comes with [mise](https://mise.jdx.dev), so you can install an
agent CLI in the sandbox, for example Claude Code with Node:

```sh
mise use -g node@lts npm:@anthropic-ai/claude-code
claude
```

The agent works on the files in `/workspaces/my-project`, which are your project
files on the host. The sandbox keeps running after you exit the shell.

## 4. Connect over SSH

You can also connect with SSH, or point your editor at the same host name:

```sh
ssh my-project.fbk
```

See [Editor support](/firebrick/docs/editor-support/) for VS Code, Zed and
JetBrains.

## 5. Stop and remove the sandbox

Stop the sandbox when you're done. It keeps its disk, so the installed tools are
still there the next time you run `fbk start`:

```sh
fbk stop
```

Remove the sandbox to delete its disk. Your project files stay on the host:

```sh
fbk rm
```
