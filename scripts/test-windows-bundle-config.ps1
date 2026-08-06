param(
    [string]$ConfigPath = (Join-Path $PSScriptRoot "..\src-tauri\tauri.conf.json"),
    [string]$CargoPath = (Join-Path $PSScriptRoot "..\src-tauri\Cargo.toml")
)

$ErrorActionPreference = "Stop"

$config = Get-Content -LiteralPath $ConfigPath -Raw | ConvertFrom-Json
$installMode = $config.bundle.windows.webviewInstallMode.type
if ($installMode -notin @("embedBootstrapper", "offlineInstaller")) {
    throw "Expected an explicit embedded WebView2 install mode; found '$installMode'."
}

$cargo = Get-Content -LiteralPath $CargoPath -Raw
if ($cargo -notmatch '(?m)^tauri-plugin-log\s*=') {
    throw "Cargo.toml does not include tauri-plugin-log."
}

Write-Output "PASS: WebView2 bundle mode is explicit and persistent logging is enabled."
