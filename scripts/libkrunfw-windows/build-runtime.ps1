# Builds the Windows runtime that fbkd embeds (ADR 0061): microsandbox's msb.exe next to a
# libkrunfw.dll whose kernel has netfilter, NAT, bridge and mqueue support. Writes msb.exe and
# libkrunfw.dll to <Output>\runtime and prints only that directory, for MSB_EMBED_ARTIFACTS_DIR;
# the build output goes to the host.
#
# Without -KernelBundle it first builds kernel.c with build-kernel.sh in a Docker or Podman
# container, which takes a few minutes. It skips the build when <Output> holds a runtime built from
# the same inputs, so cargo doesn't rebuild fbkd for an unchanged runtime.
#
# Needs the Visual Studio Build Tools with the C++ workload; libkrunfw links the DLL with MSVC, like
# microsandbox's release.

param(
    # A kernel.c that build-kernel.sh built, such as the one from the CI kernel job.
    [string] $KernelBundle = '',
    [string] $Output = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'target\libkrunfw-windows')
)

$ErrorActionPreference = 'Stop'

function Read-Versions {
    $versions = @{}
    foreach ($line in Get-Content -LiteralPath (Join-Path $PSScriptRoot 'versions.env')) {
        if ($line -match '^([A-Z0-9_]+)=(.+)$') { $versions[$Matches[1]] = $Matches[2] }
    }
    return $versions
}

# Returns the lowercase hex SHA-256 of a file. Get-FileHash doesn't load in the Windows PowerShell that
# mise starts from pwsh, so this uses .NET.
function Get-Sha256 {
    param([string] $Path)
    $stream = [IO.File]::OpenRead($Path)
    try {
        $digest = [Security.Cryptography.SHA256]::Create().ComputeHash($stream)
    } finally {
        $stream.Dispose()
    }
    return -join ($digest | ForEach-Object { $_.ToString('x2') })
}

# The SHA-256 over every file the runtime is built from, to tell whether a staged runtime is current.
function Get-InputsHash {
    $files = 'versions.env', 'windows-netfilter.patch', 'build-kernel.sh', 'build-runtime.ps1' |
        ForEach-Object { Join-Path $PSScriptRoot $_ }
    if ($KernelBundle) { $files += $KernelBundle }
    return ($files | ForEach-Object { Get-Sha256 $_ }) -join ','
}

function Invoke-Native {
    param([string] $Command, [string[]] $Arguments)
    & $Command @Arguments | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "$Command exited with $LASTEXITCODE" }
}

function Build-Kernel {
    param([hashtable] $Versions, [string] $Directory)
    $engine = @(Get-Command docker, podman -ErrorAction SilentlyContinue)[0]
    if (-not $engine) { throw 'building the kernel needs docker or podman' }
    New-Item -ItemType Directory -Force $Directory | Out-Null
    Invoke-Native $engine.Source @('run', '--rm', '--platform', 'linux/amd64',
        '-v', "${PSScriptRoot}:/patch:ro", '-v', "${Directory}:/out",
        $Versions.BUILD_IMAGE, 'bash', '/patch/build-kernel.sh')
    return (Join-Path $Directory 'kernel.c')
}

# Links kernel.c into libkrunfw.dll with libkrunfw's own MSVC build, as microsandbox's release does.
function Build-Library {
    param([hashtable] $Versions, [string] $Kernel, [string] $Destination)
    $source = Join-Path $Output 'libkrunfw'
    if (Test-Path $source) { Remove-Item -Recurse -Force $source }
    Invoke-Native git @('init', '-q', $source)
    Invoke-Native git @('-C', $source, 'fetch', '-q', '--depth', '1', $Versions.LIBKRUNFW_REPO, $Versions.LIBKRUNFW_COMMIT)
    Invoke-Native git @('-C', $source, 'checkout', '-q', 'FETCH_HEAD')
    Copy-Item -LiteralPath $Kernel (Join-Path $source 'kernel.c')
    Push-Location $source
    try {
        & .\scripts\build-windows.ps1 -SkipKernelBundle -AbiVersion $Versions.LIBKRUNFW_ABI `
            -Architecture x64 -Output libkrunfw.dll -ImportLibrary libkrunfw.lib | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "build-windows.ps1 exited with $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
    Copy-Item -LiteralPath (Join-Path $source 'libkrunfw.dll') $Destination
}

function Get-Msb {
    param([hashtable] $Versions, [string] $Destination)
    $url = "https://github.com/superradcompany/microsandbox/releases/download/v$($Versions.MICROSANDBOX_VERSION)/msb-windows-x86_64.exe"
    $download = "$Destination.download"
    Invoke-Native curl.exe @('--fail', '--location', '--silent', '--show-error', '--output', $download, $url)
    $hash = Get-Sha256 $download
    if ($hash -ne $Versions.MSB_EXE_SHA256) {
        Remove-Item -LiteralPath $download
        throw "$url has SHA-256 $hash, but versions.env pins $($Versions.MSB_EXE_SHA256)"
    }
    Move-Item -Force -LiteralPath $download $Destination
}

$runtime = Join-Path $Output 'runtime'
$stamp = Join-Path $Output 'inputs.sha256'
$inputs = Get-InputsHash
$current = (Test-Path $stamp) -and ((Get-Content -Raw -LiteralPath $stamp).Trim() -eq $inputs) -and
    (Test-Path (Join-Path $runtime 'msb.exe')) -and (Test-Path (Join-Path $runtime 'libkrunfw.dll'))
if (-not $current) {
    $versions = Read-Versions
    if (Test-Path $runtime) { Remove-Item -Recurse -Force $runtime }
    New-Item -ItemType Directory -Force $runtime | Out-Null
    if (-not $KernelBundle) { $KernelBundle = Build-Kernel $versions (Join-Path $Output 'kernel') }
    Build-Library $versions (Resolve-Path $KernelBundle).Path (Join-Path $runtime 'libkrunfw.dll')
    Get-Msb $versions (Join-Path $runtime 'msb.exe')
    Set-Content -NoNewline -LiteralPath $stamp -Value $inputs
}
Write-Output (Resolve-Path $runtime).Path
