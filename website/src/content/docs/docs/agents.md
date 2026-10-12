---
title: Installing an agent
description: Install a coding agent in a sandbox with mise, per sandbox or per project, or with another method.
---

The `firebrick-base` image comes without a coding agent, so you install one
yourself. This page shows how to install it with [mise](https://mise.jdx.dev),
the recommended method, and when to use another method instead.

## Why mise

The base image ships mise, activates it in `/home/agent/.bashrc` and puts its
shims on `PATH`. A tool you install with mise therefore works everywhere in the
sandbox: in `fbk run`, in SSH sessions and in the terminals and tasks of your
editor.

mise's registry has short names for the common agent CLIs, such as `claude`
(Claude Code), `codex` (OpenAI Codex), `copilot` (GitHub Copilot CLI), `crush`
and `opencode`. Run `mise registry` in the sandbox to see the full list. Use
these names instead of the npm packages: mise installs npm packages without
running their install scripts, and some agents, such as Claude Code's
`@anthropic-ai/claude-code`, download their native binary in such a script. The
`npm:@anthropic-ai/claude-code` package installs, but `claude` then fails with
`claude native binary not installed`.

## Installing an agent in one sandbox

Open a shell in the sandbox and install the agent with `mise use -g`, for
example Claude Code:

```sh
fbk run bash
mise use -g claude
claude
```

Or OpenAI Codex:

```sh
mise use -g codex
codex
```

`mise use -g` writes the agent to the global mise config of the sandbox, in
`/home/agent/.config/mise/config.toml`, and installs it under `/home/agent`. The
sandbox keeps its disk, so the agent survives `fbk stop` and `fbk start`. `fbk
rm` deletes the disk, and the next sandbox of the project comes up without the
agent.

## Installing an agent for the project

To give every sandbox of the project the agent, also after `fbk rm`, pin it in
the project's mise config. Add it to `mise.toml` at the root of the project:

```toml
[tools]
claude = "latest"
```

`fbk start` and `fbk run` trust the file and run `mise install` in the sandbox
when they create or start it, so the sandbox comes up with the agent ready. See
[mise tools](/docs/configuration/#mise-tools) for the files Firebrick trusts.
mise only runs inside the sandbox, never on your host.

Keep in mind that:

- The file is part of the project, so it applies to everyone working on the
  project and to mise on your host, outside the sandbox. Pin a specific version,
  such as `claude = "2.1.296"`, to give everyone the same one.
- The agent can edit the file, for example to add or remove tools. Review
  changes to it like any other change the agent makes.
- An SSH connection that starts a stopped sandbox doesn't install the tools. Run
  `fbk start` after you change the file.

When `mise install` fails, for example because of a typo in a tool name, `fbk
start` prints mise's error and exits with a non-zero status:

```text
Error: ... mise install failed in sandbox my-project: ...
mise ERROR Failed to install npm:@openai/codexx@latest: package not found: @openai/codexx
```

The sandbox keeps running, so you can connect and fix the config. Correct the
tool name in `mise.toml` and run `fbk start` again, or set `mise: false` in
`.firebrick.yml` to skip the install.

## Giving the agent its API key

Store the agent's API key as a Firebrick secret, so the sandbox never sees the
real key. See [Secrets](/docs/secrets/).

## Installing with the network policy enforced

With `network.enforce: true`, mise can only download from the hosts you allow.
Without them, `mise use` and `mise install` fail with an error that names the
blocked host, and `fbk start` prints that error. Find the hosts with `fbk
network log` after a failed install, and allow them with `fbk network allow`:

```sh
$ fbk network log
┌───────────────────────┬──────┬───────┬───────┬─────────────────────┬────────┐
│ DESTINATION           │ KIND │ PORTS │ COUNT │ LAST SEEN           │ POLICY │
├───────────────────────┼──────┼───────┼───────┼─────────────────────┼────────┤
│ api.github.com        │ dns  │       │ 4     │ 2026-10-12 20:34:00 │ denied │
│ mise-versions.jdx.dev │ dns  │       │ 5     │ 2026-10-12 20:34:00 │ denied │
...
```

Each failed attempt only reaches the first blocked host, so repeat this until
the install succeeds. Installing `claude` and `codex` with mise needs these
hosts:

```sh
fbk network allow mise-versions.jdx.dev api.github.com github.com release-assets.githubusercontent.com
```

Other tools can download from other hosts, so check the log for them. See
[Networking](/docs/networking/#recording-network-activity) for how the log and
the rules work.

## Other methods

mise doesn't fit every agent or every team. Two other methods:

- **The agent's own installer or `npm install -g`.** Run the installer the agent
  documents, or install Node with `mise use -g node@lts` and then the agent with
  `npm install -g <package>`, in a shell in the sandbox. npm runs the package's
  install scripts, so this works for agents that are only published on npm and
  need them. Like `mise use -g`, the agent lives on the sandbox's disk until
  `fbk rm`. Agents that update themselves keep doing so, which needs their
  download hosts when the network policy is enforced.
- **A custom image.** Install the agent in an image built on `firebrick-base`,
  so every sandbox that uses the image starts with it, without downloading
  anything. This suits teams that want the same agent version everywhere, or
  sandboxes that run with a strict network policy. You rebuild the image to
  update the agent. See [Custom images](/docs/custom-images/).
