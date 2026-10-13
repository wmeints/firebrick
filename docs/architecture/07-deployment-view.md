# Deployment view

## Release artifacts

A release starts with a pull request that bumps the workspace version in
`Cargo.toml` and adds the release notes to `CHANGELOG.md`; the `create-release`
skill prepares it. When it merges, `.github/workflows/tag-release.yaml` tags the
merge commit with the new version, such as `v0.4.0`, and calls
`.github/workflows/release.yaml` with the tag. A push to main that changes
`Cargo.toml` without changing the version releases nothing, because its tag
exists already. Pushing a tag by hand runs `release.yaml` as well.
`release.yaml` publishes:

- A `firebrick-<tag>-<target>.tar.gz` archive per target, with a `.sha256`
  checksum, attached to a GitHub release. The release notes are the tag's
  section of `CHANGELOG.md`, followed by GitHub's generated list of pull
  requests. Each archive holds one directory with the `fbk` and `fbkd` binaries
  side by side, because the CLI starts the daemon from its own directory.
- The `ghcr.io/wmeints/firebrick-base:<tag>` image, built from the `Dockerfile`
  for `linux/amd64` and `linux/arm64`. The image is pushed only after every
  archive has built. GHCR makes a package private when it's first published, so
  make `firebrick-base` public once in its package settings before sandboxes can
  pull it without logging in.
- The five workspace crates on crates.io, at the tag's version, so users can run
  `cargo install firebrick-cli firebrick-daemon`. The `publish` job publishes
  them once the archives and the image succeed, and `cargo publish` orders them
  by their dependencies: `firebrick-proto`, `firebrick-spec`, `firebrick-utils`,
  then `firebrick-cli` and `firebrick-daemon`. It skips the crates whose version
  is on crates.io already, so rerunning the job after a partial publish
  publishes only the rest.

The jobs run in this order: `package` for each target, then `image`, then
`publish`, then `release`, which creates the GitHub release. A failed job stops
the jobs after it, so the GitHub release only appears once crates.io has the
version.

The `image` and `publish` jobs run in the `release` environment, which waits for
a required reviewer to approve each deployment in the Actions UI, under "Review
deployments" on the run. `publish` needs `image`, so a release takes two
approvals: one for `image` once every `package` job succeeds, and one for
`publish` once the image is pushed. A deployment nobody approves within 30 days
fails the run. Re-run its failed jobs with "Re-run failed jobs" in the Actions
UI or `gh run rerun <run-id> --failed`: a release from a version bump runs
inside the `Tag release` run, and a new run of `tag-release.yaml` finds the tag
and releases nothing. Only a tag pushed by hand has a `Release` run. The
`package` and `release` jobs only build and create the GitHub release, so they
run without approval. See
[ADR 0045](decisions/0045-approve-publishing-in-a-release-environment.md).

