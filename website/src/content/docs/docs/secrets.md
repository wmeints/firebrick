---
title: Secrets
description: Give agents tokens without letting the real values into the sandbox.
---

Agents need tokens, such as an API key for their model or a GitHub token. With
`fbk secret`, the sandbox gets a placeholder instead of the real value, and the
host swaps in the value only in requests to the hosts you allow. An agent that
leaks its environment leaks the placeholder, which is useless anywhere else.

## Setting a secret

```sh
gh auth token | fbk secret set GH_TOKEN --from-stdin
fbk secret set ANTHROPIC_API_KEY --from-stdin < ~/anthropic-key.txt
fbk secret set MY_TOKEN --from-stdin --allow-host api.example.com
```

Use `--from-stdin` rather than the value as an argument, so the value stays out
of your shell history. The name is the environment variable that holds the
placeholder in the sandbox. It may contain letters, digits and underscores, may
not start with a digit, and may not start with `MSB_`, which microsandbox
reserves for its own variables.

## How the placeholder works

In the sandbox, the environment variable holds a placeholder such as
`$MSB_GH_TOKEN`. When a request to one of the secret's allowed hosts carries the
placeholder in an HTTP header, the host replaces it with the real value.
Requests that carry it to other hosts are blocked.

## Allowed hosts

These names have default allowed hosts:

| Name                                           | Allowed hosts                                         |
| ---------------------------------------------- | ----------------------------------------------------- |
| `GH_TOKEN`, `GITHUB_TOKEN`                     | `github.com`, `api.github.com`, `uploads.github.com`  |
| `COPILOT_GITHUB_TOKEN`                         | `github.com`, `api.github.com`, `*.githubcopilot.com` |
| `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` | `api.anthropic.com`                                   |

Other names need `--allow-host`, which you can repeat. It takes an exact host
name such as `api.example.com`, or `*.example.com` for the subdomains of
`example.com`. `--allow-host` replaces the defaults of a well-known name rather
than adding to them.

When the sandbox's network rules are enforced, a secret's allowed hosts must be
allowed by those rules too. See [Networking](/docs/networking/).

## Global and sandbox secrets

Secrets apply to all sandboxes. To give one project its own value, set the
secret with `--scope sandbox` in the project's directory. It applies to that
project's sandbox only, which must exist, and wins over a global secret with the
same name there:

```sh
$ fbk secret set MY_SECRET --from-stdin --allow-host api.example.com --scope sandbox
Secret MY_SECRET set for sandbox firebrick-d9f287. The sandbox sees the placeholder $MSB_MY_SECRET; it gets it after a restart when it's running.
```

A running sandbox gets a new or changed secret after `fbk stop` and `fbk start`.

## Listing and removing secrets

`fbk secret ls` shows the names, scopes and allowed hosts of the secrets, never
their values. Add `--format json` for JSON:

```text
┌─────────────────────────┬──────────────────┬───────────────────┐
│ NAME                    │ SCOPE            │ ALLOWED HOSTS     │
├─────────────────────────┼──────────────────┼───────────────────┤
│ CLAUDE_CODE_OAUTH_TOKEN │ global           │ api.anthropic.com │
│ MY_SECRET               │ firebrick-d9f287 │ api.example.com   │
│ MY_SECRET               │ other-project    │ api.example.org   │
└─────────────────────────┴──────────────────┴───────────────────┘
```

`fbk secret rm <name>` removes a global secret, and `fbk secret rm <name>
--scope sandbox` the working directory's sandbox secret, after which that
sandbox gets the global secret with the same name again. A running sandbox keeps
using a removed secret until it restarts. `fbk rm` removes the sandbox's own
secrets too. If a token leaked, revoke it where you created it as well.

## Git over HTTPS

Sandboxes that run `firebrick-base`, or an image built on it, push and pull
GitHub repositories over HTTPS with `GH_TOKEN`, or `GITHUB_TOKEN` when
`GH_TOKEN` isn't set, without configuring git. The image's credential helper
hands git the placeholder as the password; git sends it base64-encoded in a
Basic `Authorization` header, and the host decodes it, replaces the placeholder
and encodes it again. This works only when the token's allowed hosts include
`github.com`, as the defaults do. Without either secret, git prompts for
credentials as usual, and a helper in your own `~/.gitconfig`, such as the one
`gh auth setup-git` configures, still works.

The image's helper answers first whenever `GH_TOKEN` or `GITHUB_TOKEN` is set,
even when its allowed hosts don't include `github.com`, for example for a GitHub
Enterprise token. git then sends a placeholder that isn't replaced and stops
looking, so your own helper never answers. Give that token another name, or
reset the helpers for github.com in the sandbox before adding your own:

```sh
git config --global credential.https://github.com.helper ''
git config --global --add credential.https://github.com.helper '!gh auth git-credential'
```

For a custom image that isn't based on `firebrick-base`, configure an equivalent
helper in the sandbox:

```sh
git config --global credential.https://github.com.helper \
  '!f() { test "$1" = get && echo username=x-access-token && echo "password=${GH_TOKEN:-$GITHUB_TOKEN}"; }; f'
```

Don't use SSH keys for git: the sandbox's SSH server doesn't support agent
forwarding (`ssh -A`), and copying a private key into the sandbox puts the real
key where the agent can read it.

## Storage

`fbkd` stores the secrets unencrypted in `~/.local/share/firebrick/secrets.yml`
(or `$XDG_DATA_HOME/firebrick/secrets.yml`), which only your user can read and
write (mode `0600`). The values never leave your machine and never enter the
sandbox. If your machine may be compromised, rotate the tokens where you created
them and set the new values.
