#!/usr/bin/env bash
# PostToolUse hook: formats a website file with Prettier after Claude edits it.
# Uses the settings in website/.prettierrc.json; Markdown is left to dprint.
# Formatting failures (usually syntax errors) are reported back to Claude with
# exit code 2.
set -uo pipefail

file=$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))')
[[ -f "$file" && "$file" != *.md ]] || exit 0

cd "${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel)}" || exit 0

[[ "$(realpath "$file")" == "$PWD"/website/* ]] || exit 0
# Without node_modules there's no Prettier to run; `pnpm --dir website install`
# installs it.
[ -d website/node_modules ] || exit 0

if ! output=$(mise exec -- pnpm --dir website exec prettier --write --ignore-unknown -- "$(realpath "$file")" 2>&1); then
  {
    echo "Format hook: prettier failed for $file."
    echo "$output" | tail -n 40
  } >&2
  exit 2
fi
