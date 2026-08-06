param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe"),
    [int]$ExitTimeoutSeconds = 8
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This WebView2 regression test only runs on Windows."
}

if (Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class BeakrWebViewDialogWin32 {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int count);
}
"@

function Find-WebViewErrorDialog([int]$TargetProcessId) {
    $handles = [Collections.Generic.List[IntPtr]]::new()
    [BeakrWebViewDialogWin32]::EnumWindows({
        param($windowHandle, $unused)
        [uint32]$ownerProcessId = 0
        [void][BeakrWebViewDialogWin32]::GetWindowThreadProcessId(
            $windowHandle,
            [ref]$ownerProcessId
        )
        if ($ownerProcessId -eq $TargetProcessId) {
            $className = [Text.StringBuilder]::new(256)
            [void][BeakrWebViewDialogWin32]::GetClassName(
                $windowHandle,
                $className,
                $className.Capacity
            )
            if ($className.ToString() -eq "#32770") {
                $handles.Add($windowHandle)
            }
        }
        return $true
    }, [IntPtr]::Zero) | Out-Null
    if ($handles.Count -gt 0) {
        return [Windows.Automation.AutomationElement]::FromHandle($handles[0])
    }
    return $null
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$previousWebViewFolder = $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER
$missingFolder = Join-Path ([IO.Path]::GetTempPath()) "beakr-missing-webview2-$([guid]::NewGuid())"
$testStartedAt = Get-Date
$primary = $null
$secondary = $null

try {
    $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = $missingFolder
    $primary = Start-Process -FilePath $resolvedExecutable -PassThru
    Start-Sleep -Seconds 3

    # A paired release stays silent on first launch. A second launch exercises
    # the single-instance recovery path, which must attempt to create the UI.
    $secondary = Start-Process -FilePath $resolvedExecutable -PassThru

    $dialog = $null
    $dialogDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not $dialog -and [DateTime]::UtcNow -lt $dialogDeadline) {
        Start-Sleep -Milliseconds 250
        $dialog = Find-WebViewErrorDialog $primary.Id
    }
    if (-not $dialog) {
        throw "The simulated missing runtime did not produce the native WebView2 error dialog."
    }

    $ok = $dialog.FindFirst(
        [Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::AutomationIdProperty,
            "CommandButton_1"
        )
    )
    if (-not $ok) {
        throw "The WebView2 error dialog did not expose its semantic OK control."
    }
    $ok.SetFocus()
    [Windows.Forms.SendKeys]::SendWait("{ENTER}")

    $exitDeadline = [DateTime]::UtcNow.AddSeconds($ExitTimeoutSeconds)
    do {
        Start-Sleep -Milliseconds 250
        $primary.Refresh()
    } while (-not $primary.HasExited -and [DateTime]::UtcNow -lt $exitDeadline)

    if (-not $primary.HasExited) {
        throw "Beakr remained alive and headless after the fatal WebView2 error."
    }
    if ($primary.ExitCode -eq 0) {
        throw "Beakr exited successfully after a fatal WebView2 error; expected a non-zero code."
    }

    $logDirectory = Join-Path $env:LOCALAPPDATA "com.thebeakr.desktop\logs"
    $logs = @(
        Get-ChildItem -LiteralPath $logDirectory -File -ErrorAction SilentlyContinue |
            Where-Object { $_.LastWriteTime -ge $testStartedAt.AddSeconds(-2) } |
            Sort-Object LastWriteTime -Descending
    )
    if ($logs.Count -eq 0) {
        throw "No Beakr diagnostic log was written under $logDirectory."
    }
    $combinedLog = ($logs | ForEach-Object {
        Get-Content -LiteralPath $_.FullName -Raw -ErrorAction SilentlyContinue
    }) -join "`n"
    if ($combinedLog -notmatch "Failed to create settings window") {
        throw "Beakr logs did not record the fatal settings-window creation failure."
    }
    if ($combinedLog -notmatch [regex]::Escape($missingFolder)) {
        throw "Beakr logs did not record this run's unique missing-runtime path."
    }

    Write-Output "PASS: missing WebView2 produced a logged, non-zero process exit."
}
finally {
    Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $resolvedExecutable } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    if ($null -eq $previousWebViewFolder) {
        Remove-Item Env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER -ErrorAction SilentlyContinue
    }
    else {
        $env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = $previousWebViewFolder
    }
}
