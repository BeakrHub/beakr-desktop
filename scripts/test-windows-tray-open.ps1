param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe")
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This tray regression test only runs on Windows."
}

if (Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class BeakrTrayTestWin32 {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int count);
}
"@

function Get-TauriWindowState([int]$TargetProcessId) {
    $rows = [Collections.Generic.List[object]]::new()
    [BeakrTrayTestWin32]::EnumWindows({
        param($windowHandle, $unused)
        [uint32]$ownerProcessId = 0
        [void][BeakrTrayTestWin32]::GetWindowThreadProcessId(
            $windowHandle,
            [ref]$ownerProcessId
        )
        if ($ownerProcessId -eq $TargetProcessId) {
            $className = [Text.StringBuilder]::new(256)
            [void][BeakrTrayTestWin32]::GetClassName(
                $windowHandle,
                $className,
                $className.Capacity
            )
            if ($className.ToString() -eq "Tauri Window") {
                $rows.Add([pscustomobject]@{
                    Visible   = [BeakrTrayTestWin32]::IsWindowVisible($windowHandle)
                    Minimized = [BeakrTrayTestWin32]::IsIconic($windowHandle)
                })
            }
        }
        return $true
    }, [IntPtr]::Zero) | Out-Null
    return $rows
}

function Find-TrayElement([string]$NamePrefix) {
    $root = [Windows.Automation.AutomationElement]::RootElement
    $all = $root.FindAll(
        [Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.Condition]::TrueCondition
    )
    for ($i = 0; $i -lt $all.Count; $i++) {
        $element = $all.Item($i)
        if (
            $element.Current.Name -like "$NamePrefix*" -and
            $element.Current.ClassName -like "SystemTray.*"
        ) {
            return $element
        }
    }
    return $null
}

function Invoke-Element($Element, [string]$Description) {
    $pattern = $null
    if (-not $Element.TryGetCurrentPattern(
        [Windows.Automation.InvokePattern]::Pattern,
        [ref]$pattern
    )) {
        throw "$Description is not invokable through Windows UI Automation."
    }
    ([Windows.Automation.InvokePattern]$pattern).Invoke()
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-tray-$([guid]::NewGuid()).stdout.log"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-tray-$([guid]::NewGuid()).stderr.log"
$previousRustLog = $env:RUST_LOG
$process = $null

try {
    $env:RUST_LOG = "beakr_desktop=info"
    $process = Start-Process `
        -FilePath $resolvedExecutable `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath `
        -PassThru
    Start-Sleep -Seconds 4

    $startupLog = Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue
    if ($startupLog -notmatch "Found stored device token, auto-connecting on startup") {
        throw "Precondition failed: release startup did not prove that a stored device token was present."
    }

    $before = @(Get-TauriWindowState $process.Id)
    if (-not ($before | Where-Object { -not $_.Visible })) {
        throw "Precondition failed: paired release had no hidden Tauri window."
    }

    $tray = Find-TrayElement "Beakr Desktop"
    if (-not $tray) {
        $chevron = Find-TrayElement "Show Hidden Icons"
        if (-not $chevron) {
            throw "Neither Beakr nor the notification overflow control was found."
        }
        Invoke-Element $chevron "Show Hidden Icons"
        Start-Sleep -Seconds 1
        $tray = Find-TrayElement "Beakr Desktop"
    }
    if (-not $tray) {
        throw "Beakr Desktop tray element was not found after opening notification overflow."
    }

    Invoke-Element $tray "Beakr Desktop tray icon"
    Start-Sleep -Seconds 2
    $after = @(Get-TauriWindowState $process.Id)
    if (-not ($after | Where-Object { $_.Visible -and -not $_.Minimized })) {
        throw "Tray activation did not restore a visible, non-minimized Tauri window."
    }

    Write-Output "PASS: tray activation restored the paired release window."
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
