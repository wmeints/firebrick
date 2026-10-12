# 40. Compile the network presets into fbk

## Status

Accepted

## Context

With `enforce: true`, a sandbox only reaches the hosts its `.firebrick.yml`
allows. Every user first has to work out which domains their coding agent and
toolchain need, such as Claude Code, Copilot, Codex, GitHub and the package
registries of their language, and type them in with `fbk network allow`. Most
users need the same handful of domains.

Firebrick can ship a list of these domains. Where the list lives, and how it
reaches the sandbox, decide who can change it and what the user must review:

- A file in the config directory, or one read from an environment variable, is
  writable by anything that runs as the user on the host. A change to it would
  silently widen the network of every sandbox that uses it.
- A `presets` setting in `.firebrick.yml` would be expanded by `fbk` or `fbkd`
  each time the sandbox starts. The workspace is mounted into the sandbox, so
  the agent can edit the file, and the rules the sandbox gets would depend on
  the version of `fbk` that expands them. The approval from
  [ADR 0037](0037-approve-mounts-and-network-changes-in-the-spec.md) would have
  to show the expanded rules to be meaningful, and the daemon and spec crate
  would need to know the presets too.

## Decision

The presets are constants in `crates/cli/src/presets.rs`. Each preset is a name
and a list of rule strings, grouped by tool: `claude`, `copilot`, `github`,
`codex`, `node`, `dotnet`, `java`, `rust`, `go`, `python`, `mise` and `ruby`.

`fbk network presets show` prints them. `fbk network presets apply <preset>...`
copies the rules of the named presets into `network.allow` of `.firebrick.yml`,
exactly like `fbk network allow` with those rules, and `all` selects every
preset. The spec records only the rules, not which presets were applied.

## Consequences

- Nobody can change the presets by editing a file on disk; they change only with
  a new `fbk` release, where they are reviewed like any other code. A unit test
  parses every rule, so an invalid constant can't ship.
- The groups let users allow just their agent and toolchain, instead of a single
  list that opens every registry to every sandbox.
- After `apply`, the rules are ordinary `allow` rules. They go through the
  approval prompt like any other network change, the user sees exactly which
  hosts are allowed in `.firebrick.yml`, and can remove them or narrow them with
  `deny` rules. The daemon and `firebrick-spec` need no change.
- A sandbox doesn't pick up changes to a preset in a newer `fbk`: the user
  applies the preset again to add new rules, and removes dropped rules by hand.
  Removing a preset's rules with one command isn't possible either, because the
  spec doesn't record which presets were applied.
- Domain rules only allow HTTP(S) on ports 80 and 443
  ([ADR 0018](0018-enforce-egress-with-microsandboxs-network-policy.md)), so the
  presets don't cover SSH to `github.com:22` or other protocols.
