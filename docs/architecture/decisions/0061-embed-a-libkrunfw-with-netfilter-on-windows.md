# 61. Embed a libkrunfw with netfilter on Windows

## Status

Accepted

## Context

`fbkd` embeds the microsandbox runtime, `msb` and `libkrunfw`, which holds the
guest kernel ([ADR 0002](0002-embed-the-microsandbox-runtime-in-anvild.md)). On
Windows hosts, microsandbox 0.7.14 builds that kernel from
`config-libkrunfw-windows_x86_64` in its libkrunfw fork. That config leaves out
netfilter, nf_tables, NAT, conntrack, the bridge driver and POSIX message
queues, which the Linux config `config-libkrunfw_x86_64` sets. `dockerd` from
the base image ([ADR 0016](0016-run-docker-in-the-sandbox-vm.md)) then exits at
boot: `iptables -t nat -N DOCKER` fails with `Failed to initialize nft: Protocol
not supported`. Without mqueue, runc can't mount `/dev/mqueue` and no container
starts (issue #264).

We considered three options:

1. Wait for microsandbox to enable the options. We asked and got no answer.
2. Build our own `libkrunfw.dll` with the options and embed it in the Windows
   `fbkd`.
3. Start `dockerd` without iptables and bridge networking, and strip the
   `mqueue` mount from runc's bundles. Docker starts, but containers only get
   host networking, `-p` publishes nothing, Compose networks fail and POSIX
   message queues don't work.

A prototype of option 2 booted on Windows 11 with WHP, and bridge networks,
published ports, user-defined networks, `docker build` and mqueue all worked.

## Decision

The Windows `fbkd` embeds microsandbox's own `msb.exe` next to a `libkrunfw.dll`
we build, through the `MSB_EMBED_ARTIFACTS_DIR` of the `microsandbox` build
script. Linux and macOS keep microsandbox's runtime.

- `scripts/libkrunfw-windows/windows-netfilter.patch` enables the netfilter,
  NAT, bridge and mqueue options of the Linux config in the Windows config.
  `versions.env` pins the libkrunfw fork commit that microsandbox builds its
  release from, the SHA-256 of that release's `msb-windows-x86_64.exe` and the
  digest of the `ubuntu:24.04` container the kernel builds in.
- `build-kernel.sh` builds `kernel.c` in that container and fails unless the
  kernel has the options Docker needs. `build-runtime.ps1` links `kernel.c` into
  `libkrunfw.dll` with libkrunfw's `scripts/build-windows.ps1 -SkipKernelBundle`
  and MSVC, as microsandbox's release does, and stages it next to the pinned
  `msb.exe`.
- The `libkrunfw-windows.yaml` workflow runs both on pull requests that change
  them. `release.yaml` calls it, builds the Windows package with the runtime it
  produces, and attaches the kernel's source (the patched libkrunfw tree with
  the Linux tarball) and its configuration to the release, as the GPL-2.0
  requires.
- `fbkd`'s build script records the SHA-256 of the embedded `libkrunfw` when
  `MSB_EMBED_ARTIFACTS_DIR` supplies it. On startup `fbkd` replaces a runtime in
  the microsandbox home whose library differs, because an earlier `fbkd` with
  the same `msb` version installed microsandbox's library. This adds `sha2` as a
  dependency and `microsandbox-utils` as a build dependency, both in the
  versions microsandbox uses. A library configured outside the home, such as
  through `MSB_LIBKRUNFW_PATH`, is the user's choice and stays.
- On Windows, `mise run integration-tests` and `mise run install-dev` build the
  runtime with `build-runtime.ps1` and embed it.

## Consequences

- Docker in sandboxes on Windows hosts works as on Linux and macOS: bridge and
  Compose networks, `-p`, builds and mqueue. A vm-test starts `dockerd` on every
  host and checks its `DOCKER` NAT chain, the `bridge` network and mqueue.
  Another checks that `fbkd` keeps the runtime it installed, so the recorded
  hash matches the library microsandbox embeds.
- A microsandbox upgrade needs new pins in `versions.env`, and the patch may
  need porting. A unit test fails until `MICROSANDBOX_VERSION` matches the
  `microsandbox` crate.
- The release takes longer: every package waits for the kernel build, and a
  failed kernel build stops the release.
- Building or testing on Windows needs Docker or Podman for the kernel and the
  Visual Studio Build Tools for the DLL. The first build takes a few minutes;
  `build-runtime.ps1` reuses the result while its inputs don't change.
- A plain `cargo build` on Windows, without `MSB_EMBED_ARTIFACTS_DIR`, still
  embeds microsandbox's runtime, whose kernel can't run Docker. So does `cargo
  install firebrick-daemon`.
- Each release publishes the kernel source, about 230 MB.
- Sandboxes that run while `fbkd` replaces the library keep the old kernel until
  they restart.
- Drop the custom build once microsandbox's Windows kernel has these options.
