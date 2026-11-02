# Full build: tests, worker, then the NSIS installer. Needs no keys or secrets.
param([switch]$SkipTests)
# Native tools (cargo, npx) report progress on stderr; Windows PowerShell 5.1
# would turn that into terminating errors, so failures are detected through
# exit codes instead.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

& "$PSScriptRoot\fetch-vendor.ps1"

if (-not $SkipTests) {
  cargo test --release -p sleuth-core -p sleuth-engine
  if ($LASTEXITCODE) { throw 'tests failed' }
}

cargo build --release -p sleuth-worker
if ($LASTEXITCODE) { throw 'worker build failed' }
$bin = 'apps\desktop\src-tauri\binaries'
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item -Force 'target\release\sleuth-worker.exe' "$bin\sleuth-worker-x86_64-pc-windows-msvc.exe"

Push-Location 'apps\desktop\src-tauri'
try {
  npx --yes @tauri-apps/cli@2.12.4 build
  if ($LASTEXITCODE) { throw 'installer build failed' }
} finally { Pop-Location }

Get-ChildItem 'target\release\bundle\nsis' | Select-Object Name, Length
