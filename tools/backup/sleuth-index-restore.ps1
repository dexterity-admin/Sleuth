<#
.SYNOPSIS
  Put the Sleuth office index back from its GitHub backup, verified byte for byte.

.DESCRIPTION
  Reads manifest.json from the backup repository and, for every live part
  (or only those named with -Only), downloads its asset(s) into a staging
  folder, joins pieces, checks size and SHA-256 against the manifest and that
  the file is a whole SQLite database, then places it in <IndexDir>\text as
  name.tmp and renames it to name.db - the way Sleuth itself publishes a
  part, so running PCs pick it up by themselves.

  A part already on the share with the right SHA-256 is left alone, so this
  can be run at any time: it only puts back what is missing or damaged.
  -Catalog also puts back the newest catalog snapshot (into catalog.restored;
  rename it over catalog with Sleuth stopped on every PC).

.EXAMPLE
  .\sleuth-index-restore.ps1 -IndexDir U:\Sleuth-cache -Repo dexterity-admin/sleuth-index-backup -Staging D:\SleuthBackup
#>
param(
  [Parameter(Mandatory)] [string] $IndexDir,
  [Parameter(Mandatory)] [string] $Repo,
  [Parameter(Mandatory)] [string] $Staging,
  [string[]] $Only = @(),
  [string] $SleuthServer = "$env:ProgramFiles\Sleuth Server\sleuth-server.exe",
  [switch] $Catalog
)
$ErrorActionPreference = 'Stop'
# gh writes "not found" and the like to stderr; Windows PowerShell 5.1 would
# make that fatal under 'Stop'. Exit codes are checked instead.
function Gh {
  $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
  try { & gh.exe @args } finally { $ErrorActionPreference = $old }
}
function Fail($m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }
function Sha($p) { (Get-FileHash -Algorithm SHA256 -LiteralPath $p).Hash.ToLower() }
if ($Staging -match '^[cC]:') { Fail "the staging folder must not be on C:" }
$text = Join-Path $IndexDir 'text'
New-Item -ItemType Directory -Force -Path $text | Out-Null
$work = Join-Path $Staging 'restore'; New-Item -ItemType Directory -Force -Path $work | Out-Null

$raw = Gh api "repos/$Repo/contents/manifest.json" -H "Accept: application/vnd.github.raw"
if ($LASTEXITCODE -ne 0) { Fail "cannot read manifest.json from $Repo" }
$m = ($raw -join "`n") | ConvertFrom-Json

# Download an entry's asset(s), join, verify; returns the local file.
function Fetch($entry, $name) {
  $dst = Join-Path $work $name
  Remove-Item -LiteralPath $dst -ErrorAction SilentlyContinue
  $out = [IO.File]::Create($dst)
  try {
    foreach ($pc in $entry.pieces) {
      $p = Join-Path $work ("dl-" + $pc.asset)
      Gh release download $entry.release -R $Repo -p $pc.asset -O $p --clobber | Out-Null
      if ($LASTEXITCODE -ne 0) { throw "download of $($pc.asset) failed" }
      if ((Get-Item $p).Length -ne [int64]$pc.size -or (Sha $p) -ne $pc.sha256) { throw "$($pc.asset) does not match the manifest" }
      $in = [IO.File]::OpenRead($p); try { $in.CopyTo($out) } finally { $in.Close() }
      Remove-Item -LiteralPath $p
    }
  } finally { $out.Close() }
  if ((Get-Item $dst).Length -ne [int64]$entry.size -or (Sha $dst) -ne $entry.sha256) { throw "$name does not match the manifest after joining" }
  return $dst
}

$bad = 0; $put = 0; $same = 0
foreach ($prop in $m.parts.PSObject.Properties) {
  $name = $prop.Name; $e = $prop.Value
  if ($Only.Count) { if ($Only -notcontains $name) { continue } } elseif ($e.retired) { continue }
  $target = Join-Path $text $name
  if ((Test-Path -LiteralPath $target) -and (Get-Item -LiteralPath $target).Length -eq [int64]$e.size -and (Sha $target) -eq $e.sha256) { $same++; continue }
  try {
    $f = Fetch $e $name
    if (Test-Path -LiteralPath $SleuthServer) {
      $chk = & $SleuthServer check-parts $f
      if ($LASTEXITCODE -ne 0) { throw "not a whole database: $chk" }
    }
    $tmp = [IO.Path]::ChangeExtension($target, 'tmp')
    Copy-Item -LiteralPath $f -Destination $tmp -Force
    if ((Sha $tmp) -ne $e.sha256) { throw "the copy on the share differs" }
    if (Test-Path -LiteralPath $target) { Move-Item -LiteralPath $target -Destination ([IO.Path]::ChangeExtension($target, ("damaged-{0:yyyyMMddHHmmss}" -f (Get-Date)))) -Force }
    Move-Item -LiteralPath $tmp -Destination $target
    Remove-Item -LiteralPath $f
    $put++; Write-Host "restored $name"
  } catch { $bad++; Write-Host "FAILED $name : $_" -ForegroundColor Red }
}
if ($Catalog -and $m.catalogs) {
  $c = @($m.catalogs | Sort-Object release)[-1]
  try {
    $zip = Fetch $c "$($c.release).zip"
    $dst = Join-Path $IndexDir 'catalog.restored'
    Remove-Item -LiteralPath $dst -Recurse -ErrorAction SilentlyContinue
    Expand-Archive -LiteralPath $zip -DestinationPath $dst
    Write-Host "catalog $($c.release) put in $dst"
  } catch { $bad++; Write-Host "FAILED catalog: $_" -ForegroundColor Red }
}
Write-Host "restored $put, already identical $same, failed $bad"
if ($bad) { exit 1 }
