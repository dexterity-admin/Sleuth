# Downloads the third-party runtime files Sleuth ships with (PDFium and the
# PP-OCRv6 standard models) into .\vendor, checking each against a pinned
# SHA-256. Safe to run repeatedly; files already present and valid are kept.
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
Write-Host 'Vendor files ready.'
