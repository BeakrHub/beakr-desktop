param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe"),
    [int]$ObservationSeconds = 10,
    [switch]$ExpectUpdateAvailable
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This release-profile regression test only runs on Windows."
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
if ($resolvedExecutable -notmatch "[\\/]release[\\/]") {
    throw "ENG-1965 must be tested with a release-profile executable."
}

if (Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class BeakrWindowlessUpdaterWin32 {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int count);
}
"@

function Get-VisibleTauriWindowCount([int]$TargetProcessId) {
    $visibleHandles = [Collections.Generic.List[IntPtr]]::new()
    [BeakrWindowlessUpdaterWin32]::EnumWindows({
        param($windowHandle, $unused)
        [uint32]$ownerProcessId = 0
        [void][BeakrWindowlessUpdaterWin32]::GetWindowThreadProcessId(
            $windowHandle,
            [ref]$ownerProcessId
        )
        if ($ownerProcessId -eq $TargetProcessId) {
            $className = [Text.StringBuilder]::new(256)
            [void][BeakrWindowlessUpdaterWin32]::GetClassName(
                $windowHandle,
                $className,
                $className.Capacity
            )
            if (
                $className.ToString() -eq "Tauri Window" -and
                [BeakrWindowlessUpdaterWin32]::IsWindowVisible($windowHandle)
            ) {
                $visibleHandles.Add($windowHandle)
            }
        }
        return $true
    }, [IntPtr]::Zero) | Out-Null
    return $visibleHandles.Count
}

$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-updater-$([guid]::NewGuid()).stdout.log"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-updater-$([guid]::NewGuid()).stderr.log"
$previousRustLog = $env:RUST_LOG
$process = $null

try {
    $env:RUST_LOG = "beakr_desktop=info,tauri_plugin_updater=debug"
    $process = Start-Process `
        -FilePath $resolvedExecutable `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath `
        -PassThru

    Start-Sleep -Seconds $ObservationSeconds
    $process.Refresh()
    if ($process.HasExited) {
        throw "Release exited before the startup updater could be observed (exit code $($process.ExitCode))."
    }

    $startupLog = Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue
    if ($startupLog -notmatch "Found stored device token, auto-connecting on startup") {
        throw "Precondition failed: release startup did not prove that a stored device token was present."
    }

    if ($startupLog -notmatch "checking for updates") {
        throw "Paired windowless release never checked the updater feed during startup."
    }

    $visibleWindows = Get-VisibleTauriWindowCount $process.Id
    if ($ExpectUpdateAvailable) {
        if ($startupLog -notmatch "Update [^ ]+ is available") {
            throw "Precondition failed: test build did not discover an available update."
        }
        if ($visibleWindows -ne 1) {
            throw "Available update did not open exactly one settings window; found $visibleWindows visible Tauri window(s)."
        }
        Write-Output "PASS: windowless release found an update and opened one settings window without relaunching."
    }
    else {
        if ($visibleWindows -ne 0) {
            throw "Precondition failed: paired release was not windowless; found $visibleWindows visible Tauri window(s)."
        }
        Write-Output "PASS: paired windowless release checked the updater feed during startup."
    }
}
finally {
    if ($process) {
        $process.Refresh()
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            Wait-Process -Id $process.Id -ErrorAction SilentlyContinue
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
