# 50. Validate image references with oci-spec

## Status

Accepted

## Context

`image` in `.firebrick.yml` is attacker-controlled (ADR 0037) and was accepted
as any string. `fbkd` puts it in status messages, such as `image ... doesn't
exist`, and in log lines, so an image with escape sequences reached the terminal
and `fbkd.log`. microsandbox parses the image with `oci_client::Reference`, a
re-export of `oci_spec::distribution::Reference`, and can't pull anything that
type refuses. `oci-spec` is already in `Cargo.lock` through microsandbox.

## Decision

`firebrick-spec` depends on `oci-spec` with only the `distribution` feature.
`firebrick_spec::validate_image` parses the image with
`oci_spec::distribution::Reference`. The spec checks `image` while it parses,
and `fbkd` checks the image of every `StartSandbox` and `UpgradeSandbox` request
before using it.

## Consequences

- A reference passes exactly when microsandbox accepts it, so the check rejects
  no image that worked before, and the project maintains no grammar of its own.
- The grammar allows no whitespace or control characters, so a valid image is
  safe to show in a message or log line without stripping.
- `fbk` gets `oci-spec` and its dependencies, such as `regex`, as new
  dependencies; `fbkd` already had them.
- A tag may hold uppercase letters, which the OCI grammar allows, but the
  repository name may not.
