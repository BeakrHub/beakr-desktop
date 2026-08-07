param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe"),
    [int]$ExitTimeoutSeconds = 15
)

$ErrorActionPreference = "Stop"
$panicMarker = "ENG-1967 controlled startup panic"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This packaged panic regression currently runs on Windows."
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
if ($resolvedExecutable -notmatch "[\\/]release[\\/]") {
    throw "ENG-1967 panic logging must be tested with a release-profile executable."
}

if (Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

$logDirectory = Join-Path $env:LOCALAPPDATA "com.thebeakr.desktop\logs"
$startedAt = [DateTime]::UtcNow
$process = Start-Process -FilePath $resolvedExecutable -PassThru

try {
    $deadline = [DateTime]::UtcNow.AddSeconds($ExitTimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 200
        $process.Refresh()
        if ($process.HasExited) {
            break
        }
    }

    $process.Refresh()
    if (-not $process.HasExited) {
        throw "Controlled startup panic did not terminate the release within ${ExitTimeoutSeconds}s."
    }
    if ($process.ExitCode -eq 0) {
        throw "Controlled startup panic exited with code 0."
    }

    $freshLogs = @(
        Get-ChildItem -LiteralPath $logDirectory -File -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Length -gt 0 -and
                $_.LastWriteTimeUtc -ge $startedAt.AddSeconds(-2)
            }
    )
    if ($freshLogs.Count -eq 0) {
        throw "Controlled startup panic left no freshly modified Beakr log."
    }

    $panicLog = $freshLogs | Where-Object {
        (Get-Content -LiteralPath $_.FullName -Raw -ErrorAction SilentlyContinue) -match [regex]::Escape($panicMarker)
    } | Select-Object -First 1
    if (-not $panicLog) {
        throw "Fresh Beakr log did not contain the controlled panic marker."
    }

    $panicText = Get-Content -LiteralPath $panicLog.FullName -Raw
    if ($panicText -notmatch "Backtrace:") {
        throw "Panic log did not include a backtrace section."
    }

    Write-Output "PASS: controlled release panic exited nonzero and left a durable panic/backtrace log."
}
finally {
    $process.Refresh()
    if (-not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
        Wait-Process -Id $process.Id -ErrorAction SilentlyContinue
    }
}
