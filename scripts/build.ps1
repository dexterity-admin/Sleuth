# Full build: tests, worker, office server, then the NSIS installer and the
# server package. Needs no keys or secrets.
param([switch]$SkipTests)
# Native tools (cargo, npx) report progress on stderr; Windows PowerShell 5.1
# would turn that into terminating errors, so failures are detected through
# exit codes instead.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

& "$PSScriptRoot\fetch-vendor.ps1"

if (-not $SkipTests) {
  cargo test --release --workspace --exclude sleuth
  if ($LASTEXITCODE) { throw 'tests failed' }
}

cargo build --release -p sleuth-worker -p sleuth-server
if ($LASTEXITCODE) { throw 'worker or server build failed' }
$bin = 'apps\desktop\src-tauri\binaries'
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item -Force 'target\release\sleuth-worker.exe' "$bin\sleuth-worker-x86_64-pc-windows-msvc.exe"

# DLLs the OCR runtime loads, shipped next to the worker so a PC without the
# Visual C++ runtime, or with an older DirectML, still works: DirectML from the
# ONNX Runtime build, and the Visual C++ runtime (app-local deployment).
$runtime = 'apps\desktop\src-tauri\runtime'
New-Item -ItemType Directory -Force $runtime | Out-Null
Copy-Item -Force 'target\release\DirectML.dll' "$runtime\DirectML.dll"
foreach ($dll in 'msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
  $src = Join-Path $env:SystemRoot "System32\$dll"
  if (-not (Test-Path $src)) { throw "$dll not found on the build machine" }
  Copy-Item -Force $src "$runtime\$dll"
}

Push-Location 'apps\desktop\src-tauri'
try {
  npx --yes @tauri-apps/cli@2.12.4 build
  if ($LASTEXITCODE) { throw 'installer build failed' }
} finally { Pop-Location }

& "$PSScriptRoot\package-server.ps1" -Runtime $runtime
if ($LASTEXITCODE) { throw 'server package failed' }

Get-ChildItem 'target\release\bundle\nsis', 'out' -ErrorAction SilentlyContinue | Select-Object Name, Length
