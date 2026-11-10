#!/bin/bash
# Builds the x86_64 guest kernel of the libkrunfw.dll that fbkd embeds on Windows, with the
# netfilter, NAT, bridge and mqueue options of windows-netfilter.patch (ADR 0061). Runs inside the
# BUILD_IMAGE container from versions.env, with this directory mounted at /patch and an output
# directory at /out, and writes:
#
# - kernel.c: the kernel bundle that build-runtime.ps1 links into libkrunfw.dll with MSVC.
# - kernel.config: the configuration the kernel was built with.
# - libkrunfw-windows-source.tar.gz: the patched libkrunfw tree with the Linux source tarball, the
#   complete source of the kernel (GPL-2.0).
#
# It builds in the container's own filesystem: the kernel tree has symlinks and files that differ
# only in case (xt_TCPMSS.c and xt_tcpmss.c), which macOS and Windows mounts can't hold.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=versions.env
source /patch/versions.env

# On arm64 hosts the x86_64 kernel is cross-compiled. Ubuntu 24.04's 6.8 kernel headers predate the
# __u128 in arm64's sigcontext.h, which breaks the host build of the 6.12 objtool on newer distros.
packages=(make gcc flex bison bc perl python3 python3-pyelftools curl git ca-certificates diffutils
    patch xz-utils tar libelf-dev libssl-dev dwarves)
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

# Fail when the configuration lost one of the options Docker needs, for example because a new
# kernel version renamed it and olddefconfig dropped it.
required=(NETFILTER NF_TABLES NFT_COMPAT NF_NAT NF_CONNTRACK IP_NF_IPTABLES IP_NF_NAT BRIDGE
    BRIDGE_NETFILTER VETH POSIX_MQUEUE)
for option in "${required[@]}"; do
    if ! grep -qx "CONFIG_$option=y" linux-*/.config; then
        echo "the kernel configuration doesn't set CONFIG_$option=y" >&2
        exit 1
    fi
done

cp kernel.c /out/kernel.c
cp linux-*/.config /out/kernel.config
tar --exclude=./.git --exclude='./linux-*' --exclude=./kernel.c \
    -czf /out/libkrunfw-windows-source.tar.gz .
cd /out
sha256sum kernel.c kernel.config libkrunfw-windows-source.tar.gz
