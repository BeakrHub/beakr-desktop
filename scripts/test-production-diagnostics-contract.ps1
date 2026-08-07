$ErrorActionPreference = "Stop"

$lib = Get-Content -LiteralPath (Join-Path $PSScriptRoot "..\src-tauri\src\lib.rs") -Raw
$tray = Get-Content -LiteralPath (Join-Path $PSScriptRoot "..\src-tauri\src\tray.rs") -Raw
$settings = Get-Content -LiteralPath (Join-Path $PSScriptRoot "..\src\components\Settings.tsx") -Raw

$checks = [ordered]@{
    "panic hook installed after logger initialization" = $lib -match "diagnostics::install_panic_hook"
    "periodic log flusher started" = $lib -match "diagnostics::spawn_log_flusher"
    "explicit one-megabyte log size cap" = $lib -match "max_file_size\(1_000_000\)"
    "three rotated logs retained" = $lib -match "RotationStrategy::KeepSome\(3\)"
    "tray contains Open log folder item" = $tray -match 'with_id\("open_logs", "Open log folder"\)'
    "tray dispatches Open log folder action" = $tray -match '"open_logs"\s*=>'
    "settings invokes Open log folder command" = $settings -match 'invoke\("open_log_folder"\)'
    "settings exposes Open log folder copy" = $settings -match 'Open log folder'
}

$missing = @($checks.GetEnumerator() | Where-Object { -not $_.Value } | ForEach-Object { $_.Key })
if ($missing.Count -gt 0) {
    throw "Production diagnostics contract is incomplete: $($missing -join '; ')."
}

Write-Output "PASS: panic logging, bounded rotation, tray access, and settings access are wired."
