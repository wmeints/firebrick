#!/bin/bash
# Builds a prototype libkrunfw.dll whose Windows guest kernel has netfilter, NAT and bridge
# support, so dockerd can start in Windows sandboxes (issue #264). Runs inside an ubuntu:24.04
# container with this directory mounted at /patch and an output directory at /out; see README.md.
#
# It builds in the container's own filesystem: the kernel tree has symlinks and files that differ
# only in case (xt_TCPMSS.c and xt_tcpmss.c), which macOS and Windows mounts can't hold.
set -euo pipefail

# The libkrunfw fork microsandbox 0.7.14 builds its runtime from (its vendor/libkrunfw submodule).
LIBKRUNFW_REPO=https://github.com/superradcompany/libkrunfw.git
LIBKRUNFW_COMMIT=223f11693774646eaa8ba892e3a6ebd07653154b

# On arm64 hosts the x86_64 kernel is cross-compiled. Ubuntu 24.04's 6.8 kernel headers predate the
# __u128 in arm64's sigcontext.h, which breaks the host build of the 6.12 objtool on newer distros.
packages=(make gcc flex bison bc perl python3 python3-pyelftools curl git ca-certificates diffutils
    patch xz-utils tar libelf-dev libssl-dev dwarves gcc-mingw-w64-x86-64)
cross_compile=
if [ "$(uname -m)" != x86_64 ]; then
    packages+=(gcc-x86-64-linux-gnu binutils-x86-64-linux-gnu)
    cross_compile=x86_64-linux-gnu-
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq "${packages[@]}" >/dev/null

git init -q /src
cd /src
git remote add origin "$LIBKRUNFW_REPO"
git fetch -q --depth 1 origin "$LIBKRUNFW_COMMIT"
git checkout -q FETCH_HEAD
git apply /patch/windows-netfilter.patch

# ARCH and CROSS_COMPILE go in the environment: on libkrunfw's make command line they don't reach
# the kernel's sub-makes, which then configure and build an arm64 kernel on arm64 hosts.
export ARCH=x86_64 CROSS_COMPILE="$cross_compile"
make -j"$(nproc)" OS=Windows kernel.c

if ! grep -qE '^# Linux/x86(_64)? ' linux-*/.config; then
    echo "the kernel wasn't configured for x86_64:" >&2
    sed -n 3p linux-*/.config >&2
    exit 1
fi

echo "--- netfilter, bridge and mqueue options in the built kernel"
grep -E '^CONFIG_(NETFILTER|NF_TABLES|NFT_COMPAT|NF_NAT|NF_CONNTRACK|IP_NF_IPTABLES|IP_NF_NAT|BRIDGE|BRIDGE_NETFILTER|VETH|POSIX_MQUEUE)=' \
    linux-*/.config

# Linked with MinGW for the prototype, with the command libkrunfw's Makefile uses for
# WINDOWS_TOOLCHAIN=mingw; microsandbox's release links kernel.c with MSVC
# (scripts/build-windows.ps1 -SkipKernelBundle). The Makefile itself would pick the exported
# CROSS_COMPILE's gcc, and passing CC to it would rebuild the kernel with MinGW.
x86_64-w64-mingw32-gcc -DABI_VERSION=5 -shared -Wl,--section-alignment,65536 -o libkrunfw.dll \
    kernel.c libkrunfw.def
cp libkrunfw.dll /out/libkrunfw.dll
cp kernel.c /out/kernel.c
cp linux-*/.config /out/kernel.config
cd /out
sha256sum libkrunfw.dll kernel.c kernel.config
