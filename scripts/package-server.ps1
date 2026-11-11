# Zip the optional office server: sleuth-server.exe with the PDF and OCR files
# it needs, and its setup guide. Output: out\Sleuth-Server_<version>.zip
param([string]$Runtime = 'apps\desktop\src-tauri\runtime', [string]$Version = '')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $Version) {
  $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
}
$stage = Join-Path $root "out\server-stage"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\models" | Out-Null
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
Copy-Item "$target\release\sleuth-server.exe" $stage
Copy-Item 'vendor\pdfium\bin\pdfium.dll' $stage
Get-ChildItem $Runtime -Filter *.dll | Copy-Item -Destination $stage
foreach ($m in 'pp-ocrv6_small_det.onnx', 'pp-ocrv6_small_rec.onnx', 'ppocrv6_dict.txt', 'pp-lcnet_x1_0_textline_ori.onnx') {
  Copy-Item "vendor\models\$m" "$stage\models\$m"
}
# LibreDWG (GPLv3, unmodified, run as a separate program) reads AutoCAD files.
New-Item -ItemType Directory -Force "$stage\libredwg" | Out-Null
Copy-Item 'vendor\libredwg\*' "$stage\libredwg"
Copy-Item 'docs\SERVER.md' "$stage\README.md"
$zip = Join-Path $root "out\Sleuth-Server_${Version}_x64.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host "Wrote $zip"
