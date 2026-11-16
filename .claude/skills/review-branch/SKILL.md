---
name: review-branch
description: "Review the branch with the implementation-reviewer passes, the website-reviewer and the test-reviewer agents in parallel, then merge their findings into one summary with the reviewer agent. Use before opening a pull request, from the submit-pr skill, or when the user asks for a full review of the branch. Optional args: the files or commits to review."
---

# Review branch

Run the review agents in parallel and merge their findings into one report with
one verdict.

## Steps

### 1. Pick the scope

Without arguments, review the branch diff: `git diff main...HEAD` plus `git diff
HEAD`. With arguments, review those files or commits instead. Call the sentence
that names the scope `<scope>`, for example "Review the branch diff: `git diff
main...HEAD` plus `git diff HEAD`."

### 2. Run the review passes

Launch these eight agents with the Agent tool, all in one message so they run in
parallel:

| Pass                                | `subagent_type`           | Prompt                                                                                                |
| ----------------------------------- | ------------------------- | ----------------------------------------------------------------------------------------------------- |
| `implementation-reviewer:ownership` | `implementation-reviewer` | Run only the `ownership` category. `<scope>` Return every finding with its category and check number. |
| `implementation-reviewer:safety`    | `implementation-reviewer` | Run only the `safety` category. `<scope>` Return every finding with its category and check number.    |
| `implementation-reviewer:errors`    | `implementation-reviewer` | Run only the `errors` category. `<scope>` Return every finding with its category and check number.    |
| `implementation-reviewer:fallbacks` | `implementation-reviewer` | Run only the `fallbacks` category. `<scope>` Return every finding with its category and check number. |
| `implementation-reviewer:async`     | `implementation-reviewer` | Run only the `async` category. `<scope>` Return every finding with its category and check number.     |
| `implementation-reviewer:ladder`    | `implementation-reviewer` | Run only the `ladder` category. `<scope>` Return every finding with its category and check number.    |
| `website-reviewer`                  | `website-reviewer`        | `<scope>` Use category `website` and your own check numbers for the findings.                         |
| `test-reviewer`                     | `test-reviewer`           | `<scope>` Use category `tests` and your own check numbers for the findings.                           |

Append to every prompt: "For each finding give the file, line, category, check
number, what is wrong, a concrete failure scenario, a suggested fix, and whether
it is uncertain. End with a one-line verdict: ready, ready after the listed
fixes, needs rework, or no Rust changes / no website changes / no test changes."

Wait for every agent's notification. Never predict or fill in the result of a
pass that hasn't reported. A pass that fails or returns no findings list counts
as not reviewed.

### 3. Merge the findings

Launch the `reviewer` agent with a prompt that contains:

- `<scope>`
- "Do your own general review, then merge these findings from the
  implementation-reviewer, website-reviewer and test-reviewer passes with yours.
  Remove duplicates, rank everything from most to least severe, and give one
  overall verdict: ready, ready after the listed fixes, or needs rework."
- When passes are not reviewed: "These passes returned no result and count as
  not reviewed: `<passes>`. Name them under "Not reviewed" in the report. The
  verdict can't be "ready"."
- The findings and verdict of each pass, labeled with the pass name.

### 4. Report

Show the user the reviewer's report and the verdict. A missing pass must never
read as a clean pass: when a pass is not reviewed and the verdict is "ready",
report "ready after the listed fixes" and name the missing passes. When the
`reviewer` agent itself returns no result, report the verdict as "not reviewed"
with the findings per pass.
