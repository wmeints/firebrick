# libkrunfw.dll with netfilter for Windows sandboxes

microsandbox's Windows guest kernel has no netfilter, bridge or mqueue support,
so `dockerd` can't start in sandboxes on Windows hosts (issue #264). The Windows
`fbkd` therefore embeds microsandbox's `msb.exe` next to a `libkrunfw.dll` that
this directory builds, whose kernel has them
([ADR 0061](../../docs/architecture/decisions/0061-embed-a-libkrunfw-with-netfilter-on-windows.md)).

- `versions.env` pins the microsandbox release, the SHA-256 of its `msb.exe`,
  the libkrunfw fork commit that release builds from (the `vendor/libkrunfw`
  submodule of its tag) and the container the kernel builds in.
- `windows-netfilter.patch` changes `config-libkrunfw-windows_x86_64` in that
  fork. It enables the netfilter, NAT and bridge options and the two POSIX
  message queue options (runc mounts `mqueue` at `/dev/mqueue`) that the Linux
  config `config-libkrunfw_x86_64` sets and the Windows config doesn't.
- `build-kernel.sh` clones the fork, applies the patch and builds `kernel.c` in
  the pinned container. It fails unless the kernel has the options Docker needs,
  and also writes the kernel configuration and the complete kernel source, which
  the release publishes.
- `build-runtime.ps1` links `kernel.c` into `libkrunfw.dll` with libkrunfw's
  `scripts/build-windows.ps1 -SkipKernelBundle` and MSVC, like microsandbox's
  release, downloads and checks the pinned `msb.exe`, and prints the directory
  that holds both, for `MSB_EMBED_ARTIFACTS_DIR`.

The `libkrunfw-windows.yaml` workflow builds the runtime in CI, and the release
workflow embeds it in the Windows package.

## Build on Windows

`build-runtime.ps1` needs Docker or Podman for the kernel and the Visual Studio
Build Tools with the C++ workload for the DLL. It writes to
`target\libkrunfw-windows`, takes a few minutes the first time, and reuses the
runtime while its inputs don't change. `mise run integration-tests` and `mise
run install-dev` run it for you. To build `fbkd` with it yourself:

```powershell
$env:MSB_EMBED_ARTIFACTS_DIR = & scripts\libkrunfw-windows\build-runtime.ps1
cargo build --bin fbkd
```

Without `MSB_EMBED_ARTIFACTS_DIR`, `fbkd` embeds microsandbox's own runtime,
whose kernel can't run Docker.

## Upgrade microsandbox

A unit test in `crates/daemon/src/runtime.rs` fails until `versions.env` pins
the microsandbox version in `Cargo.lock`. Then:

1. Set `LIBKRUNFW_COMMIT` to the `vendor/libkrunfw` submodule of the new
   microsandbox tag, and `LIBKRUNFW_ABI` to `LIBKRUNFW_ABI` in
   `microsandbox-utils`.
2. Set `MICROSANDBOX_VERSION` and `MSB_EXE_SHA256`, the SHA-256 of
   `msb-windows-x86_64.exe` in the new release.
3. Port `windows-netfilter.patch` when it no longer applies, and run
   `build-runtime.ps1` and `mise run integration-tests`.
4. Drop this directory once microsandbox's Windows config sets the options
   itself.
