#!/usr/bin/env bash
# Stop hook: verifies the definition of done before Claude ends its turn.
# Runs only the checks for the uncommitted changes:
# - Rust sources or build files: format, clippy and the unit tests, plus the
#   vm-tests when a crate other than the CLI, the proto or the Cargo files
#   changed.
# - The website: install, format, lint, type check, unit tests, build and the
#   end-to-end tests.
# - Markdown files: dprint; GitHub workflows: actionlint.
# Exit code 2 blocks the stop and feeds the failing output back to Claude. A
# stop that this hook already blocked once isn't blocked again: a failure Claude
# couldn't fix is reported to the user instead, so the hook can't loop.
set -uo pipefail

active=$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("stop_hook_active", False))' 2>/dev/null)

cd "${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel)}" || exit 0

changed=$(git status --porcelain --untracked-files=all)
[ -z "$changed" ] && exit 0

# Prints the changed paths that match the extended regex in $1.
changed_matching() {
  grep -E "$1" <<<"$changed" || true
}

# Prints stdin as a message to the user. Exit 0 afterwards so the stop isn't
# blocked; `report` runs in a pipeline, so it can't exit the script itself.
report() {
  python3 -c 'import json, sys; print(json.dumps({"systemMessage": sys.stdin.read()}))'
}

run_check() {
  local name=$1
  shift
  local output
  output=$("$@" 2>&1) && return
  if [ "$active" = True ]; then
    {
      echo "Stop hook: '$name' still fails ($*) after one retry; not blocking again."
      echo "$output" | tail -n 40
    } | report
    exit 0
  fi
  {
    echo "Stop hook: '$name' failed ($*). Fix it before finishing the turn."
    echo "$output" | tail -n 80
  } >&2
  exit 2
}

# Runs pnpm in website/ with the Node and pnpm versions pinned in mise.toml.
website() {
  if command -v mise >/dev/null; then
    mise exec -- pnpm --dir website "$@"
  else
    pnpm --dir website "$@"
  fi
}

check_rust() {
  local rust
  rust=$(changed_matching '\.(rs|proto)$|Cargo\.(toml|lock)$|\.cargo/config\.toml$')
  [ -z "$rust" ] && return
  run_check format cargo fmt --all --check
  run_check clippy cargo clippy --workspace --all-targets --all-features -- -D warnings
  run_check unit-tests cargo test --workspace
  if grep -qE ' crates/[^/]+/' <<<"$(grep -vE ' crates/(cli|proto)/' <<<"$rust")" ||
    grep -qE 'Cargo\.(toml|lock)$|\.cargo/config\.toml$' <<<"$rust"; then
    run_check integration-tests integration_tests
  fi
}

# Runs the vm-tests. On Windows a script runs the test binaries, because cargo's
# job object keeps microsandbox from starting VMs (ADR 0050).
integration_tests() {
  if [ "${OS:-}" = Windows_NT ]; then
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/vm-tests.ps1
  else
    cargo test -p firebrick-daemon --features vm-tests
  fi
}

check_website() {
  [ -z "$(changed_matching ' website/|mise\.(toml|lock)$')" ] && return
  run_check website-install website install --frozen-lockfile
  run_check website-format website run format:check
  run_check website-lint website run lint
  run_check website-typecheck website run check
  run_check website-unit-tests website run test
  run_check website-build website run build
  run_check website-browser website exec playwright install chromium
  run_check website-e2e-tests website run test:e2e
}

check_rust
check_website
[ -n "$(changed_matching '\.md$')" ] && run_check markdown dprint check
[ -n "$(changed_matching ' \.github/workflows/')" ] && run_check workflows actionlint

exit 0
