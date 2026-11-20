# Builds the portable zip from an existing release build:
#   cargo build --release; ./scripts/package.ps1
# The zip contains a `portable` marker file, so settings stay in a `data`
# folder next to gitust.exe (the GitHub token still goes to the OS vault).
param([string]$Version = "", [string]$Out = "dist")
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
if (-not $Version) {
    $Version = (cargo metadata --no-deps --format-version 1 --manifest-path "$root\Cargo.toml" | ConvertFrom-Json).packages[0].version
}
$exe = Join-Path $root "target\release\gitust.exe"
if (-not (Test-Path $exe)) { throw "Build first: cargo build --release" }

$outDir = Join-Path $root $Out
$stage = Join-Path $outDir "Gitust-$Version"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage
Copy-Item (Join-Path $root "README.md") $stage
Set-Content -Path (Join-Path $stage "portable") -Encoding utf8 -Value "While this file exists, Gitust keeps its settings in the 'data' folder next to gitust.exe. Delete it to use your user profile instead."

$zip = Join-Path $outDir "Gitust-$Version-windows-x64-portable.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host "Created $zip"
