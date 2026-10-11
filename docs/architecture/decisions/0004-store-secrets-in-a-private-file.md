# 4. Store secrets in a private file

## Status

Accepted

## Context

Coding agents in a sandbox need tokens for GitHub, Claude Code and GitHub
Copilot. A token in the guest's environment can be read and leaked by the agent
or by anything it runs, so the real value shouldn't enter the VM.

microsandbox's secrets feature solves this inside the VM. The guest gets a
placeholder such as `$MSB_GH_TOKEN` in the secret's environment variable, and
the TLS proxy on the host replaces the placeholder with the real value in
requests to the secret's allowed hosts. Requests that carry the placeholder to
other hosts are blocked. Each secret needs at least one allowed host.

microsandbox (0.7.6, and 0.7.7 at the time of writing) accepts a secret's value
in two ways:

- **Inline value** - `.value(..)`. microsandbox stores it in plaintext in its
  sandbox database, `~/.microsandbox/db/msb.db`.
- **Source reference** - `.source(SecretSource::Env { var })`. The database
  stores only the reference, and microsandbox resolves it from the environment
  of the process that boots the sandbox, which is `fbkd`. `SecretSource::Store`,
  for a host-side secret store, isn't implemented yet.

We considered two options:

- **A: inline values in private files** - `fbkd` keeps the secrets in
  `$XDG_DATA_HOME/firebrick/secrets.yml` (mode `0600`), passes the values
  inline, and makes microsandbox's `db` directory `0700`.
- **B: OS keyring with an environment source** - `fbkd` keeps the secrets in the
  keyring (Secret Service), loads them into namespaced environment variables
  such as `FIREBRICK_SECRET_GH_TOKEN` before its tokio runtime starts, and
  passes `SecretSource::Env` references. `set_var` is only sound before other
  threads exist, so setting a secret has to restart `fbkd`.

|                                    | A: inline value, private files                                      | B: keyring + environment source                                                    |
| ---------------------------------- | ------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| At rest                            | Plaintext in `secrets.yml` (`0600`) and `msb.db` (`db/` is `0700`). | Encrypted in the keyring. No plaintext on disk.                                    |
| Other users on the host            | Blocked by file permissions.                                        | Blocked.                                                                           |
| Malware running as the user        | Can read both files.                                                | Can query the unlocked keyring, or read `/proc/<pid>/environ` of `fbkd`.           |
| Backups, a stolen unencrypted disk | Exposed.                                                            | Protected.                                                                         |
| Changing a secret                  | `fbkd` updates each sandbox through `modify()`.                     | `fbkd` restarts and drops open `fbk run` and SSH sessions.                         |
| Cost                               | No new dependency.                                                  | The `keyring` crate, a desktop keyring, `unsafe` `set_var` and a restart protocol. |

Both options keep the value out of the VM equally well. They differ only in how
the value is protected on the host, and neither protects against malware that
runs as the user.

## Decision

We use option A. The keyring adds complexity without protecting against the
threat that matters, malware running as the user. With full-disk encryption, the
remaining gain of option B, protection at rest, is small.

- `fbk secret set <name> [<value>] [--from-stdin] [--allow-host <host>]...`
  sends the secret to `fbkd` through the `SetSecret` RPC.
- `fbkd` validates the name (an environment variable name that doesn't start
  with `MSB_`), the value (not empty) and the allowed hosts (host names,
  optionally `*.`-prefixed with at least two labels after it, such as
  `*.example.com`; `*`, `*.com` and IP addresses aren't allowed). It parses them
  with the parser of the network rules, so both accept the same host names.
  Without `--allow-host`, it uses defaults for well-known names:

  | Name                                           | Default allowed hosts                                 |
  | ---------------------------------------------- | ----------------------------------------------------- |
  | `GH_TOKEN`, `GITHUB_TOKEN`                     | `github.com`, `api.github.com`, `uploads.github.com`  |
  | `COPILOT_GITHUB_TOKEN`                         | `github.com`, `api.github.com`, `*.githubcopilot.com` |
  | `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` | `api.anthropic.com`                                   |

- `fbkd` stores the secret in `secrets.yml`, writing a `0600` temporary file and
  renaming it, and adds it to every sandbox it created (the ones with an
  `firebrick.hostname` label) with `modify().secret(..).next_start()`. New
  sandboxes get all stored secrets when they're created. microsandbox enables
  TLS interception for sandboxes with secrets.
- `fbk secret ls` lists the names and allowed hosts, never the values. `fbk
  secret rm` removes a secret from the sandboxes with
  `modify().remove_secret(..).next_start()`, and then from `secrets.yml`. When a
  sandbox fails, the secret stays in `secrets.yml`, so running `fbk secret rm`
  again retries it.
- On startup, `fbkd` sets the microsandbox `db` directory to `0700`.

## Consequences

- The real values never enter the VM. A running sandbox gets a new or changed
  secret the next time it starts. A running sandbox also keeps a removed secret
  until it restarts, because microsandbox can't reconfigure secrets live yet.
- The values are in plaintext at rest, readable by every process that runs as
  the user. **When the host may be compromised, rotate the secrets** at their
  issuers (GitHub, Anthropic) and set the new values with `fbk secret set`.
  Changing them in firebrick alone doesn't help, because the old values may
  already have been copied.
- microsandbox only substitutes placeholders in HTTP headers. Tools that send
  the token in a header (`gh`, the GitHub and Anthropic APIs) work. `git` over
  HTTPS works too: microsandbox decodes Basic auth credentials, replaces the
  placeholder and encodes them again, so a credential helper that returns the
  placeholder as the password is enough.
- Tools that check a token's format before using it may reject the placeholder.
- When microsandbox implements `SecretSource::Store`, we can revisit this
  decision without changing the CLI.
