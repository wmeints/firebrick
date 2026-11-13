---
name: fix-bug
description: "Fix a bug through root cause analysis and a reproducing test. Use when the user reports a bug, a crash, a failing command, an unexpected error, or wrong behavior and wants it fixed."
---

# Fix bug

Fix the cause of a bug, not its symptom, and leave a test behind that fails
without the fix.

## Steps

### 1. Reproduce the bug

- When the bug comes from a GitHub issue, read it with one command:

  ```sh
  gh issue view <N> --json title,body,labels,comments
  ```

  Plain `gh issue view <N> --comments` prints only the comments. When the issue
  has the `design-ready` label, its body is the agreed description of the
  expected behavior.
- Restate the observed and the expected behavior in one sentence each.
- Reproduce the bug with the smallest command or input you can find. Build the
  project first with the build task described in `CLAUDE.md`.
- When the project has a `smoke-test` skill, use it to reproduce bugs in the
  running application, so the reproduction doesn't touch the user's own
  environment.
- If you can't reproduce it, stop and ask the user for the missing details. Do
  not fix a bug you haven't seen.

### 2. Find the root cause

- Trace the failure from the symptom back to the code that makes the wrong
  decision. Read the code; don't guess from names.
- Ask "why" until the answer is a line of code or a wrong assumption, not a side
  effect of one.
- Check the architecture docs in `docs/architecture` for the structure of the
  project and any known pitfalls.
- Write down the root cause in one or two sentences. If several causes are
  plausible, rule them out one by one with evidence.

### 3. Write a failing test

- Add a test that reproduces the bug through the public interface of the
  affected module, next to the existing tests. Follow the test conventions
  already used in the project.
- Test against the real dependency where that is feasible, such as a file in a
  `tempfile` directory, a real socket, or a VM in a `vm-tests` integration test,
  rather than a mock. Put the test where the "Testing" section of `CLAUDE.md`
  says it belongs.
- Run it and confirm it fails for the reason you found in step 2.

### 4. Fix the root cause

- Make the smallest change that fixes the cause. Follow the coding guidelines in
  `CLAUDE.md`.
- Look for the same mistake elsewhere in the code and fix it there as well.
- When the fix adds a dependency or makes an architectural choice, add a
  decision record to `docs/architecture/decisions/` and list it in
  `docs/architecture/09-decisions.md`. Number it after the newest record on
  `origin/main` and on your branch, so it doesn't collide with one merged in the
  meantime:

  ```sh
  git fetch origin
  git ls-tree --name-only origin/main docs/architecture/decisions/
  ```

### 5. Verify

- Run the new test and confirm it passes.
- Run the format, lint, and test tasks described in `CLAUDE.md`. Run the
  integration tests as well when the fix touches code that they cover.
- Repeat the reproduction from step 1 and confirm the bug is gone.

### 6. Report

Tell the user the root cause, the fix, the test that covers it, and any related
spots you changed. Suggest a commit message of the form `fix(<scope>):
<description>`, ending with `Closes #<N>` when the bug comes from an issue.
