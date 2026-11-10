# Installs fbkd on Windows for `mise run install-dev`, with the runtime the Windows release
# embeds, whose kernel has netfilter (ADR 0061).

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$env:MSB_EMBED_ARTIFACTS_DIR = & "$PSScriptRoot/libkrunfw-windows/build-runtime.ps1"
cargo install --locked --path "$root/crates/daemon"
exit $LASTEXITCODE
