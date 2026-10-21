---
title: Installation
description: Install the fbk and fbkd binaries from a release or from source.
---

Each [GitHub release](https://github.com/wmeints/firebrick/releases) has an
archive per platform with the `fbk` and `fbkd` binaries:

| Platform              | Target                      |
| --------------------- | --------------------------- |
| Linux x86_64          | `x86_64-unknown-linux-gnu`  |
| Linux ARM64           | `aarch64-unknown-linux-gnu` |
| macOS (Apple Silicon) | `aarch64-apple-darwin`      |
| Windows 11 x64        | `x86_64-pc-windows-msvc`    |

The Windows archive is a `.zip`; see
[Windows: run Firebrick natively](#windows-run-firebrick-natively) to install
it. The steps below install both binaries in `~/.local/bin`, which doesn't need
root permissions. Keep `fbk` and `fbkd` in the same directory, because the CLI
starts the daemon from its own directory. To build them with Cargo instead, see
[Installing with Cargo](#installing-with-cargo).

## System requirements

Check that your machine meets these requirements before you download anything.

- **Linux**, on x86_64 or ARM64:
  - A CPU with hardware virtualization (Intel VT-x or AMD-V) enabled in the
    firmware.
  - The KVM kernel module loaded, so `/dev/kvm` exists.
  - Read and write access to `/dev/kvm` for your user, usually through the `kvm`
    group.
  - glibc 2.35 or newer for the release binaries.

  When Linux runs inside a VM or a container, the host must expose `/dev/kvm` to
  it, for example through nested virtualization.
- **macOS**: Apple Silicon. Intel Macs aren't supported.
- **Windows**: Windows 11 on x64 with hardware virtualization enabled in the
  firmware and the Windows Hypervisor Platform turned on; see
  [Windows: run Firebrick natively](#windows-run-firebrick-natively). Windows
  support is a preview, like microsandbox's. Firebrick also runs the Linux
  release inside a WSL 2 distribution with nested virtualization; see
  [Windows: run Firebrick in WSL 2](#windows-run-firebrick-in-wsl-2).

On Linux, or in your WSL distribution, check KVM and glibc before you install:

```sh
ls -l /dev/kvm
test -r /dev/kvm && test -w /dev/kvm && echo "KVM is usable"
ldd --version | head -1   # glibc 2.35 or newer
```

When a check fails:

- **`/dev/kvm` is missing**: enable hardware virtualization in the firmware, or
  load the KVM module for your CPU with `sudo modprobe kvm_intel` or `sudo
  modprobe kvm_amd`. On WSL, enable nested virtualization as described in
  [Windows: run Firebrick in WSL 2](#windows-run-firebrick-in-wsl-2).
- **`/dev/kvm` exists, but KVM isn't usable**: add your user to the `kvm` group
  with `sudo usermod -aG kvm "$USER"`, then log out and in again. On WSL, run
  `wsl --shutdown` from PowerShell instead and open the distribution again.

## 1. Download and install the binaries

The commands in this step use bash or zsh syntax. If you use fish, run `bash`
first and run them in that shell.

Set the release to install and pick the target for your machine:

```sh
VERSION=v0.9.0
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  TARGET=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  Darwin-arm64)  TARGET=aarch64-apple-darwin ;;
  *) echo "Unsupported platform: $(uname -s)-$(uname -m). Stop here and build from source." ;;
esac
NAME="firebrick-$VERSION-$TARGET"
```

When this prints `Unsupported platform`, there's no release archive for your
machine. Skip the remaining steps and follow
[Building from source](#building-from-source) instead.

Download the archive and its checksum, and verify the archive:

```sh
curl -fLO "https://github.com/wmeints/firebrick/releases/download/$VERSION/$NAME.tar.gz"
curl -fLO "https://github.com/wmeints/firebrick/releases/download/$VERSION/$NAME.tar.gz.sha256"
shasum -a 256 -c "$NAME.tar.gz.sha256"   # or: sha256sum -c "$NAME.tar.gz.sha256"
```

Extract the archive and copy both binaries to `~/.local/bin`:

```sh
tar -xzf "$NAME.tar.gz"
mkdir -p ~/.local/bin
install -m 755 "$NAME/fbk" "$NAME/fbkd" ~/.local/bin/
```

## 2. Add `~/.local/bin` to your `PATH`

Check whether the directory is on your `PATH` already:

```sh
command -v fbk
```

When this prints nothing, add the directory to the startup file of your shell
and open a new terminal:

- **zsh**, the default shell on macOS:

  ```sh
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
  ```

- **bash**, the default shell on most Linux distributions:

  ```sh
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
  ```

- **fish**:

  ```sh
  fish_add_path ~/.local/bin
  ```

## 3. Verify the installation

```sh
fbk --version
fbk ls
```

`fbk --version` prints the installed version, for example `fbk 0.9.0`.

`fbk ls` starts `fbkd`, which installs the microsandbox runtime in
`~/.local/state/firebrick/msb`, and lists your sandboxes (none yet). An error
here means `fbkd` couldn't start, for example because it isn't next to `fbk`.

## macOS: remove the quarantine flag

The macOS binaries aren't signed. When you download the archive through a
browser instead of `curl`, macOS quarantines the binaries and refuses to run
them. Remove the quarantine flag:

```sh
xattr -d com.apple.quarantine ~/.local/bin/fbk ~/.local/bin/fbkd
```

## Windows: run Firebrick natively

`fbk` and `fbkd` run on Windows 11 x64 through the Windows Hypervisor Platform.
Windows support is experimental. Run these steps in PowerShell:

1. Turn on the Windows Hypervisor Platform from an administrator PowerShell, and
   restart Windows when it asks you to:

   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
   ```

2. Download the archive and its checksum, and verify the archive:

   ```powershell
   $Version = "v0.9.0"
   $Name = "firebrick-$Version-x86_64-pc-windows-msvc"
   $Base = "https://github.com/wmeints/firebrick/releases/download/$Version"
   Invoke-WebRequest "$Base/$Name.zip" -OutFile "$Name.zip"
   Invoke-WebRequest "$Base/$Name.zip.sha256" -OutFile "$Name.zip.sha256"
   $Expected = (Get-Content "$Name.zip.sha256").Split(" ")[0]
   if ((Get-FileHash "$Name.zip").Hash -ne $Expected) { throw "Checksum mismatch" }
   ```

3. Extract the archive and copy both binaries to
   `%LOCALAPPDATA%\Programs\firebrick`:

   ```powershell
   Expand-Archive "$Name.zip" -DestinationPath .
   $Dir = "$env:LOCALAPPDATA\Programs\firebrick"
   New-Item -ItemType Directory -Force $Dir | Out-Null
   Copy-Item "$Name\fbk.exe", "$Name\fbkd.exe" $Dir
   ```

4. Add the directory to your `PATH`, unless it's there already, and open a new
   terminal:

   ```powershell
   $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
   if (";$UserPath;" -notlike "*;$Dir;*") {
     [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
   }
   ```

5. Verify the installation with `fbk --version` and `fbk ls`, as in
   [Verify the installation](#3-verify-the-installation).

The binaries aren't signed. When you download the archive through a browser
instead, unblock it with `Unblock-File "$Name.zip"` before you extract it, or
SmartScreen may refuse to start `fbkd`. To build the binaries from source
instead, install the Visual Studio Build Tools with the C++ workload, which Rust
needs to link them, from an administrator PowerShell, then follow
[Building from source](#building-from-source). Cargo installs `fbk.exe` and
`fbkd.exe` in `%USERPROFILE%\.cargo\bin`.

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools --override "--passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

On Windows, Firebrick keeps its data and logs in `%LOCALAPPDATA%\firebrick` and
your approvals in `%APPDATA%\firebrick`; see
[File locations](/docs/configuration/#file-locations). The generated SSH config
is included from `%USERPROFILE%\.ssh\config`, so the `.fbk` host names work for
Windows' OpenSSH and the editors that use it. The clipboard works without extra
tools, and `~user` in a mount's host path isn't supported.

`fbk` starts the daemon without a console, so nothing can ask it to stop
cleanly. `Stop-Process -Name fbkd` ends it at once: running sandboxes keep
running, but a network or image upgrade that's still in progress is cut off, and
the last few seconds of network activity aren't recorded. Stop it when no `fbk`
command is running. Windows can't replace or remove a running program, so stop
the daemon before you upgrade or uninstall Firebrick.

## Windows: run Firebrick in WSL 2

The Linux release also runs in a WSL 2 distribution when WSL exposes KVM to it
through nested virtualization. Check the
[System requirements](#system-requirements) for Windows first.

1. Enable nested virtualization in `%UserProfile%\.wslconfig` on Windows:

   ```ini
   [wsl2]
   nestedVirtualization=true
   ```

2. Restart WSL from PowerShell so it picks up the setting:

   ```powershell
   wsl --shutdown
   ```

3. In the WSL distribution, run the checks from
   [System requirements](#system-requirements). When `/dev/kvm` is missing
   there, nested virtualization isn't active.

Then follow the Linux steps above inside the distribution, and keep your
projects in the distribution's file system, for example under `~/projects`,
instead of under `/mnt/c`.

`fbkd` generates its SSH config in the distribution and includes it from
`~/.ssh/config` there, so the `.fbk` host names from
[Editor support](/docs/editor-support/) only resolve for `ssh` and editors that
run inside WSL. Editors on Windows don't see them.

## Upgrading and uninstalling

To upgrade, repeat step 1 with the new `VERSION`, then stop the running daemon
so the CLI starts the new one on the next command:

```sh
pkill -TERM -x fbkd
```

To uninstall, stop the daemon and remove the binaries:

```sh
pkill -TERM -x fbkd
rm ~/.local/bin/fbk ~/.local/bin/fbkd
```

On Windows, stop the daemon with `Stop-Process -Name fbkd` instead, then repeat
steps 2 and 3 of
[Windows: run Firebrick natively](#windows-run-firebrick-natively) to upgrade,
or remove `%LOCALAPPDATA%\Programs\firebrick` to uninstall.

## Installing with Cargo

Each release is also published to crates.io. Instead of downloading an archive,
build and install both binaries with Cargo. This needs a
[Rust toolchain](https://rustup.rs/) and `protoc` 3.15 or newer, the Protocol
Buffers compiler, on your `PATH`. The `protobuf-compiler` package of Ubuntu
22.04 is too old; install a current release from the
[protobuf releases](https://github.com/protocolbuffers/protobuf/releases)
instead:

```sh
cargo install firebrick-cli firebrick-daemon
fbk --version
```

Cargo installs `fbk` and `fbkd` in `~/.cargo/bin`, so both binaries end up in
the same directory, as the CLI requires. Install both crates with the same
version. To upgrade, run the same command again and stop the running daemon with
`pkill -TERM -x fbkd`. To uninstall, stop the daemon and run `cargo uninstall
firebrick-cli firebrick-daemon`.

## Building from source

Install the development toolchain with [mise](https://mise.jdx.dev) from a clone
of the repository, then let `cargo install` put both binaries in `~/.cargo/bin`.
Make sure that directory is on your `PATH`, as in step 2:

```sh
git clone https://github.com/wmeints/firebrick.git
cd firebrick
mise install
cargo install --locked --path crates/cli
cargo install --locked --path crates/daemon
```

A build from source that isn't a release defaults to a `firebrick-base` image
tag that may not exist yet. Set `image` in `.firebrick.yml` to a released tag or
your own image; see [Custom images](/docs/custom-images/).
