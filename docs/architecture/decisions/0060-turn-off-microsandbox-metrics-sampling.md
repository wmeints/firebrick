# 60. Turn off microsandbox metrics sampling

## Status

Accepted

## Context

An idle sandbox's `msb` process used about 10% of a host core on macOS. The
guest was idle and the vCPU threads used less than 1%. The time went to
microsandbox's metrics sampler: once per second, it calls `mincore` over all of
the guest's memory to count the host-resident pages. With 4 GiB of guest memory
that is 262,144 pages of 16 KiB, and one such call takes about 70 ms on an Apple
Silicon Mac. The sampler publishes CPU, memory, disk and network figures in a
shared-memory registry for `msb metrics` and the SDK's metrics calls.

Firebrick reads none of these figures.

## Decision

`fbkd` creates every sandbox with `SandboxBuilder::disable_metrics_sample()`, so
the `msb` process starts no sampler.

## Consequences

- An idle sandbox costs almost no host CPU, and the cost no longer grows with
  its memory size.
- `msb metrics` and the SDK's metrics calls return no figures for firebrick
  sandboxes. Showing resource usage in `fbk` would need the sampler back, at a
  longer interval than its 1 second default.
- `modify()` can't change the setting, so sandboxes created by an older `fbkd`
  keep sampling until `fbk upgrade` or `fbk network` recreates them.
