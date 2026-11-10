# Prototype: libkrunfw.dll with netfilter for Windows sandboxes

`dockerd` doesn't start in sandboxes on Windows hosts, because microsandbox's
Windows guest kernel has no netfilter or bridge support (issue #264). This
directory builds a prototype `libkrunfw.dll` whose kernel has them, to check
that Docker works with it before we decide on a fix.

- `windows-netfilter.patch` changes `config-libkrunfw-windows_x86_64` in
  microsandbox's libkrunfw fork (`superradcompany/libkrunfw` at `223f116`, the
  `vendor/libkrunfw` submodule of microsandbox 0.7.14). It enables the 95
  netfilter, NAT and bridge options and the two POSIX message queue options
  (runc mounts `mqueue` at `/dev/mqueue`) that the Linux config
  `config-libkrunfw_x86_64` sets and the Windows config doesn't.
- `build.sh` clones the fork, applies the patch, builds the x86_64 kernel and
  links `libkrunfw.dll` with MinGW. It runs in an `ubuntu:24.04` container.

## Build

Run from the repository root, with Docker or Podman. The build takes a while and
needs a few GB of disk in the container.

```sh
mkdir -p scripts/libkrunfw-windows/out
docker run --rm -v "$PWD/scripts/libkrunfw-windows:/patch:ro" -v "$PWD/scripts/libkrunfw-windows/out:/out" ubuntu:24.04 bash /patch/build.sh
```

On Windows, in PowerShell:

```powershell
New-Item -ItemType Directory -Force scripts\libkrunfw-windows\out | Out-Null
docker run --rm -v "${PWD}\scripts\libkrunfw-windows:/patch:ro" -v "${PWD}\scripts\libkrunfw-windows\out:/out" ubuntu:24.04 bash /patch/build.sh
```

The build prints the netfilter options of the built kernel and writes
`scripts/libkrunfw-windows/out/libkrunfw.dll`,
`scripts/libkrunfw-windows/out/kernel.c` (for an MSVC build with libkrunfw's
`scripts/build-windows.ps1 -SkipKernelBundle`) and
`scripts/libkrunfw-windows/out/kernel.config`.

## Test on Windows

microsandbox only accepts `MSB_LIBKRUNFW_PATH` together with `MSB_PATH`, so
point `MSB_PATH` at the `msb.exe` that `fbkd` installed. `fbk` starts `fbkd`
with its own environment, so set both in the shell that runs `fbk`:

```powershell
Stop-Process -Name fbkd
$env:MSB_PATH = "$env:LOCALAPPDATA\firebrick\state\msb\bin\msb.exe"
$env:MSB_LIBKRUNFW_PATH = "$PWD\scripts\libkrunfw-windows\out\libkrunfw.dll"
fbk stop
fbk start
fbk run grep -iE 'iptables|nft|failed' /var/log/dockerd.log
fbk run docker run --rm hello-world
fbk run docker run --rm alpine wget -qO- https://example.com
```

A VM keeps the kernel it booted with, so stop and start the sandbox after you
change the DLL. To go back to the stock kernel, stop `fbkd`, remove both
variables and stop and start the sandbox again.
