---
title: Usage scenarios
description: End-to-end walkthroughs for the Azure DevOps MCP server, agents that need your dotfiles, and an IDE connected to a sandbox.
---

The other pages each cover one topic. This page combines them into the setups
people run into most, from an empty project to a working agent. Each step links
to the page with the detail.

All scenarios start in the project directory with a `.firebrick.yml`, which `fbk
init` writes (see [Configuration](/docs/configuration/)), and with the network
policy enforced, so the sandbox only reaches the hosts you allow:

```sh
fbk init
fbk network policy enable
```

## Using the Azure DevOps MCP server

The
[Azure DevOps MCP server](https://learn.microsoft.com/en-us/azure/devops/mcp-server/remote-mcp-server)
gives an agent tools for work items, repositories, pull requests, pipelines and
wikis. It comes in two transports:

| Transport | Runs                                                  | Authentication                   |
| --------- | ----------------------------------------------------- | -------------------------------- |
| `stdio`   | In the sandbox, with `npx`, which needs Node.js 20+   | An Azure DevOps PAT, or Entra ID |
| HTTP      | Hosted by Azure DevOps at `https://mcp.dev.azure.com` | Entra ID (OAuth) in your browser |

Use the local `stdio` server with a PAT when you want the credential to stay out
of the sandbox: it's the only option where it does. With the remote server, the
OAuth token lives on the sandbox's disk, like any agent login.

The examples use Claude Code as the agent and `contoso` as the organization.
Install the agent and allow its hosts first; see
[Installing an agent](/docs/agents/).

### The local server with a PAT

1. Pin Node.js for the project, so `npx` is there in every sandbox, in
   `mise.toml` at the root of the project (see
   [Installing an agent for the project](/docs/agents/#installing-an-agent-for-the-project)):

   ```toml
   [tools]
   node = "22"
   ```

2. Store the PAT as a secret. With `--authentication pat`, the server reads
   `PERSONAL_ACCESS_TOKEN`, expects the base64 encoding of `<email>:<pat>` in
   it, and sends it as is in an `Authorization: Basic` header. Store that
   encoded value, and allow only the Azure DevOps hosts the server sends it to:

   ```sh
   printf '%s' 'me@contoso.com:<pat>' | base64 | tr -d '\n' \
     | fbk secret set PERSONAL_ACCESS_TOKEN --from-stdin \
       --allow-host dev.azure.com --allow-host vssps.dev.azure.com \
       --allow-host almsearch.dev.azure.com --allow-host '*.visualstudio.com'
   ```

   The sandbox only sees the placeholder `$MSB_PERSONAL_ACCESS_TOKEN`; the host
   swaps in the real value on requests to those hosts. See
   [Secrets](/docs/secrets/).

3. Allow the hosts. `npx` downloads the server from npm, and the server talks to
   Azure DevOps. A secret's allowed hosts must be allowed by the network rules
   too:

   ```sh
   fbk network presets apply mise node github
   fbk network allow dev.azure.com vssps.dev.azure.com almsearch.dev.azure.com "*.visualstudio.com"
   ```

   See
   [Presets](/docs/networking/#allowing-coding-agents-and-toolchains-with-presets).

4. Start the sandbox and add the server to the agent:

   ```sh
   fbk start
   fbk run -- claude mcp add ado -- npx -y @azure-devops/mcp contoso --authentication pat
   ```

5. Run the agent with `fbk run claude` and ask it to list your work items. When
   a tool call fails, run `fbk network log` on the host to find the hosts it was
   denied, allow them with `fbk network allow`, and try again. The hosts depend
   on the tools you use and on your organization. See
   [Recording network activity](/docs/networking/#recording-network-activity).

### The remote server with Entra ID

The remote server needs a client that can sign in with Microsoft Entra ID.
Visual Studio Code with GitHub Copilot and the GitHub Copilot CLI can do that
out of the box. Claude Code and Cursor need a custom Entra app registration;
follow Microsoft's steps for
[Claude Code](https://learn.microsoft.com/en-us/azure/devops/mcp-server/remote-mcp-server#claude-code),
which register `http://localhost:3118/callback` as the redirect URI.

1. Allow the MCP endpoint and the Entra sign-in page. Firebrick only opens a
   login page in your browser when the network rules allow its host:

   ```sh
   fbk network allow mcp.dev.azure.com login.microsoftonline.com
   ```

2. Add the server with your app registration's client ID:

   ```sh
   fbk start
   fbk run -- claude mcp add --transport http ado https://mcp.dev.azure.com/contoso \
     --client-id <client-id> --callback-port 3118
   ```

3. Run `fbk run claude`, then `/mcp`, and sign in. The sandbox has no browser,
   so Firebrick opens the sign-in page in the browser on your host and forwards
   `localhost:3118` to the sandbox, where Claude Code waits for the callback.
   See [Browser logins](/docs/networking/#browser-logins).

If the sign-in page doesn't open, check `fbk network log` for the host it
needed, and the `fbkd` log for a login page Firebrick refused to open.

## Using oh-my-pi or another agent that needs your dotfiles

Some agents rely on your own setup: your shell, git config, and the agent's own
settings, such as the providers in oh-my-pi's `~/.omp/agent/models.yml`. Keep
that setup in a dotfiles repository and let Firebrick apply it in every sandbox,
instead of setting it up again after each `fbk rm`.

1. Manage the dotfiles with [mise dotfiles](https://mise.jdx.dev/dotfiles.html):
   a `mise.toml` in the repository maps each file to its place in the home
   directory. Install the agent through a global mise config that the dotfiles
   bring along, so every sandbox gets it:

   ```toml
   # ~/dotfiles/mise.toml
   [dotfiles]
   "~/.gitconfig" = { source = "git/config", mode = "copy" }
   "~/.bashrc" = { source = "bashrc" }
   "~/.config/mise/config.toml" = { source = "mise/global.toml", mode = "copy" }
   "~/.omp/agent/models.yml" = { source = "omp/models.yml", mode = "copy" }
   ```

   ```toml
   # ~/dotfiles/mise/global.toml
   [tools]
   "github:can1357/oh-my-pi" = "latest"
   ```

2. Point `mise.config` at the repository in `.firebrick.yml`:

   ```yaml
   mise:
     config: ~/dotfiles
   ```

3. Allow the hosts mise downloads from, and the agent's model provider:

   ```sh
   fbk network presets apply mise github
   fbk network allow api.example-provider.com
   ```

4. Start the sandbox, approve the dotfiles mount when `fbk start` asks, and run
   the agent:

   ```sh
   fbk start
   fbk run omp
   ```

Firebrick mounts the directory read-only at `/opt/firebrick/dotfiles` and
applies it each time the sandbox starts, before it installs the tools. The agent
can read every file in the directory, so keep keys and tokens out of it and give
the agent its API key with [`fbk secret`](/docs/secrets/) instead. See
[Dotfiles](/docs/configuration/#dotfiles).

Use `mode = "copy"` for files the agent or `fbk` writes to. A file without it is
a symlink into the read-only mount, so writes to it fail. That includes
`~/.gitconfig`: `fbk` writes your git identity to it on every `fbk start`.

### Dotfiles or a mount

Dotfiles suit settings that should be the same in every sandbox and that the
agent shouldn't change. Use a [`mounts`](/docs/configuration/#extra-mounts)
entry instead when the sandbox needs a host directory as it is, such as a shared
library of skills or prompts, or needs to write back to it:

```yaml
mounts:
  - host: ~/omp-skills
    guest: /home/agent/.omp/skills
    readonly: true
```

Leave out `readonly` only for directories you'd let the agent change. Don't
mount your whole home directory or a directory with credentials: the agent can
read everything in a mount. `fbk start` asks you to approve each new mount. See
[Approving settings changes](/docs/configuration/#approving-settings-changes).

### Your git identity

You don't need dotfiles for the commit author. `fbk start` and `fbk run` copy
`user.name` and `user.email` from the host, the way `git` reads them in the
project directory, to the sandbox each time they start it. Your other git
settings, such as aliases and signing keys, stay on the host unless your
dotfiles bring them. See [Git identity](/docs/configuration/#git-identity).

## Using an IDE connected to the sandbox

An IDE with remote development over SSH runs its backend, terminals and
extensions in the sandbox, so its integrated agent, such as GitHub Copilot,
works on the files in the sandbox instead of on your host.

1. Allow the hosts the agent needs, and give the agent its token as a secret
   when it reads one from the environment. For GitHub Copilot:

   ```sh
   fbk network presets apply copilot github
   gh auth token | fbk secret set COPILOT_GITHUB_TOKEN --from-stdin
   ```

   `COPILOT_GITHUB_TOKEN` is allowed on GitHub and `*.githubcopilot.com` by
   default. See [Allowed hosts](/docs/secrets/#allowed-hosts).

2. Start the sandbox. `fbk start` prints the commands that open the workspace:

   ```sh
   $ fbk start
   Connect with: ssh my-app.fbk
   Open in VS Code: code --folder-uri vscode-remote://ssh-remote+my-app.fbk/workspaces/my-app
   Open in Zed: zed ssh://my-app.fbk/workspaces/my-app
   ```

3. Open the workspace at `/workspaces/<leaf>` on the `<leaf>.fbk` host. In VS
   Code and Zed, run the printed command; for Cursor, replace `code` with
   `cursor`. In a JetBrains IDE, create an SSH connection to `my-app.fbk` in
   Gateway or under **File | Remote Development**. See
   [Editor support](/docs/editor-support/).

4. Sign the agent in. The IDE's own sign-in for an extension stores its token in
   the sandbox, where the agent can read it. To keep the token out, run the
   agent's CLI in the IDE's terminal instead, such as the GitHub Copilot CLI
   (`mise use -g copilot`), which uses `COPILOT_GITHUB_TOKEN`.

The IDE downloads its server and extensions into the sandbox the first time it
connects. With the network policy enforced, check `fbk network log` for the
hosts it was denied and allow them. The IDE backend needs memory, so raise
`resources.memory` in `.firebrick.yml` for larger IDEs; see
[Configuration](/docs/configuration/).
