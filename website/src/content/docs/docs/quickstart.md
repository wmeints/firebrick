---
title: Quickstart
description: Go from the installed binaries to an agent running in a sandbox.
---

This page takes you from the installed `fbk` and `fbkd` binaries to a coding
agent running in a sandbox. Install them first; see
[Installation](/docs/installation/).

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
[Configuration](/docs/configuration/) to change them.

## 2. Run the agent

Open a shell in the sandbox:

```sh
fbk run bash
```

The base image comes with [mise](https://mise.jdx.dev), so you can install an
agent CLI in the sandbox, for example Claude Code:

```sh
mise use -g claude
claude
```

See [Installing an agent](/docs/agents/) for other agents, installing the agent
for the whole project, and the hosts the install needs when the network policy
is enforced.

The agent works on the files in `/workspaces/my-project`, which are your project
files on the host. The sandbox keeps running after you exit the shell.

## 3. Log in

The first time you start Claude Code, it asks you to log in. Pick your account
type and Claude Code opens the login page. The sandbox has no browser, so
Firebrick opens the page in the browser on your host and forwards the login's
callback back into the sandbox. Log in there, and Claude Code is ready to go.

The login stays on the sandbox's disk, so you don't log in again after `fbk
stop` and `fbk start`, only after `fbk rm` or `fbk upgrade`.

The agent's token lives in the sandbox, where the agent and anything it runs can
read it. To keep it out of the sandbox, store an API key as a Firebrick secret
instead: the sandbox then gets a placeholder that the host replaces with the key
only in requests to the API. See [Secrets](/docs/secrets/).

## 4. Connect over SSH

You can also connect with SSH, or point your editor at the same host name:

```sh
ssh my-project.fbk
```

See [Editor support](/docs/editor-support/) for VS Code, Zed and JetBrains.

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
