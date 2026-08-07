param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe")
)

$ErrorActionPreference = "Stop"

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "This packaged diagnostics regression currently drives Windows UI Automation."
}

$resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
if ($resolvedExecutable -notmatch "[\\/]release[\\/]") {
    throw "ENG-1967 diagnostics actions must be tested with a release-profile executable."
}
if (Get-Process -Name "beakr-desktop" -ErrorAction SilentlyContinue) {
    throw "Stop the existing Beakr Desktop instance before running this test."
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class BeakrDiagnosticsMouse {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint x, uint y, uint data, UIntPtr extra);
}
"@

function Get-AllUiElements {
    return [Windows.Automation.AutomationElement]::RootElement.FindAll(
        [Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.Condition]::TrueCondition
    )
}

function Find-VisibleTrayElements([string]$NamePrefix) {
    $all = Get-AllUiElements
    $matches = [Collections.Generic.List[object]]::new()
    for ($i = 0; $i -lt $all.Count; $i++) {
        $element = $all.Item($i)
        if (
            $element.Current.Name -like "$NamePrefix*" -and
            $element.Current.ClassName -like "SystemTray.*" -and
            -not $element.Current.IsOffscreen
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

function Click-TrayElement($Element, [switch]$RightButton) {
    $bounds = $Element.Current.BoundingRectangle
    $x = [int]($bounds.Left + ($bounds.Width / 2))
    $y = [int]($bounds.Top + ($bounds.Height / 2))
    [void][BeakrDiagnosticsMouse]::SetCursorPos($x, $y)
    if ($RightButton) {
        [BeakrDiagnosticsMouse]::mouse_event(8, 0, 0, 0, [UIntPtr]::Zero)
        [BeakrDiagnosticsMouse]::mouse_event(16, 0, 0, 0, [UIntPtr]::Zero)
    }
    else {
        [BeakrDiagnosticsMouse]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
        [BeakrDiagnosticsMouse]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
    }
}

function Find-VisibleElement([Windows.Automation.ControlType]$ControlType, [string]$Name) {
    $all = Get-AllUiElements
    $matches = [Collections.Generic.List[object]]::new()
    for ($i = 0; $i -lt $all.Count; $i++) {
        $element = $all.Item($i)
        if (
            $element.Current.ControlType -eq $ControlType -and
            $element.Current.Name -eq $Name -and
            -not $element.Current.IsOffscreen
        ) {
            $matches.Add($element)
        }
    }
    return $matches
}

function Get-ExplorerLogWindows($Shell, [string]$LogDirectory) {
    $matches = [Collections.Generic.List[object]]::new()
    foreach ($window in @($Shell.Windows())) {
        try {
            if (-not $window.LocationURL) {
                continue
            }
            $location = ([uri]$window.LocationURL).LocalPath.TrimEnd("\")
            if ($location -ieq $LogDirectory.TrimEnd("\")) {
                $matches.Add($window)
            }
        }
        catch {
            # Ignore non-Explorer shell windows that do not expose LocationURL.
        }
    }
    return $matches
}

$logDirectory = Join-Path $env:LOCALAPPDATA "com.thebeakr.desktop\logs"
$shell = New-Object -ComObject Shell.Application
$beforeExplorerHandles = @(
    Get-ExplorerLogWindows $shell $logDirectory | ForEach-Object { $_.HWND }
)
$process = $null

try {
    $process = Start-Process -FilePath $resolvedExecutable -PassThru
    Start-Sleep -Seconds 6
    $process.Refresh()
    if ($process.HasExited) {
        throw "Diagnostics release exited during startup (exit code $($process.ExitCode))."
    }

    $startupMarker = "startup diagnostics initialized (process_id=$($process.Id))"
    $startupLog = Get-ChildItem -LiteralPath $logDirectory -File -ErrorAction SilentlyContinue |
        Where-Object {
            (Get-Content -LiteralPath $_.FullName -Raw -ErrorAction SilentlyContinue) -match [regex]::Escape($startupMarker)
        } |
        Select-Object -First 1
    if (-not $startupLog) {
        throw "Release startup did not flush its process-specific marker to a Beakr log file."
    }

    $trayElements = @(Find-VisibleTrayElements "Beakr Desktop")
    if ($trayElements.Count -eq 0) {
        $chevrons = @(Find-VisibleTrayElements "Show Hidden Icons")
        if ($chevrons.Count -ne 1) {
            throw "Expected one visible notification overflow control; found $($chevrons.Count)."
        }
        Invoke-Element $chevrons[0] "Show Hidden Icons"
        Start-Sleep -Seconds 1
        $trayElements = @(Find-VisibleTrayElements "Beakr Desktop")
    }
    if ($trayElements.Count -ne 1) {
        throw "Expected one visible Beakr tray icon; found $($trayElements.Count)."
    }

    Click-TrayElement $trayElements[0] -RightButton
    Start-Sleep -Seconds 1
    $openLogItems = @(Find-VisibleElement ([Windows.Automation.ControlType]::MenuItem) "Open log folder")
    if ($openLogItems.Count -ne 1) {
        throw "Expected one Open log folder tray item; found $($openLogItems.Count)."
    }
    Invoke-Element $openLogItems[0] "Open log folder tray item"

    $explorerDeadline = [DateTime]::UtcNow.AddSeconds(10)
    $logExplorerWindows = @()
    while ([DateTime]::UtcNow -lt $explorerDeadline) {
        Start-Sleep -Milliseconds 250
        $logExplorerWindows = @(Get-ExplorerLogWindows $shell $logDirectory)
        if ($logExplorerWindows.Count -gt 0) {
            break
        }
    }
    if ($logExplorerWindows.Count -eq 0) {
        throw "Tray action did not open the Beakr log directory in Explorer."
    }

    Write-Output "PASS: startup marker was durable and the tray opened its exact log folder in Explorer."
}
finally {
    if ($process) {
        $process.Refresh()
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            Wait-Process -Id $process.Id -ErrorAction SilentlyContinue
        }
    }
    foreach ($window in @(Get-ExplorerLogWindows $shell $logDirectory)) {
        if ($window.HWND -notin $beforeExplorerHandles) {
            try { $window.Quit() } catch {}
        }
    }
}
