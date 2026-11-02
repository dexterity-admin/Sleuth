# Set the workspace version (used by CI for development builds).
param([Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'
$toml = Join-Path (Split-Path -Parent $PSScriptRoot) 'Cargo.toml'
$text = Get-Content -Raw $toml
$text = $text -replace '(?m)^(\[workspace\.package\][\s\S]*?^version\s*=\s*)"[^"]*"', "`$1`"$Version`""
Set-Content -NoNewline -Encoding utf8 $toml $text
Write-Host "Version set to $Version"
