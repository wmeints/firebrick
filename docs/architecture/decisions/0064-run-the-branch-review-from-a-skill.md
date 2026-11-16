# 64. Run the branch review from a skill

## Status

Accepted

## Context

The branch review ran as a Claude Code workflow,
`.claude/workflows/review-branch.js`. The script started the six
`implementation-reviewer` passes, the `website-reviewer` and the `test-reviewer`
agents in parallel with a JSON schema for their findings, and handed the results
to the `reviewer` agent for one summary. The `submit-pr` skill ran it with the
Workflow tool.

Workflows are a Claude Code feature. Other coding agents, such as Codex or
oh-my-pi, read skills from a `SKILL.md` file and run subagents, but they can't
run a workflow script. Contributors who use those agents couldn't run the review
that `submit-pr` requires.

## Decision

Replace the workflow with a `review-branch` skill in
`.claude/skills/review-branch/SKILL.md`. The skill describes the same steps in
Markdown: pick the scope, launch the eight review passes in parallel, hand their
findings to the `reviewer` agent, and report one verdict. The `submit-pr` skill
runs it with the Skill tool, and any agent that loads skills can follow it.

The skill keeps the workflow's rule that a missing pass never reads as a clean
pass: a pass without a result is named under "Not reviewed", and the verdict
can't be "ready".

## Consequences

- Any coding agent that supports skills and subagents can run the branch review
  and `submit-pr`, so the development environment doesn't depend on Claude Code.
- No schema enforces the shape of the findings. The skill asks each pass for the
  same fields in its prompt, and the `reviewer` agent merges what comes back, so
  a pass may return findings in a looser format.
- The agent that runs the skill orchestrates the passes itself, so the steps can
  drift when an agent skips or merges them. The workflow ran them the same way
  every time.
- [ADR 0029](0029-check-the-website-with-eslint-prettier-vitest-and-playwright.md)
  still names the `review-branch` workflow; the `website-reviewer` now joins the
  `review-branch` skill instead.
