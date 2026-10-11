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
list a workflow for a release to publish through it.

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
[ADR 0001](decisions/0001-release-through-github-releases-and-ghcr.md).

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
`website/src/content/docs/docs/`, served under `/docs/`. Its `site` and `base`
settings point at the repository's GitHub Pages URL,
`https://wmeints.github.io/firebrick/`, and the end-to-end tests serve the build
under that base path. See
[ADR 0027](decisions/0027-build-the-website-with-astro-starlight-and-tailwind.md).

`.github/workflows/website.yaml` publishes the site to GitHub Pages on pushes to
`main` that change `website/`, `mise.toml` or the workflow, and on manual runs
from `main`. Its `check` job uploads `website/dist` as a Pages artifact once the
checks and end-to-end tests pass, and its `deploy` job deploys it with
`actions/deploy-pages`. Pull requests only check the site. The repository's
Pages source must be set to "GitHub Actions" before the first deployment. See
[ADR 0032](decisions/0032-publish-the-website-on-github-pages.md).
