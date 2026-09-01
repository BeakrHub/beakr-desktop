param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe"),
    [int]$MinimumLifetimeSeconds = 30
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This lifecycle regression test only runs on Windows."
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$existing = Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue
if ($existing) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-lifetime-$([guid]::NewGuid()).stdout.log"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-lifetime-$([guid]::NewGuid()).stderr.log"
$previousRustLog = $env:RUST_LOG
$process = $null

try {
    $env:RUST_LOG = "beakr_desktop=info"
    $process = Start-Process `
        -FilePath $resolvedExecutable `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath `
        -PassThru

    $deadline = [DateTime]::UtcNow.AddSeconds($MinimumLifetimeSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "Paired release exited before ${MinimumLifetimeSeconds}s (exit code $($process.ExitCode))."
        }
    }

    $startupLog = Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue
    if ($startupLog -notmatch "Found stored device token, auto-connecting on startup") {
        throw "Precondition failed: release startup did not prove that a stored device token was present."
    }

    Write-Output "PASS: paired release stayed alive for at least ${MinimumLifetimeSeconds}s."
}
finally {
    if ($process) {
        $process.Refresh()
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
        }
    }
    if ($null -eq $previousRustLog) {
        Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    }
    else {
        $env:RUST_LOG = $previousRustLog
    }
    Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
}
