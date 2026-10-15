# Runs the vm-tests on Windows: `mise run integration-tests` runs this script
# there instead of `cargo test -p firebrick-daemon --features vm-tests`.
#
# Cargo runs test binaries in a job object without the breakaway right, and
# microsandbox starts each VM with CREATE_BREAKAWAY_FROM_JOB, which Windows
# refuses in such a job. So this script only builds the tests with cargo and
# runs their binaries itself, outside cargo's job (ADR 0050). Arguments are
# passed to each test binary, for example a test name to filter on.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$build = cargo test --manifest-path "$root/Cargo.toml" -p firebrick-daemon `
    --features vm-tests --no-run --message-format=json-render-diagnostics
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$tests = $build |
    ForEach-Object { $_ | ConvertFrom-Json } |
    Where-Object { $_.reason -eq 'compiler-artifact' -and $_.executable -and
        $_.target.kind -contains 'test' } |
    ForEach-Object { $_.executable }

# The `[env]` of .cargo/config.toml only reaches binaries that cargo runs, so
# set it here the same way: an MSB_HOME set in the environment takes precedence.
if (-not $env:MSB_HOME) { $env:MSB_HOME = '/tmp/firebrick-msb' }
$env:FIREBRICK_EDITOR_CONFIG_ROOT = '/tmp/firebrick-test/config'

# Every test VM gets 4 GiB, which WHP commits when it boots, and a VM that doesn't
# fit exits before its agent starts. libtest runs a test per CPU at once, which can
# be more than fits, so run as many as the free commit has room for, at 5 GiB each
# with the VM process, unless RUST_TEST_THREADS says otherwise.
if (-not $env:RUST_TEST_THREADS) {
    $freeGiB = (Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory / 1MB
    $cpus = [Environment]::ProcessorCount
    $env:RUST_TEST_THREADS = [Math]::Max(1, [Math]::Min($cpus, [Math]::Floor($freeGiB / 5)))
}

# Cargo runs a package's tests in the package's directory.
Push-Location "$root/crates/daemon"
$failed = @()
try {
    foreach ($test in $tests) {
        & $test @args
        if ($LASTEXITCODE -ne 0) { $failed += "$(Split-Path -Leaf $test) (exit $LASTEXITCODE)" }
    }
} finally {
    Pop-Location
}

# Name the failed binaries last, where a caller that keeps only the end of the
# output, such as the stop hook, still sees them.
if ($failed) {
    Write-Output "vm-tests failed in: $($failed -join ', ')"
    exit 1
}
exit 0
