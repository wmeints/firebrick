# Creates and removes an isolated environment for running `fbk` and `fbkd` end
# to end on Windows without touching the user's daemon, sandboxes or config.
# The Windows counterpart of smoke-env.sh.
#
#   smoke-env.ps1 up      creates the environment and prints its env file
#   smoke-env.ps1 down F  stops its sandboxes and daemon and deletes it
#
# Dot-source the env file (`. <env file>`) in every command that runs `fbk`.

param(
    [Parameter(Position = 0)][string]$Command,
    [Parameter(Position = 1)][string]$EnvFile
)

$ErrorActionPreference = 'Stop'
$repo = (git -C $PSScriptRoot rev-parse --show-toplevel) -replace '/', '\'

# Pipe names are one machine-wide namespace, so the environment's pipes get a
# prefix of their own, which also keeps fbk from finding the user's daemon.
function Up {
    $name = 'firebrick-smoke.' + -join ((48..57) + (97..122) | Get-Random -Count 4 | ForEach-Object { [char]$_ })
    $root = Join-Path ([IO.Path]::GetTempPath()) $name
    foreach ($dir in 'msb', 'data', 'state', 'config', 'editors', 'home', 'work') {
        New-Item -ItemType Directory -Force (Join-Path $root $dir) | Out-Null
    }

    $envFile = Join-Path $root 'env.ps1'
    @"
`$env:SMOKE_ROOT = '$root'
`$env:FIREBRICK_PIPE_PREFIX = '\\.\pipe\$name'
`$env:MSB_HOME = '$root\msb'
`$env:XDG_DATA_HOME = '$root\data'
`$env:XDG_STATE_HOME = '$root\state'
`$env:XDG_CONFIG_HOME = '$root\config'
`$env:FIREBRICK_EDITOR_CONFIG_ROOT = '$root\editors'
`$env:USERPROFILE = '$root\home'
`$env:SMOKE_WORK = '$root\work'
`$env:SMOKE_SSH_CONFIG = '$root\data\firebrick\ssh\config'
`$FIREBRICK = '$repo\target\debug\fbk.exe'
`$MSB = '$root\msb\bin\msb.exe'
"@ | Set-Content -Path $envFile -Encoding utf8
    $envFile
}

# Returns the ID of the process that serves the pipe, or 0 when nothing does.
function Get-PipeServer([string]$pipe) {
    if (-not ('SmokePipe' -as [type])) {
        Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; using Microsoft.Win32.SafeHandles;
public static class SmokePipe {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint pid);
}
'@
    }
    $name = $pipe -replace '^\\\\\.\\pipe\\', ''
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $name, [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $client.Connect(2000)
        $serverId = [uint32]0
        [void][SmokePipe]::GetNamedPipeServerProcessId($client.SafePipeHandle, [ref]$serverId)
        return $serverId
    } catch {
        return 0
    } finally {
        $client.Dispose()
    }
}

function Down([string]$envFile) {
    if (-not $envFile) { throw 'usage: smoke-env.ps1 down <env file>' }
    $tempRoot = [IO.Path]::GetTempPath().TrimEnd('\')
    $full = [IO.Path]::GetFullPath($envFile)
    if ($full -notmatch ('^' + [regex]::Escape($tempRoot) + '\\firebrick-smoke\.[0-9a-z]{4}\\env\.ps1$')) {
        throw "refusing to clean up ${envFile}: not a smoke-test environment"
    }
    . $full

    $daemon = Get-PipeServer "$env:FIREBRICK_PIPE_PREFIX-fbkd"
    if ($daemon) {
        $sandboxes = & $FIREBRICK ls --format json 2>$null | ConvertFrom-Json
        foreach ($sandbox in @($sandboxes)) {
            & $FIREBRICK stop $sandbox.name *> $null
            & $FIREBRICK rm $sandbox.name *> $null
            if ($LASTEXITCODE -ne 0) { Write-Warning "couldn't remove sandbox $($sandbox.name)" }
        }
        # Only the daemon that serves this environment's pipe, and only fbkd.
        $process = Get-Process -Id $daemon -ErrorAction SilentlyContinue
        if ($process -and $process.Path -like '*\fbkd.exe') {
            Stop-Process -Id $daemon -Force
            $process.WaitForExit(5000) | Out-Null
        }
    }

    Remove-Item -Recurse -Force $env:SMOKE_ROOT
    "removed $env:SMOKE_ROOT"
}

switch ($Command) {
    'up' { Up }
    'down' { Down $EnvFile }
    default {
        Write-Error 'usage: smoke-env.ps1 up | down <env file>'
        exit 2
    }
}
