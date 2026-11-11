# Downloads the third-party runtime files Sleuth ships with (PDFium, the
# PP-OCRv6 standard models and LibreDWG) into .\vendor, checking each against
# a pinned SHA-256. Safe to run repeatedly; files already present and valid
# are kept.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root 'vendor'

$pdfiumTag = 'chromium/8125'
$files = @(
  @{ Path = 'models\pp-ocrv6_small_det.onnx';          Url = 'https://github.com/GreatV/oar-ocr/releases/download/v0.7.0/pp-ocrv6_small_det.onnx';          Sha = 'd73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e' },
  @{ Path = 'models\pp-ocrv6_small_rec.onnx';          Url = 'https://github.com/GreatV/oar-ocr/releases/download/v0.7.0/pp-ocrv6_small_rec.onnx';          Sha = '5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634' },
  @{ Path = 'models\ppocrv6_dict.txt';                 Url = 'https://github.com/GreatV/oar-ocr/releases/download/v0.7.0/ppocrv6_dict.txt';                 Sha = 'b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d' },
  @{ Path = 'models\pp-lcnet_x1_0_textline_ori.onnx';  Url = 'https://github.com/GreatV/oar-ocr/releases/download/v0.3.0/pp-lcnet_x1_0_textline_ori.onnx';  Sha = '6b02efabbedd6be69e3de4c86b8dceed2d7329e75c12a796e6717bfb0d646950' }
)

function Get-Sha([string]$p) { (Get-FileHash -Algorithm SHA256 $p).Hash.ToLower() }

foreach ($f in $files) {
  $dest = Join-Path $vendor $f.Path
  if ((Test-Path $dest) -and (Get-Sha $dest) -eq $f.Sha) { continue }
  New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
  Write-Host "Downloading $($f.Path)"
  Invoke-WebRequest -Uri $f.Url -OutFile "$dest.part" -UseBasicParsing
  if ((Get-Sha "$dest.part") -ne $f.Sha) { Remove-Item "$dest.part"; throw "Checksum mismatch for $($f.Path)" }
  Move-Item -Force "$dest.part" $dest
}

$dll = Join-Path $vendor 'pdfium\bin\pdfium.dll'
if (-not (Test-Path $dll)) {
  Write-Host "Downloading PDFium $pdfiumTag"
  $dir = Join-Path $vendor 'pdfium'
  New-Item -ItemType Directory -Force $dir | Out-Null
  $tgz = Join-Path $dir 'pdfium-win-x64.tgz'
  Invoke-WebRequest -Uri "https://github.com/bblanchon/pdfium-binaries/releases/download/$pdfiumTag/pdfium-win-x64.tgz" -OutFile $tgz -UseBasicParsing
  tar -xzf $tgz -C $dir
  if (-not (Test-Path $dll)) { throw 'pdfium.dll missing after extraction' }
}

# LibreDWG (GPLv3 or later) reads .dwg files. Sleuth runs the unmodified
# dwgread.exe as a separate process; nothing of it is linked into Sleuth.
# Only the reader and the one DLL it loads are kept, with the licence and a
# note of where the binaries and their source come from.
$dwgVersion = '0.14'
$dwgRelease = "https://github.com/LibreDWG/libredwg/releases/download/$dwgVersion"
$dwgZip = "libredwg-$dwgVersion-win64.zip"
$dwgZipSha = '1ad7e15344d20b3426c3435b078d82fb84b35062815946b2cca9c5fc9810fea8'
$dwgFiles = @(
  @{ Name = 'dwgread.exe';    Sha = '4b4e829cc545b237860f8c1359a80c48bda0e6821ac71727307f58bcdc8459c0' },
  @{ Name = 'libredwg-0.dll'; Sha = 'cc86e25e755ac225eadc839eaa232c2f2233c7a1b60fcd77b2cd1915761aef12' }
)
$dwgLicence = @{ Name = 'COPYING'; Url = "https://raw.githubusercontent.com/LibreDWG/libredwg/$dwgVersion/COPYING"; Sha = '8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903' }
$dwgDir = Join-Path $vendor 'libredwg'

function Test-Sha([string]$p, [string]$sha) { (Test-Path $p) -and (Get-Sha $p) -eq $sha }

$dwgMissing = $dwgFiles | Where-Object { -not (Test-Sha (Join-Path $dwgDir $_.Name) $_.Sha) }
if ($dwgMissing) {
  Write-Host "Downloading LibreDWG $dwgVersion"
  New-Item -ItemType Directory -Force $dwgDir | Out-Null
  $zip = Join-Path $dwgDir $dwgZip
  Invoke-WebRequest -Uri "$dwgRelease/$dwgZip" -OutFile "$zip.part" -UseBasicParsing
  if ((Get-Sha "$zip.part") -ne $dwgZipSha) { Remove-Item "$zip.part"; throw "Checksum mismatch for $dwgZip" }
  Move-Item -Force "$zip.part" $zip
  tar -xf $zip -C $dwgDir @($dwgFiles | ForEach-Object { $_.Name })
  Remove-Item $zip
  foreach ($f in $dwgFiles) {
    if (-not (Test-Sha (Join-Path $dwgDir $f.Name) $f.Sha)) { throw "Checksum mismatch for $($f.Name) in $dwgZip" }
  }
}
$licence = Join-Path $dwgDir $dwgLicence.Name
if (-not (Test-Sha $licence $dwgLicence.Sha)) {
  Invoke-WebRequest -Uri $dwgLicence.Url -OutFile "$licence.part" -UseBasicParsing
  if ((Get-Sha "$licence.part") -ne $dwgLicence.Sha) { Remove-Item "$licence.part"; throw 'Checksum mismatch for the LibreDWG licence (COPYING)' }
  Move-Item -Force "$licence.part" $licence
}
@(
  "LibreDWG $dwgVersion (GNU General Public License, version 3 or later; see COPYING)",
  '',
  'These files are unmodified copies from the official Windows 64-bit release:',
  "  $dwgRelease/$dwgZip",
  "  SHA-256 $dwgZipSha",
  'Release page:',
  "  https://github.com/LibreDWG/libredwg/releases/tag/$dwgVersion",
  'Complete corresponding source code:',
  "  $dwgRelease/libredwg-$dwgVersion.tar.xz",
  '',
  'Sleuth runs dwgread.exe as a separate program to read the text of .dwg',
  'files. It is not linked into Sleuth and may be replaced by any other build',
  'of LibreDWG.'
) | Set-Content -Encoding utf8 (Join-Path $dwgDir 'SOURCE.txt')

Write-Host 'Vendor files ready.'
