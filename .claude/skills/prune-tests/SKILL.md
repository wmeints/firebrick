---
name: prune-tests
description: "Analyze the existing tests of a crate, module or test file and propose the ones to delete, merge, move or fix, so the suite stays fast, readable and resilient to refactoring without losing protection against regressions. Use when the user wants to prune, trim, clean up or reduce the tests, speed up the test suite, or deal with flaky or redundant tests."
---

# Prune tests

More tests don't mean better quality. Every test costs time on each run, review
effort on each change, and rework on each refactoring. Keep the tests that
protect against regressions and remove the ones that only add overhead. This
skill removes and reshapes tests; it doesn't add new ones. Tests belong to new
behavior (`implement-feature`) and to bugs (`fix-bug`).

## What a test has to earn

Judge every test against the four pillars of a valuable test: it protects
against regressions, survives refactoring, gives fast feedback and is easy to
maintain.

Functional tests at the seam of a module, its public interface, carry most of
the value: the gRPC tests in `crates/daemon/tests/grpc.rs`, the `vm-tests` and
the tests of a deep module's public functions. They describe the behavior and
survive changes inside the module.

A unit test of the internals of a module earns its place only when it covers one
of these:

1. **Logic with many combinations**, such as parsers and policy matching, whose
   paths are too many to cover at the seam.
2. **Invariants that must always hold**, such as the states of a state machine.
3. **Risky edge cases that are hard to reach** from the seam, such as input the
   guest controls, path handling on the host and the network policy: anything
   that would let the agent reach the host, leak secrets or bypass the policy.
4. **Fast feedback** on code the agent edits often, where the functional test
   needs a VM or a running daemon.
5. **A regression**: the minimal reproduction of a fixed bug. Once a functional
   test covers the same behavior, the reproduction may go.

## Steps

### 1. Agree on the scope

- Take the crate, module or test file the user names. When they name none, count
  the tests per crate and per file and propose the largest or slowest one; don't
  prune the whole workspace in one pass.

  ```sh
  grep -rcE '#\[(tokio::)?test' crates --include=*.rs | grep -v ':0$' | sort -t: -k2 -nr
  ```

- The website's tests in `website/tests/unit/` and `website/tests/e2e/` follow
  the same rules; the mutation check in step 2 only applies to Rust.

### 2. Measure the baseline

- Record how long the scope's tests take. Build first, so the time is the time
  of the tests:

  ```sh
  cargo test -p <crate> --no-run
  time cargo test -p <crate> <filter>
  ```

  For the `vm-tests`, time `cargo test -p firebrick-daemon --features vm-tests
  --test <name>` per test file.
- Find flaky tests: run the scope's tests five times in a row, and search the
  history with `git log --oneline -i --grep=flak --grep=intermittent
  --grep=retry -- <paths>`.
- Run [cargo-mutants](https://mutants.rs/) on the production files the scope's
  tests cover, so step 6 can show that the pruned suite still catches the same
  mutants. Install it with `cargo install --locked cargo-mutants` when `cargo
  mutants --version` fails. Mutation testing is slow, so name each file:

  ```sh
  cargo mutants -p <crate> --file <path> --output <scratchpad>/mutants-before
  ```

  The missed mutants are in
  `<scratchpad>/mutants-before/mutants.out/missed.txt`. Skip this step when the
  scope only has `vm-tests` or website tests, and say so in the report.

### 3. Judge each test

Read each test and the code it calls; don't judge from the name. For each test,
find out what behavior it pins down, whether another test already pins it down,
and which of the five reasons above it serves. Use `git log --follow -p` on the
test file to see why a test was added: a test from a `fix(...)` commit is a
regression test.

Give each test one verdict:

- **Keep**: it covers behavior no other test covers, or serves one of the five
  reasons above.
- **Delete**: it adds overhead without protection. Typical cases:
  - Another test covers the same behavior, usually a functional test at the seam
    that makes a unit test of the internals redundant.
  - It can't fail: the expected value comes from the code under test, or the
    assertion passes for wrong results, such as `assert!(result.is_ok())`.
  - It tests an implementation detail, such as a private helper, the order of
    calls on a fake or the exact text of a log line, and breaks on refactoring
    without the behavior changing.
  - It is a regression test whose bug a functional test now covers.
  - It tests the standard library, a dependency or the compiler, such as a
    derived `Clone` or serde round-tripping a plain struct.
- **Merge**: several tests run the same steps with different inputs. Turn them
  into one table-driven test with a case per row, naming each case in the
  assertion message.
- **Move**: the test is at the wrong level. A slow functional test of an edge
  case belongs in a unit test of the module that decides it; a unit test that
  only exercises the public interface through the seam may belong next to the
  other seam tests.
- **Fix**: the test is flaky or slow for a reason in the test itself, such as a
  fixed `sleep`, a shared port or socket name, or a VM booted for a check that
  doesn't need one. Fix it, or delete it when the behavior is covered elsewhere.
  A flaky test is never kept as it is.

When removing a test would leave behavior uncovered and no reason above applies,
keep it. When you find behavior that no test covers, report it; don't write the
test here.

### 4. Propose the changes

Present the proposal before you change anything, grouped by verdict, as a table
per file:

| Test                    | Verdict | Reason                              | Covered by                   |
| ----------------------- | ------- | ----------------------------------- | ---------------------------- |
| `parses_empty_policy`   | Delete  | Seam test covers the same input     | `grpc::rejects_empty_policy` |
| `parses_ipv4`, `..ipv6` | Merge   | Same steps, different addresses     | -                            |
| `waits_for_ready`       | Fix     | Sleeps 2 s; poll the socket instead | -                            |

Leave out the tests you keep, but give their count. Ask the user to approve the
proposal, or to pick the rows to apply, with the `AskUserQuestion` tool. Apply
only what they approve.

### 5. Apply the approved changes

- Delete, merge, move and fix the tests as approved. Follow the "Testing"
  section of `CLAUDE.md` for where each test lives.
- Remove the test helpers, fixtures and dev-dependencies that no test uses any
  more. Leave the production code alone; when it has items that only the deleted
  tests used, report them instead.
- When the Architecture Docs or a skill name a test you removed, update them.

### 6. Verify

- Run the checks from the definition of done in `CLAUDE.md`. Run the `vm-tests`
  when you changed them.
- Run cargo-mutants again on the same files, with `--output
  <scratchpad>/mutants-after`, and compare `missed.txt` with the baseline. A
  mutant that only the pruned suite misses means a removed test was protecting
  something: restore that test, or the part of a merged test that caught it.
- Time the scope's tests again.

### 7. Report

Tell the user how many tests the scope had and has now, the time before and
after, the flaky tests you fixed or removed, and the result of the mutation
comparison. List the uncovered behavior and the production items only the
deleted tests used, as candidates for follow-up issues. Suggest a commit message
of the form `test(<scope>): <description>`.
