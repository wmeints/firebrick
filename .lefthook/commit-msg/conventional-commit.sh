#!/usr/bin/env bash
# Enforces https://www.conventionalcommits.org: `type(scope)!: description`.
# A script file rather than an inline `run`, because lefthook on Windows passes
# a multi-line `run` to `sh` as one argument, which arrives cut off.
set -u

pattern='^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([a-z0-9._/-]+\))?!?: .+'
header=$(head -n1 "$1")
if ! printf '%s' "$header" | grep -Eq "$pattern" && ! printf '%s' "$header" | grep -Eq '^(Merge|Revert|fixup!|squash!) '; then
  echo "Commit message does not follow Conventional Commits:"
  echo "  $header"
  echo "Expected: <type>(<scope>)?!?: <description>"
  echo "Types: build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test"
  exit 1
fi
