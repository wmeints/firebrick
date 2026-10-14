# 49. Pin the base image digest and the apt signing keys

## Status

Accepted

## Context

The `firebrick-base` image built `FROM ubuntu:26.04`, a tag that Docker Hub can
point at another image at any time. It also trusted whatever key
`https://mise.jdx.dev/gpg-key.pub` and
`https://download.docker.com/linux/ubuntu/gpg` served, and that key decides
which `mise` and Docker packages apt accepts. A retagged base image or a
compromised download server would end up in every sandbox built afterwards,
without a change in the repository.

## Decision

- `FROM` names the tag and the digest of its multi-platform index:
  `ubuntu:26.04@sha256:<digest>`. The index digest covers both platforms that
  `.github/workflows/image.yaml` builds, `linux/amd64` and `linux/arm64`.
- The `Dockerfile` checks the fingerprint of each downloaded apt signing key
  before it adds the repository. The expected fingerprints are the ones the
  projects publish: `24853EC9F655CE80B48E6C3A8B81C9D17413A06D` in mise's
  installation docs and `9DC858229FC7DD38854AE2D88D81803C0EBFCD88` in Docker's
  Engine install docs. A file with another key, or with more than one key, fails
  the build with a message that names the repository and the expected
  fingerprint.
- Dependabot (`.github/dependabot.yml`) proposes digest updates for the
  `Dockerfile` weekly, seven days after a new image at the earliest, like the
  action updates of
  [ADR 0043](0043-pin-github-actions-and-ci-tools-to-checksums.md).

## Consequences

- A new Ubuntu image reaches the base image only through a Dependabot pull
  request, which a maintainer reviews. Security updates of the Ubuntu image
  arrive up to a week later; `apt-get` still installs the current packages.
- When mise or Docker rotate their signing key, the image build fails until a
  maintainer checks the new fingerprint against the project's documentation and
  updates the `ARG` in the `Dockerfile`.
- The apt packages themselves stay unpinned; apt verifies them against the
  pinned keys.
- `fbkd` still pulls the default image by tag.
