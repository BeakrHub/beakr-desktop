param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe")
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This packaged-binary regression currently drives Windows UI Automation."
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
public static class BeakrSingleInstanceWin32 {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int count);
}
"@

function Get-TauriWindowState([int]$TargetProcessId) {
    $rows = [Collections.Generic.List[object]]::new()
    [BeakrSingleInstanceWin32]::EnumWindows({
        param($windowHandle, $unused)
        [uint32]$ownerProcessId = 0
        [void][BeakrSingleInstanceWin32]::GetWindowThreadProcessId(
            $windowHandle,
            [ref]$ownerProcessId
        )
        if ($ownerProcessId -eq $TargetProcessId) {
            $className = [Text.StringBuilder]::new(256)
            [void][BeakrSingleInstanceWin32]::GetClassName(
                $windowHandle,
                $className,
                $className.Capacity
            )
            if ($className.ToString() -eq "Tauri Window") {
                $rows.Add([pscustomobject]@{
                    Handle    = $windowHandle
                    Visible   = [BeakrSingleInstanceWin32]::IsWindowVisible($windowHandle)
                    Minimized = [BeakrSingleInstanceWin32]::IsIconic($windowHandle)
                })
            }
        }
        return $true
    }, [IntPtr]::Zero) | Out-Null
    return $rows
}

function Find-TrayElements([string]$NamePrefix) {
    $root = [Windows.Automation.AutomationElement]::RootElement
    $all = $root.FindAll(
        [Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.Condition]::TrueCondition
    )
    $matches = [Collections.Generic.List[object]]::new()
    for ($i = 0; $i -lt $all.Count; $i++) {
        $element = $all.Item($i)
        if (
            $element.Current.Name -like "$NamePrefix*" -and
            $element.Current.ClassName -like "SystemTray.*"
        ) {
            $matches.Add($element)
        }
    }
    return $matches
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
$tempFiles = [Collections.Generic.List[string]]::new()
$previousRustLog = $env:RUST_LOG

function Start-TestInstance([string]$Tag) {
    $stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-single-$Tag-$([guid]::NewGuid()).stdout.log"
    $stderrPath = Join-Path ([IO.Path]::GetTempPath()) "beakr-single-$Tag-$([guid]::NewGuid()).stderr.log"
    $tempFiles.Add($stdoutPath)
    $tempFiles.Add($stderrPath)
    return [pscustomobject]@{
        Process = Start-Process `
            -FilePath $resolvedExecutable `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru
        Stderr = $stderrPath
    }
}

try {
    $env:RUST_LOG = "beakr_desktop=info"
    $first = Start-TestInstance "first"
    Start-Sleep -Seconds 4

    $startupLog = Get-Content -LiteralPath $first.Stderr -Raw -ErrorAction SilentlyContinue
    if ($startupLog -notmatch "Found stored device token, auto-connecting on startup") {
        throw "Precondition failed: first release startup did not prove that a stored device token was present."
    }

    $before = @(Get-TauriWindowState $first.Process.Id)
    if ($before | Where-Object { $_.Visible }) {
        throw "Precondition failed: first paired launch already had a visible Tauri window."
    }

    $second = Start-TestInstance "second"
    Start-Sleep -Seconds 4

    $running = @(
        Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -eq $resolvedExecutable }
    )
    if ($running.Count -ne 1) {
        throw "Expected one Beakr process after two launches; found $($running.Count)."
    }

    $after = @(Get-TauriWindowState $running[0].Id)
    $restored = @($after | Where-Object { $_.Visible -and -not $_.Minimized })
    if ($restored.Count -ne 1) {
        throw "Second launch did not restore exactly one visible, non-minimized Tauri window."
    }
    $foregroundHandle = [BeakrSingleInstanceWin32]::GetForegroundWindow()
    Write-Output "Window handle: $($restored[0].Handle); foreground handle: $foregroundHandle"
    if ($restored[0].Handle -ne $foregroundHandle) {
        throw "The surviving Beakr window was not focused after the second launch."
    }

    $trayElements = @(Find-TrayElements "Beakr Desktop")
    if ($trayElements.Count -eq 0) {
        $chevrons = @(Find-TrayElements "Show Hidden Icons")
        if ($chevrons.Count -ne 1) {
            throw "Notification overflow control was not uniquely available."
        }
        Invoke-Element $chevrons[0] "Show Hidden Icons"
        Start-Sleep -Seconds 1
        $trayElements = @(Find-TrayElements "Beakr Desktop")
    }
    if ($trayElements.Count -ne 1) {
        throw "Expected one Beakr tray icon after two launches; found $($trayElements.Count)."
    }

    Write-Output "PASS: two launches left one process, one tray icon, and one focused window."
}
finally {
    Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $resolvedExecutable } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    if ($null -eq $previousRustLog) {
        Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    }
    else {
        $env:RUST_LOG = $previousRustLog
    }
    if ($tempFiles.Count -gt 0) {
        Remove-Item -LiteralPath $tempFiles.ToArray() -Force -ErrorAction SilentlyContinue
    }
}
