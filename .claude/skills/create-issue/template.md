# Issue Body Templates

Use the template that matches the issue type. Keep the section names. Replace
every `<...>` placeholder with real content, and remove a section marked
_optional_ when it doesn't apply. Write each paragraph and list item on a single
line: this file is wrapped at 80 columns, but the issue body must not be.

## Bug

````markdown
## Summary

<One sentence: what is wrong, and where.>

## Reproduction

Version: `<git rev-parse --short HEAD>`

```sh
<the smallest command sequence that shows the bug>
```

**Observed:** <the actual output or behavior, quoted exactly.>

**Expected:** <the correct output or behavior.>

## Context

- **Affected code:** <paths and symbols, such as `src/orders/service.py` and
  `OrderService.place_order`.>
- **Suspected cause:** <hypothesis and the evidence for it, or "unknown".>
- **Constraints:** <architecture rules that apply, with a link to the doc.>

## Acceptance criteria

- [ ] A test in `<test file>` reproduces the bug and fails without the fix.
- [ ] <the observable behavior after the fix.>
- [ ] <each related error case.>

## Out of scope

- <adjacent work the implementer must leave alone.>

## Verification

```sh
<the project's format, lint and test commands>
<the integration test command, when the change touches code it covers>
<the reproduction command, now showing the expected behavior>
```

## Notes _(optional)_

<Decisions left to the implementer, links, related issues.>
````

## Feature

````markdown
## Goal

<The problem this solves and for whom, in two or three sentences.>

## Behavior

<The commands, flags, API messages or output that change, with an example of
each.>

```sh
<example invocation and output>
```

## Errors

- <what can go wrong> → <what the user sees.>

## Context

- **Affected code:** <modules, files and symbols to change.>
- **Reuse:** <existing code or dependencies to build on.>
- **Constraints:** <architecture rules that apply, with a link to the doc.>

## Acceptance criteria

- [ ] <each observable behavior from the Behavior section.>
- [ ] <each error case from the Errors section.>
- [ ] Unit tests in `<test file>` cover the public interface.
- [ ] <integration tests in `<test file>`, when the change crosses a process,
      network or storage boundary.>
- [ ] The architecture docs describe the new behavior.
- [ ] <A decision record in `docs/architecture/decisions/`, when this adds a
      dependency or makes an architectural choice.>
- [ ] <The user docs in `website/src/content/docs/docs/` show the new usage,
      when user-facing behavior changes.>

## Out of scope

- <adjacent work the implementer must leave alone.>

## Verification

```sh
<the project's format, lint and test commands>
<the integration test command, when the change touches code it covers>
<a command that demonstrates the feature>
```

## Notes _(optional)_

<Decisions left to the implementer, alternatives considered, links.>
````
