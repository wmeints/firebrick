# 66. Publish the website with each release

## Status

Accepted. Changes when the site deploys from
[ADR 0032](0032-publish-the-website-on-github-pages.md).

## Context

`.github/workflows/website.yaml` deployed the site to GitHub Pages on every push
to `main` that changed `website/`
([ADR 0032](0032-publish-the-website-on-github-pages.md)). The docs describe
features as soon as they merge, so a user who installs the latest release reads
about options and commands that their `fbk` doesn't have yet. The installation
docs also name the version, which the version bump pull request updates.

## Decision

The site deploys only as part of a release:

- `website.yaml` keeps checking the site on pull requests, pushes to `main` and
  manual runs, but only deploys when another workflow calls it with
  `workflow_call` and a `tag` input. It then checks out that tag, runs the same
  checks, uploads `website/dist` and deploys it.
- `release.yaml` calls `website.yaml` in a `website` job once the `release` job
  has created the GitHub release, and skips it for pre-release tags, so the docs
  of the last full release stay online.
- The calling jobs grant `pages: write` and `id-token: write`, because a called
  workflow gets no more permissions than its caller: the `website` job in
  `release.yaml` and the `release` job in `tag-release.yaml`.

## Consequences

- The published docs match the latest release. Docs changes merged since then
  wait for the next release.
- A fix to the docs alone needs a release, or a re-run of the release's
  `website` job, to go online.
- A tag pushed by hand runs `release.yaml` on the tag's ref. The `github-pages`
  environment only allows deployments from `main` by default, so its deployment
  rules must also allow `v*` tags for such a release to deploy the site.
