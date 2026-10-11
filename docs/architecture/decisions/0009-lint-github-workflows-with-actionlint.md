# 9. Lint GitHub workflows with actionlint

## Status

Accepted. Since
[ADR 0036](0036-run-each-ci-workflow-only-for-the-changes-it-checks.md), CI runs
it only on pull requests and pushes that change its inputs.

## Context

The CI and release workflows are YAML files that only run on GitHub. A typo in
an expression, an unknown input or a shell error in a `run` step shows up only
after a push, and for the release workflow only when a tag is pushed. Agents
already ran `actionlint` by hand on workflow changes.

## Decision

`actionlint` checks every file in `.github/workflows/`. `mise.toml` pins its
version, lefthook runs it before a commit that changes a workflow, and CI runs
it on every pull request.

## Consequences

- Syntax, expression and type errors in workflows fail before they're merged.
- `actionlint` doesn't know what a workflow is meant to do, so logic errors,
  such as downloading the wrong artifacts, still need a review.
- `actionlint` runs `shellcheck` on `run` steps only when `shellcheck` is
  installed, so local results can differ from CI.
