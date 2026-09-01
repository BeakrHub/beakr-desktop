param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot "..\src-tauri\target\release\beakr-desktop.exe"),
    [int]$MinimumLifetimeSeconds = 60
)

$ErrorActionPreference = "Stop"

if ($env:GITHUB_ACTIONS -ne "true") {
    throw "This smoke wrapper seeds the app store and may only run on an isolated GitHub Actions runner."
}

$storeDirectory = Join-Path $env:APPDATA "com.thebeakr.desktop"
$storePath = Join-Path $storeDirectory "settings.json"
if (Test-Path -LiteralPath $storePath) {
    throw "Refusing to overwrite an existing Beakr settings store on the CI runner."
}

$createdDirectory = -not (Test-Path -LiteralPath $storeDirectory)
try {
    New-Item -ItemType Directory -Path $storeDirectory -Force | Out-Null
    $storeJson = @{ device_token = "ci-paired-smoke-token" } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($storePath, $storeJson, [Text.UTF8Encoding]::new($false))

    & (Join-Path $PSScriptRoot "test-windows-paired-lifetime.ps1") `
        -ExecutablePath $ExecutablePath `
        -MinimumLifetimeSeconds $MinimumLifetimeSeconds
}
finally {
    Remove-Item -LiteralPath $storePath -Force -ErrorAction SilentlyContinue
    if (
        $createdDirectory -and
        (Test-Path -LiteralPath $storeDirectory) -and
        @(Get-ChildItem -LiteralPath $storeDirectory -Force).Count -eq 0
    ) {
        Remove-Item -LiteralPath $storeDirectory -Force -ErrorAction SilentlyContinue
    }
}
