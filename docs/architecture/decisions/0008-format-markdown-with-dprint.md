# 8. Format Markdown with dprint

## Status

Accepted. Since
[ADR 0036](0036-run-each-ci-workflow-only-for-the-changes-it-checks.md), CI runs
it only on pull requests and pushes that change its inputs.

## Context

The README, the architecture docs and the agent skills are Markdown. They were
wrapped by hand, so line widths and table alignment drifted with every edit.
Agents and people edit these files all the time, so the layout needs a
formatter, the same way `rustfmt` decides the layout of the Rust code.

## Decision

`dprint` with its Markdown plugin formats every Markdown file in the repository.
`dprint.json` wraps prose at 80 columns and aligns tables; tables and code
blocks may be wider than 80 columns. Links that don't fit stay on one line.

`mise.toml` pins `dprint` and `dprint.json` pins the plugin version. The Claude
Code hook formats a Markdown file after each edit, lefthook checks every
Markdown file before a commit that touches Markdown, and CI checks them on every
pull request.

We chose `dprint` over Prettier because it's a single binary that `mise`
installs, so contributors and CI don't need Node.js.

## Consequences

- Markdown diffs show rewrapped paragraphs when a sentence changes.
- `dprint` downloads its Markdown plugin from `plugins.dprint.dev` on first use
  and caches it.
- Contributors run `dprint fmt` before committing, or let the hooks do it.
