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

Linux needs KVM, and the Linux binaries need glibc 2.35 or newer. Windows isn't
supported.

The steps below install both binaries in `~/.local/bin`, which doesn't need root
permissions. Keep `fbk` and `fbkd` in the same directory, because the CLI starts
the daemon from its own directory. To build them with Cargo instead, see
[Installing with Cargo](#installing-with-cargo).

## 1. Download and install the binaries

The commands in this step use bash or zsh syntax. If you use fish, run `bash`
first and run them in that shell.

Set the release to install and pick the target for your machine:

```sh
VERSION=v0.4.0
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

`fbk --version` prints the installed version, for example `fbk 0.4.0`.

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
cargo install-cli     # cargo install --locked --path crates/cli
cargo install-daemon  # cargo install --locked --path crates/daemon
```

A build from source that isn't a release defaults to a `firebrick-base` image
tag that may not exist yet. Set `image` in `.firebrick.yml` to a released tag or
your own image; see [Custom images](/firebrick/docs/custom-images/).
