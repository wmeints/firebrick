# 64. Publish to winget from the release workflow

## Status

Accepted

## Context

Installing firebrick natively on Windows meant downloading the release `.zip`,
verifying its checksum, copying `fbk.exe` and `fbkd.exe` into
`%LOCALAPPDATA%\Programs\firebrick` and adding that directory to the `PATH` by
hand. Windows users expect to run `winget install`, and to upgrade and uninstall
the same way (#277).
[ADR 0063](0063-publish-a-homebrew-formula-in-a-custom-tap.md) covers macOS and
Linux with Homebrew.

winget installs packages from the community repository `microsoft/winget-pkgs`,
where each version is a set of YAML manifests added by a pull request that the
winget maintainers and their bots validate and merge. A manifest can point at an
MSI, an MSIX, an executable installer, or a `.zip` with nested portable files.

## Decision

The release workflow submits the manifests of the package `wmeints.Firebrick`,
with the moniker `firebrick`, to `microsoft/winget-pkgs`:

- **The community repository.** It's the source every winget install searches by
  default, so `winget install firebrick` works without adding a source. Hosting
  a private REST source would need a server and an extra `winget source add`.
- **A portable zip installer, not an MSI.** The manifests point at the existing
  Windows `.zip` from [ADR 0053](0053-release-a-windows-zip-archive.md), pinned
  to its checksum, as a `zip` installer with `fbk.exe` and `fbkd.exe` as nested
  `portable` files, each with a command alias. winget extracts them and puts
  both commands on the `PATH`, and `fbk` finds `fbkd` next to it or on the
  `PATH`. An MSI would be a second Windows package to build, test and, to avoid
  SmartScreen warnings, sign, for no gain over what winget already does with the
  archive.
- **The moniker `firebrick`.** Users type `winget install firebrick` instead of
  the full identifier, as long as no other package matches `firebrick`
  ambiguously.
- **A pinned `wingetcreate`, not a third-party action.** The `winget` job
  downloads `wingetcreate.exe` from a `microsoft/winget-create` release and
  checks it against a sha256 in the workflow before running it
  ([ADR 0043](0043-pin-github-actions-and-ci-tools-to-checksums.md)), instead of
  the unversioned `aka.ms/wingetcreate/latest` link or a community action that
  would see the token. `wingetcreate` is Microsoft's own submission tool. The
  manifests come from `.github/scripts/winget-manifests.sh`, so the maintainer
  can render and validate them locally and use them for the first submission.
- **A token pushing from the release workflow.** The `winget` job runs after the
  `release` job, in the `release` environment, so a reviewer approves each
  submission ([ADR 0045](0045-approve-publishing-in-a-release-environment.md)).
  It authenticates with `WINGET_TOKEN`, a classic token with `public_repo`:
  `wingetcreate` forks `microsoft/winget-pkgs` into the token owner's account
  and opens the pull request from the fork, which a fine-grained token scoped to
  one repository can't do. Only the submit step gets it, and the job's
  `GITHUB_TOKEN` stays at `contents: read`. The job skips pre-release tags and
  submits nothing when the version is in `microsoft/winget-pkgs` already.

## Consequences

- Windows 11 x64 users install, upgrade and uninstall firebrick with winget. On
  ARM64 winget finds no applicable installer, because there's no ARM64 build.
- A release takes a fourth approval in the `release` environment, for the
  `winget` job.
- A new version reaches `winget upgrade` only once the winget maintainers merge
  the pull request, some time after the release.
- `WINGET_TOKEN` can write to every public repository of its owner. It lives
  only in the `release` environment and reaches only the submit step, and the
  maintainer must renew it before it expires; an expired token fails the
  `winget` job, and rerunning it with a new token submits the manifests.
- The maintainer submits the first version by hand and checks with `winget
  search firebrick` that the moniker resolves.
- Windows can't replace or remove a running `fbkd`, so users stop the daemon
  before `winget upgrade` or `winget uninstall`; winget doesn't stop it.
- The manifests depend on the Windows archive's name and layout. Renaming the
  archive or its directory means changing the script too.
