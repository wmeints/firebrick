---
name: code-scan
description: "Scan the codebase with foxguard and triage every finding: fix the real problems, mark false positives with an inline ignore that gives the reason, and ask the user about the ones in doubt. Use when the user wants to scan, security scan or foxguard the code, or triage foxguard findings."
---

# Code scan

Run [foxguard](https://crates.io/crates/foxguard) over the repository and leave
no finding untriaged: each one is fixed, ignored with a reason, or put to the
user.

## Steps

### 1. Run the scan

- Check that foxguard is installed with `foxguard --version`. When it isn't,
  install it with `cargo install foxguard`.
- Scan the repository from its root and keep the JSON report in the scratchpad
  directory:

  ```sh
  foxguard . -f json --output <scratchpad>/foxguard.json
  ```

  Each entry in `findings` has a `rule_id`, `severity`, `file`, `line`,
  `description` and `snippet`. Run `foxguard . --explain` for the source-to-sink
  trace of a taint finding, such as `rs/no-path-traversal`.
- Group the findings by `rule_id`. Findings of one rule tend to share a verdict,
  but judge each one on its own code.

### 2. Judge each finding

Read the code around the line, and follow the value back to where it comes from;
don't judge from the snippet. Remember what Firebrick protects against: a coding
agent in the sandbox that tries to reach the host, leak secrets or bypass the
network policy. A value is untrusted when the guest, a network peer or a
repository config can choose it. The user's own CLI arguments and environment on
the host are not.

Put each finding in one of three groups:

- **Fix**: you are certain the code is wrong, for example a path from the guest
  that reaches the host file system unchecked, or an `unwrap()` that breaks the
  rules in `CLAUDE.md`.
- **Ignore**: you are certain the finding needs no change, for example a path
  built from a constant, a test helper, or an `unsafe` block that already has a
  `// HUMAN-APPROVED:` comment.
- **Ask**: anything else. When you'd need to guess what the user intends, or the
  fix would change behavior or the architecture, ask.

### 3. Ask about the findings in doubt

Ask about all of them at once with the `AskUserQuestion` tool, before you change
any code, so the user isn't interrupted for each one. For every finding explain
the rule, the file and line, where the value comes from, how it could be
exploited, and the fix you'd make. Offer "Fix" and "Ignore" as the options. Move
each finding to the group the user picks; when they ignore one, use their reason
in the comment.

### 4. Fix the real problems

- Fix a finding by following the `fix-bug` skill: write a test that reproduces
  the problem, then make the smallest change that fixes the cause.
- Don't run `foxguard --fix`; its rewrites don't follow the guidelines in
  `CLAUDE.md`.

### 5. Ignore the false positives

foxguard only accepts an inline ignore that has nothing else after it on the
line: `// foxguard: ignore - <reason>` and `// foxguard: ignore[rule] (reason)`
suppress nothing. Put the reason on its own comment line above the ignore, and
name the rule, so a new kind of finding on the same line still shows up:

```rust
// The pipe name is absolute; only the test name and process id vary.
// foxguard: ignore[rs/no-path-traversal]
return (None, PathBuf::from(pipe));
```

- The ignore applies to the next line that isn't empty or a comment, so other
  comments may sit between it and the code. Keep a `// HUMAN-APPROVED:` comment
  directly above the code, under the ignore:

  ```rust
  // The descriptor stays valid until `LocalFree` in `Drop`.
  // foxguard: ignore[rs/unsafe-block]
  // HUMAN-APPROVED: reads the DACL of a descriptor we own
  let status = unsafe { GetSecurityDescriptorDacl(/* ... */) };
  ```

- Ignore several rules on one line with a comma-separated list, such as `//
  foxguard: ignore[rs/no-path-traversal, rs/no-unwrap-in-lib]`.
- Files that can't keep a comment, such as `Cargo.lock`, which cargo rewrites,
  get an entry under `scan.ignore_rules` in `.foxguard.yml` at the repository
  root instead, with the reason in a YAML comment:

  ```yaml
  scan:
    ignore_rules:
      # rustls and russh need RSA and ring until they offer post-quantum
      # alternatives.
      - path: Cargo.lock
        rules: [manifest/cargo-pq-vulnerable-dep]
  ```

  Suppress a whole file only when every finding of that rule in it has the same
  reason.
- An existing ignore with text after the directive suppresses nothing; rewrite
  it in the form above.

### 6. Verify

- Run `foxguard .` again. It should report only the findings the user chose to
  keep; when one you ignored still shows up, check the comment against the rules
  in step 5.
- Run the checks from the definition of done in `CLAUDE.md` for what you
  changed.

### 7. Report

Tell the user how many findings the scan reported, what you fixed and how, and
what you ignored and why, grouped by rule. Suggest one commit for the fixes,
`fix(<scope>): <description>`, and one for the ignores, `chore: mark foxguard
false positives`.
