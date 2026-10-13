# 42. Strip escape sequences with console

## Status

Accepted

## Context

`firebrick_utils::strip_control` cleans guest output before it ends up in an
error message, a log line, a mise progress line or `fbk network`. It removed
single control characters, so the parameters of escape sequences stayed in the
text, such as `[1;32m` or `]52;c;...`. Removing whole sequences needs a parser
for CSI, OSC and the other `ESC` sequences. `fbk` already depends on `console`
through `indicatif`, and `console::strip_ansi_codes` is such a parser.

## Decision

`firebrick-utils` depends on `console` with only the `alloc` and `ansi-parsing`
features. `strip_control` rewrites the C1 forms of CSI (`U+009B`) and OSC
(`U+009D`) to their `ESC` forms, removes the sequences with
`console::strip_ansi_codes` and then removes every remaining control character
except newline and tab.

## Consequences

- The project maintains no parser of its own, and the crate is already in
  `Cargo.lock` through `indicatif`, so it adds no new third-party code to `fbk`.
  `fbkd` gets it as a new dependency.
- The final filter still guarantees that no control character other than newline
  and tab remains, whatever `console` does with malformed input.
- `console` drops an unterminated sequence up to the end of the text, newlines
  included, so text after an unclosed OSC doesn't show.
- `console` treats `ESC` followed by digits as the start of a sequence, so `ESC
  7` also removes the letter after it, and it leaves the intermediates and final
  byte of a CSI sequence with an intermediate, such as the ` q` of `ESC [ 2 q`,
  in the text. Both lose a little text or leave a printable remainder, but no
  control characters.
