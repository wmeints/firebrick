# Instructions for working on Firebrick

## Purpose of this project

This project builds a sandbox for coding agents. The goal is to keep the agent
safe inside a microVM-based sandbox.

## Technology stack

- Use [Microsandbox](https://docs.microsandbox.dev/getting-started/introduction)
  as the basis for the sandbox.
- Use [Clap](https://docs.rs/clap/latest/clap/) for the CLI implementation.
- Use [Tokio](https://tokio.rs/) for the async runtime
- Use [Tonic](https://github.com/grpc/grpc-rust) for the gRPC implementation.
- Use [Serde_Yaml](https://docs.rs/serde_yaml/latest/serde_yaml/) for parsing
  the yaml configuration.
- Use [Tracing](https://github.com/tokio-rs/tracing) for OpenTelemtry tracing
  data.
- Build the website in `website/` with [Astro](https://docs.astro.build/),
  [Starlight](https://starlight.astro.build/) for the docs and
  [Tailwind CSS](https://tailwindcss.com/) for styling. Use
  [pnpm](https://pnpm.io/) to manage its dependencies, never `npm` or
  `corepack`.

## Important commands

- `cargo build` - compiles all crates into the required executables.
- `cargo test --workspace` - runs the unit tests.
- `cargo test -p firebrick-daemon --features vm-tests` - runs the integration
  tests that boot real microsandbox VMs.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - runs
  the linter, including the `vm-tests` integration tests.
- `cargo fmt --all` - formats the code; `cargo fmt --all --check` verifies it.
- `dprint fmt` - formats the Markdown files to 80 columns with aligned tables;
  `dprint check` verifies them.

For the website, run these from the repository root:

- `pnpm --dir website install --frozen-lockfile` - installs the dependencies.
  Dependencies may not run install scripts unless `allowBuilds` in
  `website/pnpm-workspace.yaml` approves them.
- `pnpm --dir website run dev` - previews the site on
  `http://localhost:4321/firebrick/`.
- `pnpm --dir website run build` - builds the static site into `website/dist`
  and fails on broken links in the docs.
- `pnpm --dir website run format` - formats the code with Prettier;
  `format:check` verifies it. dprint formats the Markdown.
- `pnpm --dir website run lint` - runs ESLint, including accessibility rules for
  `.astro` files.
- `pnpm --dir website run check` - type-checks the `.astro` and TypeScript files
  with `astro check`.
- `pnpm --dir website run test` - runs the Vitest unit tests.
- `pnpm --dir website run test:e2e` - runs the Playwright end-to-end tests
  against the built site; run `build` first, and `pnpm --dir website exec
  playwright install chromium` once.

`.cargo/config.toml` defines shortcuts for the Rust commands: `cargo lint`,
`cargo unit-tests`, `cargo integration-tests`, and `cargo install-cli` / `cargo
install-daemon` to install the binaries into `~/.cargo/bin`.

## Coding guidelines

Prefer deep modules with narrow interfaces for structuring the code. Each module
should have tests focusing the public interface.

Before implementing anything make sure you understand the problem. Perform a
root cause analysis for bugs and ensure you have a thorough spec for new
features.

Follow the implementation ladder to prevent over-engineering:

1. Does it have to be built. No? Don't do it.
2. Does it already exist in the codebase? Reuse it.
3. Does the standard library do it? Use it.
4. Does a project dependency provide it? Use the dependency.
5. Can this be done with one line? Write the one-liner.
6. Only then, implement the minimum amount of logic required.

Clippy runs with `-D warnings`, so every warning fails the build. Write code
that fits from the start:

- Keep functions small and shallow. Split logic into small functions early
  instead of nesting `if`/`match` blocks deeply.
- Let `rustfmt` decide the layout; don't format code by hand.
- Document every public item and every crate with `///` and `//!` comments.
- Use `thiserror` for error types in library code and `anyhow` in binaries.
  Propagate errors with `?` and add context instead of discarding the source.
- Don't `unwrap()` or `expect()` outside tests unless the invariant is
  documented next to the call. Bound integer conversions with `try_from` instead
  of `as`.
- Don't suppress lints with `#[allow(...)]` or `#[expect(...)]`; fix the code.
  If a finding is a false positive, ask the user, and add the attribute only
  with a `// HUMAN-APPROVED: <reason>` comment directly above it. A long reason
  may wrap onto more `//` lines, as long as the comment ends on the line
  directly above the code.
- Don't write `unsafe` code without the same `// HUMAN-APPROVED: <reason>`
  comment directly above it.

The website follows the same rules where they apply:

- ESLint and `astro check` fail on warnings. Don't suppress them with
  `eslint-disable`, `@ts-ignore`, `@ts-expect-error` or `prettier-ignore`
  without a `// HUMAN-APPROVED: <reason>` comment directly above (`<!--
  HUMAN-APPROVED: <reason> -->` in HTML markup).
- The site is static and served under the `base` path in `astro.config.mjs`.
  Build links and asset paths from `import.meta.env.BASE_URL`, never as
  hand-written root-relative paths.
- Make no third-party requests: self-host fonts and assets.
- Ship no client JavaScript where HTML and CSS do the job.
- Customize Starlight through `customCss` and component overrides; don't copy
  its source.

## Testing

- Unit tests live next to the code in a `#[cfg(test)] mod tests` block at the
  bottom of the module and test its public interface.
- Tests that boot real microsandbox VMs live in the crate's `tests/` directory
  and are registered in `Cargo.toml` as a `[[test]]` with `required-features =
  ["vm-tests"]`, so `cargo test` skips them by default.
- Use `#[tokio::test]` for async tests, and give temporary files, sockets and
  sandboxes unique names (for example with `tempfile` or the process id) so
  tests can run in parallel.
- Fix a bug by first writing a test that reproduces it.
- The website's unit tests live in `website/tests/unit/` and render components
  with Astro's container API. Its end-to-end tests live in `website/tests/e2e/`
  and drive the built site with Playwright under the GitHub Pages base path;
  they check that every internal link and asset resolves and that no page
  scrolls horizontally at 375px.

## Definition of done

A change is done when:

1. `cargo fmt --all --check`, `dprint check`, `cargo clippy --workspace
   --all-targets --all-features -- -D warnings` and `cargo test --workspace`
   pass.
2. When `website/` changed, its `format:check`, `lint`, `check`, `test`, `build`
   and `test:e2e` scripts pass.
3. `cargo test -p firebrick-daemon --features vm-tests` passes when a crate
   other than `crates/cli`, `crates/proto`, a `Cargo.toml`, `Cargo.lock` or
   `.cargo/config.toml` changed.
4. The [Architecture Docs](docs/architecture/01-introduction-and-goals.md)
   describe the new behavior, and a decision record exists in
   `docs/architecture/decisions/` for new dependencies or architectural choices.

## Workflows

- Use the `create-issue` skill to file work on GitHub as an issue an agent can
  implement without further questions.
- Use the `fix-bug` skill for bugs: reproduce, find the root cause, write a
  failing test, then fix.
- Use the `smoke-test` skill to run `fbk` and `fbkd` against real VMs, for
  example to reproduce a bug or try a change by hand. It isolates the daemon,
  microsandbox state and SSH config from the user's own.
- Use the `implement-feature` skill for new behavior: agree on a spec first,
  then test, implement and document.
- Use the `sandbox-reference` skill before relying on how the `microsandbox`
  crate behaves. It shows where to find the source of the locked version and
  lists the behavior and limits we already know.
- Use the `create-release` skill to release a new version. It opens a version
  bump PR with the release notes in `CHANGELOG.md` and merges it once the checks
  pass; `.github/workflows/tag-release.yaml` then tags the version and calls the
  release workflow.
- Use the `submit-pr` skill to open a PR. It runs the checks and the
  `review-branch` workflow (`.claude/workflows/review-branch.js`), so don't run
  the review agents before each commit. The workflow runs the
  `implementation-reviewer` agent once per category, the `website-reviewer`
  agent and the `test-reviewer` agent in parallel, and the `reviewer` agent
  merges their findings with its own review into one summary.

## Automated checks

These checks enforce the rules above, so don't try to bypass them:

- `mise install` provides the Rust toolchain, Node, pnpm, `buf`, `protoc`,
  `dprint`, `actionlint`, `lefthook`, the GitHub CLI (`gh`) and Claude Code.
- `.cargo/config.toml` sets `MSB_HOME=/tmp/firebrick-msb` for every cargo
  command, so builds and the `vm-tests` use their own microsandbox runtime,
  database and images instead of `~/.microsandbox`. A `msb` that migrated the
  user's database can't break the tests. Concurrent test runs, for example from
  two worktrees, can share the home: each run names its sandboxes
  `fbk-it-<pid>-<test>` and first removes the leftovers of killed runs. Don't
  point the tests at `~/.microsandbox`; if the isolated home is broken, remove
  `/tmp/firebrick-msb` and run the tests again.
- Claude Code hooks in `.claude/settings.json` run `cargo fmt` on Rust files
  (`.claude/hooks/format-rust.sh`), `dprint fmt` on Markdown files
  (`.claude/hooks/format-markdown.sh`) and Prettier on the other website files
  (`.claude/hooks/format-website.sh`) after each edit. Before a turn ends with
  uncommitted changes, `.claude/hooks/stop-checks.sh` runs the checks for what
  changed: for Rust, proto or Cargo changes the format check, clippy and the
  unit tests, plus the `vm-tests` when the files from step 3 of the definition
  of done changed; for changes in `website/` or `mise.toml` the website's
  install, format check, lint, type check, unit tests, build and end-to-end
  tests; `dprint check` for Markdown and `actionlint` for workflows. A failure
  blocks the turn once; when it still fails on the retry, the hook reports it to
  the user and lets the turn end instead of looping.
- Lefthook runs the format checks for Rust and Markdown, `actionlint` on the
  GitHub workflows, clippy and the unit tests before each commit, and for
  commits that touch `website/` or `mise.toml` the website's install, format
  check, lint, type check, unit tests and build.
- GitHub Actions (`.github/workflows/ci.yaml`) runs the format checks,
  `actionlint`, build, clippy, unit tests and the `vm-tests` integration tests
  on every pull request and every push to `main`.
- `.github/workflows/website.yaml` runs the website's install, format check,
  lint, type check, unit tests, build and end-to-end tests on pull requests and
  pushes to `main` that change `website/`, `mise.toml` or the workflow. On
  pushes to `main` and manual runs from `main` it also deploys the checked build
  to GitHub Pages, so merging a website change publishes it (ADR 0032).
- `.github/workflows/image.yaml` builds the `firebrick-base` image for both
  platforms, without pushing it, on pull requests and pushes to `main` that
  change the `Dockerfile`.