The `publish` job authenticates with crates.io
[Trusted Publishing](https://crates.io/docs/trusted-publishing): it exchanges
the workflow's OIDC token for a short-lived crates.io token, so the release
needs no stored secret. It needs the `id-token: write` permission, and
`tag-release.yaml` grants it to the job that calls `release.yaml`, because a
called workflow gets no more permissions than its caller. crates.io matches the
workflow file configured for each crate against the token's `workflow_ref`
claim, which names the top-level workflow. A release from a merged version bump
therefore authenticates as `tag-release.yaml`, and a tag pushed by hand as
`release.yaml`. Each crate's Trusted Publishing configuration on crates.io must
list a workflow for a release to publish through it. Both configurations require
the `release` environment, so crates.io only accepts a token minted by a job in
that environment, after a reviewer approved it. A token from any other job fails
to authenticate and publishes nothing.

Tags with a suffix, such as `v0.2.0-rc.1`, publish a pre-release, on GitHub and
on crates.io. `cargo install` skips pre-releases unless asked for one with
`--version`.

| Target                      | Runner             |
| --------------------------- | ------------------ |
| `x86_64-unknown-linux-gnu`  | `ubuntu-22.04`     |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` |
| `aarch64-apple-darwin`      | `macos-latest`     |

The Linux binaries are built on Ubuntu 22.04, so they need glibc 2.35 or newer.

Windows has no release: the CLI and daemon talk over a unix socket and rely on
unix signals. See
[ADR 0001](decisions/0001-release-through-github-releases-and-ghcr.md). Windows
users run the Linux release in WSL 2 with nested virtualization, which exposes
`/dev/kvm` to the distribution.

## Host requirements

`fbkd` runs sandboxes through the microsandbox runtime (`msb` and `libkrunfw`),
which needs KVM on Linux or Apple Silicon on macOS. `fbkd` embeds the runtime
that matches the `microsandbox` crate it's built with (currently `0.7.6`). On
startup it extracts that runtime when none is installed, and replaces an
installed runtime with another version. See
[ADR 0002](decisions/0002-embed-the-microsandbox-runtime-in-anvild.md).

- The runtime, microsandbox's database (`db/msb.db`), its `config.json`, the
  images and the sandboxes live in firebrick's own microsandbox home,
  `$XDG_STATE_HOME/firebrick/msb` (by default `~/.local/state/firebrick/msb`).
  `fbkd` never reads or writes `~/.microsandbox`, so a separately installed
  `msb` keeps its own runtime and database. See
  [ADR 0023](decisions/0023-keep-a-firebrick-microsandbox-home-in-the-xdg-state-directory.md).
- `fbkd` ignores an empty or relative `XDG_STATE_HOME` or `HOME`, as the XDG
  spec requires, and refuses to start when neither is an absolute path and
  `MSB_HOME` isn't set. The home holds the `msb` binary `fbkd` runs, so it must
  not land in the current directory or a shared temp directory. `fbkd` creates
  it with mode `0700`.
- A non-empty `MSB_HOME` environment variable overrides the home. The cargo
  commands in the repository and the `smoke-test` skill use it to isolate
  development builds. `MSB_PATH` and `MSB_LIBKRUNFW_PATH` point `fbkd` at a
  runtime elsewhere.
- Sandboxes that an earlier `fbkd` created in `~/.microsandbox` stay there.
  `fbkd` doesn't move or list them; remove them with `msb` if you want to.
- `fbkd` reads the version from the `msb` binary without running it. A runtime
  without version information predates it and counts as outdated, and so does an
  `msb` in the microsandbox home whose version can't be read.
- A runtime that `MSB_PATH` or `paths.msb` points to is never replaced. When its
  version differs from the embedded one, `fbkd` refuses to start.
- A partial runtime (one of the two files missing) stops `fbkd` from starting.
  Remove both files to let `fbkd` reinstall the runtime.

The macOS binaries aren't signed. When the archive is downloaded through a
browser, macOS quarantines them; remove the quarantine with `xattr -d
com.apple.quarantine fbk fbkd`.

## Website

`website/` builds the product website, a landing page and the user docs, into
static files in `website/dist`. The docs are Markdown pages in
`website/src/content/docs/docs/`, served under `/docs/`. Its `site` setting
points at the custom domain `https://firebrick-sandbox.dev`, which serves the
site at the root, so it has no `base`. The end-to-end tests serve the build at
the root too. See
[ADR 0027](decisions/0027-build-the-website-with-astro-starlight-and-tailwind.md)
and [ADR 0034](decisions/0034-serve-the-website-from-firebrick-sandbox-dev.md).

The landing page at `/` (`website/src/pages/index.astro`) is built from the
sections in `website/src/components/landing/`. The landing page and the docs
share the design tokens and self-hosted fonts in
`website/src/styles/global.css`, and the docs are themed through the Starlight
overrides in `website/src/components/starlight/`. The landing page links to docs
pages through `docsPage` in `website/src/site.ts`, which fails the build for a
page that doesn't exist. See
[ADR 0035](decisions/0035-theme-the-website-with-shared-tokens-and-self-hosted-fonts.md).

Link previews of every page show the social card in
`website/public/social-card.png`, a 1200×630 PNG. The landing page sets its own
Open Graph and Twitter tags; the docs pages get theirs from Starlight, and the
`head` setting in `website/astro.config.mjs` adds the image. Open Graph needs an
absolute image URL, so both build it from the `site` setting.

`.github/workflows/website.yaml` publishes the site to GitHub Pages on pushes to
`main` that change `website/`, `mise.toml`, `mise.lock` or the workflow, and on
manual runs from `main`. Its `check` job uploads `website/dist` as a Pages
artifact once the checks and end-to-end tests pass, and its `deploy` job deploys
it with `actions/deploy-pages`. Pull requests only check the site. The
repository's Pages source must be set to "GitHub Actions" and its custom domain
to `firebrick-sandbox.dev` before the first deployment. See
[ADR 0032](decisions/0032-publish-the-website-on-github-pages.md).

## Continuous integration

Each workflow runs on pull requests and pushes to `main` only when the files it
checks, or the workflow file itself, changed. `workflow_dispatch` runs a
workflow by hand. See
[ADR 0036](decisions/0036-run-each-ci-workflow-only-for-the-changes-it-checks.md).

| Change                                                                                   | Workflow          | Checks                                               |
| ---------------------------------------------------------------------------------------- | ----------------- | ---------------------------------------------------- |
| `crates/**`, `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `clippy.toml`, `mise.*`, `ci.yaml` | `ci.yaml`         | `cargo fmt`, clippy, unit tests, then the `vm-tests` |
| `website/**`, `mise.*`, `website.yaml`                                                   | `website.yaml`    | The website's checks, build and end-to-end tests     |
| `**/*.md`, `dprint.json`, `mise.*`, `markdown.yaml`                                      | `markdown.yaml`   | `dprint check`                                       |
| `.github/workflows/**`, `mise.*`                                                         | `actionlint.yaml` | `actionlint`, with `shellcheck` on the `run` steps   |
| `Dockerfile`, `image.yaml`                                                               | `image.yaml`      | Builds the `firebrick-base` image                    |

`mise.toml` pins the Rust toolchain, Node, pnpm, dprint, actionlint and
shellcheck, and `mise.lock` records their checksums, so `mise.*` stands for both
files and runs every workflow except `image.yaml`. A pull request that changes
only the website or the docs runs no `CI` jobs. `main` has no required status
checks, so a workflow that its filter skipped doesn't block a merge.

The `check` job of `ci.yaml` runs clippy and then the unit tests; there's no
separate build step, because both compile every target. The `integration` job
restores the dependencies the `check` job cached (`Swatinem/rust-cache` with a
shared key) and compiles only `firebrick-daemon` again, for its `vm-tests`
feature.

Both jobs restore the microsandbox runtime in `/tmp/firebrick-msb/bin` and
`/tmp/firebrick-msb/lib` from an `actions/cache` entry keyed on `Cargo.lock`,
and date its files back before the first cargo command. The `microsandbox` build
script installs the runtime into `MSB_HOME` and reruns when the runtime's files
change. Cargo dates a build script run to when the run started, so a runtime the
script installed looks changed to the next cargo command, which recompiles
`microsandbox` and every crate that depends on it. Without the cached runtime,
every step of a job recompiled them.

### Supply chain

The workflows pin every action to a full commit SHA, with the matching release
in a comment, so a moved or hijacked tag doesn't change what they run.
Dependabot proposes new SHAs weekly, seven days after a release at the earliest.
`jdx/mise-action` pins mise to the version that generated `mise.lock` and
installs the tools with `--locked`, so each download must match the checksum in
`mise.lock`; run `mise lock` after changing a version in `mise.toml`. The
release jobs install only `rust` and `protoc`. See
[ADR 0043](decisions/0043-pin-github-actions-and-ci-tools-to-checksums.md).

`.github/workflows/audit.yaml` runs `cargo audit` against `Cargo.lock` with the
latest RustSec advisory database every day, on manual runs, and on pull requests
and pushes to `main` that change the workflow. A vulnerability advisory fails
the job, and GitHub notifies about the failed scheduled run; unmaintained,
unsound and yanked crates only print warnings. A database that can't be fetched
also fails the job. When an advisory doesn't apply to firebrick and no upgrade
fixes it, a maintainer adds its ID to `[advisories] ignore` in
`.cargo/audit.toml`, with a comment that gives the reason. See
[ADR 0046](decisions/0046-audit-dependencies-with-cargo-audit-daily.md).

GitHub enforces the pins: the repository requires actions pinned to a full
commit SHA, so a workflow that references an action by tag fails at startup. `gh
api repos/wmeints/firebrick/actions/permissions --jq .sha_pinning_required`
prints `true` when the setting is on.

The release gate and the pinning are repository settings, outside the code. A
maintainer with admin access applies them in this order:

1. Repository → Settings → Environments: create `release` with `wmeints` as a
   required reviewer, and limit deployment branches and tags to the `main`
   branch and `v*.*.*` tags. This comes before the next release: a job that
   names a missing environment creates it without reviewers and runs without
   approval.
2. crates.io: for each crate (`firebrick-spec`, `firebrick-utils`,
   `firebrick-proto`, `firebrick-cli`, `firebrick-daemon`), set the environment
   `release` on both Trusted Publishing configurations, `tag-release.yaml` and
   `release.yaml`.
3. Repository → Settings → Actions → General: enable "Require actions to be
   pinned to a full-length commit SHA".
4. Repository → Settings → Rules: set the "Protect main branch" ruleset, which
   blocks deleting and force pushing the default branch, to Active.

The environment allows `main` because a release from `tag-release.yaml` runs on
`main`, and `v*.*.*` tags because a tag pushed by hand runs on the tag. It
doesn't require protected tags: whoever can push a tag can merge to `main`, and
each deployment still needs approval. See
[ADR 0045](decisions/0045-approve-publishing-in-a-release-environment.md).
