<#
.SYNOPSIS
  Back up the Sleuth office index to a private GitHub repository, byte for byte.

.DESCRIPTION
  The office index (e.g. U:\Sleuth-cache) is a folder of parts, text\*.db.
  A published part never changes after it is written (new readings go into
  new parts; merges write a new part and retire the old ones), so each part
  is backed up once, by name, and checked for ever after by its SHA-256.

  Each run:
    1. lists the live parts in <IndexDir>\text (not *.tmp, not retired ones);
    2. checks every part already in the manifest still has its recorded size
       and SHA-256 on the share: a difference is CORRUPTION, reported, and
       with -Repair the part is put back from GitHub (verified) at once;
    3. copies each new part to a local staging folder (never C:), checks the
       copy is a whole SQLite database (sleuth-server check-parts), hashes
       the copy, and uploads that same file as an asset of the month's
       release (parts-YYYY-MM) in the backup repository; the asset is then
       downloaded again and its hash compared, so what GitHub holds is known
       to be identical;
    4. zips the catalog folder (file lists; small, changes often) as a
       dated asset of release catalog-YYYY-MM-DD, keeping the last 14;
    5. writes manifest.json (every part: name, size, sha256, release, when)
       to the backup repository's main branch, which starts its verify
       workflow (size, presence and hash of every asset).

  Files over 1.9 GB are uploaded in 1.9 GB pieces (GitHub's limit per asset
  is 2 GB) and joined again by the restore script; the manifest keeps the
  hash of the whole file and of each piece.

  Needs: gh (GitHub CLI) signed in as dexterity-admin, with access to the
  backup repository; sleuth-server.exe (for check-parts). Nothing is
  written to C:.

.EXAMPLE
  .\sleuth-index-backup.ps1 -IndexDir U:\Sleuth-cache -Repo dexterity-admin/sleuth-index-backup -Staging D:\SleuthBackup

.EXAMPLE
  # Daily at 02:30 on the server PC (Task Scheduler), repairing what it finds:
  .\sleuth-index-backup.ps1 -IndexDir U:\Sleuth-cache -Repo dexterity-admin/sleuth-index-backup -Staging D:\SleuthBackup -Repair
#>
param(
  [Parameter(Mandatory)] [string] $IndexDir,
  [Parameter(Mandatory)] [string] $Repo,
  [Parameter(Mandatory)] [string] $Staging,
  [string] $SleuthServer = "$env:ProgramFiles\Sleuth Server\sleuth-server.exe",
  [switch] $Repair,
  # Also put back parts missing from the share (normally merged away).
  [switch] $RepairMissing,
  [int] $KeepCatalogs = 14,
  # Back up at most this many new parts in this run (0 = all); the rest next run.
  [int] $MaxNew = 0
)
$ErrorActionPreference = 'Stop'
# gh writes "not found" and the like to stderr; Windows PowerShell 5.1 would
# make that fatal under 'Stop'. Exit codes are checked instead.
function Gh {
  $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
  try { & gh.exe @args } finally { $ErrorActionPreference = $old }
}
$PieceBytes = 1900MB

function Fail($m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }
function Note($m) { Write-Host ("{0:HH:mm:ss}  {1}" -f (Get-Date), $m) }
function Sha($p) { (Get-FileHash -Algorithm SHA256 -LiteralPath $p).Hash.ToLower() }

if ($Staging -match '^[cC]:') { Fail "the staging folder must not be on C:" }
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail "gh (GitHub CLI) is not installed" }
if (-not (Test-Path -LiteralPath $SleuthServer)) { Fail "sleuth-server.exe not found at $SleuthServer (pass -SleuthServer)" }
$text = Join-Path $IndexDir 'text'
if (-not (Test-Path -LiteralPath $text)) { Fail "$text cannot be opened" }
$vis = Gh repo view $Repo --json visibility -q .visibility
if ($LASTEXITCODE -ne 0) { Fail "cannot open $Repo with gh (signed in? the repository exists?)" }
if ($vis -ne 'PRIVATE') { Fail "$Repo is $vis; the office index must only go to a PRIVATE repository" }
# One PC does the backups: the one named in backup-host.txt in the repository
# (set by setup-backup-host.ps1). Any other PC's task does nothing, so two
# PCs never upload at the same time.
$hostFile = Gh api "repos/$Repo/contents/backup-host.txt" -H "Accept: application/vnd.github.raw"
if ($LASTEXITCODE -eq 0 -and "$hostFile".Trim() -and "$hostFile".Trim() -ne $env:COMPUTERNAME) {
  Write-Host "Backups of this index are run by $("$hostFile".Trim()) (backup-host.txt); nothing done here."
  exit 0
}

New-Item -ItemType Directory -Force -Path $Staging | Out-Null
# One backup at a time (a long first run may still go when the next one is due).
try { $lock = [IO.File]::Open((Join-Path $Staging 'backup.lock'), 'OpenOrCreate', 'ReadWrite', 'None') } catch { Write-Host "another backup is running; nothing done"; exit 0 }
$work = Join-Path $Staging 'work'; New-Item -ItemType Directory -Force -Path $work | Out-Null
$log = Join-Path $Staging ("backup-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))
Start-Transcript -LiteralPath $log | Out-Null

# ---- the manifest as it is in the backup repository ----
$manifest = @{ format = 1; parts = @{}; catalogs = @() }
$raw = Gh api "repos/$Repo/contents/manifest.json" -H "Accept: application/vnd.github.raw"
if ($LASTEXITCODE -eq 0 -and $raw) {
  $m = ($raw -join "`n") | ConvertFrom-Json
  foreach ($p in $m.parts.PSObject.Properties) { $manifest.parts[$p.Name] = $p.Value }
  if ($m.catalogs) { $manifest.catalogs = @($m.catalogs) }
}
Note "manifest: $($manifest.parts.Count) part(s) backed up before"

function Ensure-Release($tag, $title) {
  Gh release view $tag -R $Repo | Out-Null
  if ($LASTEXITCODE -ne 0) { Gh release create $tag -R $Repo --title $title --notes "Sleuth office index backup. Assets are byte-identical copies; see manifest.json." | Out-Null }
}

# Upload a file (in pieces when large); download it again and compare.
function Upload-Verified($file, $tag, $assetBase) {
  $len = (Get-Item -LiteralPath $file).Length
  $pieces = @()
  if ($len -le $PieceBytes) {
    $pieces += [pscustomobject]@{ path = $file; name = $assetBase }
  } else {
    $in = [IO.File]::OpenRead($file); $buf = New-Object byte[] (8MB); $i = 0
    try {
      while ($in.Position -lt $len) {
        $pp = Join-Path $work ("{0}.part{1:D3}" -f $assetBase, $i)
        $out = [IO.File]::Create($pp); $left = [int64]$PieceBytes
        while ($left -gt 0 -and ($n = $in.Read($buf, 0, [Math]::Min($buf.Length, $left))) -gt 0) { $out.Write($buf, 0, $n); $left -= $n }
        $out.Close(); $pieces += [pscustomobject]@{ path = $pp; name = (Split-Path $pp -Leaf) }; $i++
      }
    } finally { $in.Close() }
  }
  $out = @()
  foreach ($pc in $pieces) {
    $h = Sha $pc.path
    Gh release upload $tag $pc.path -R $Repo --clobber | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "upload of $($pc.name) failed" }
    $back = Join-Path $work ("check-" + $pc.name)
    Remove-Item -LiteralPath $back -ErrorAction SilentlyContinue
    Gh release download $tag -R $Repo -p $pc.name -O $back --clobber | Out-Null
    if ($LASTEXITCODE -ne 0 -or (Sha $back) -ne $h) { throw "$($pc.name) on GitHub is not identical to the file sent" }
    Remove-Item -LiteralPath $back
    $out += [pscustomobject]@{ asset = $pc.name; size = (Get-Item -LiteralPath $pc.path).Length; sha256 = $h }
    if ($pc.path -ne $file) { Remove-Item -LiteralPath $pc.path }
  }
  return ,$out
}

# ---- 1. live parts ----
$live = Get-ChildItem -LiteralPath $text -Filter *.db -File | Where-Object { -not (Test-Path -LiteralPath ([IO.Path]::ChangeExtension($_.FullName, 'retired'))) }
Note "live parts on the share: $($live.Count) ($([math]::Round(($live | Measure-Object Length -Sum).Sum / 1GB, 2)) GB)"

# ---- 2. parts backed up before: still identical on the share? ----
$corrupt = @(); $missing = @()
foreach ($f in $live) {
  $e = $manifest.parts[$f.Name]
  if (-not $e) { continue }
  if ($f.Length -ne [int64]$e.size -or (Sha $f.FullName) -ne $e.sha256) { $corrupt += $f.Name; Write-Host "CORRUPTED on the share: $($f.Name)" -ForegroundColor Red }
}
foreach ($name in @($manifest.parts.Keys)) {
  if (-not ($live | Where-Object Name -eq $name) -and -not (Test-Path -LiteralPath (Join-Path $text ([IO.Path]::ChangeExtension($name, 'retired'))))) {
    # Gone without being retired by a merge: missing, not merged away.
    # A part merged away loses its .retired note after a day, so a missing
    # part may simply be merged: reported, put back only with -RepairMissing.
    if (-not $manifest.parts[$name].retired) { Write-Host "MISSING from the share (merged away, or deleted): $name" -ForegroundColor Yellow; $missing += $name }
  }
}
$fix = @($corrupt) + $(if ($RepairMissing) { @($missing) } else { @() })
if ($fix.Count -and $Repair) {
  Note "repairing $($fix.Count) part(s) from the backup"
  & (Join-Path $PSScriptRoot 'sleuth-index-restore.ps1') -IndexDir $IndexDir -Repo $Repo -Staging $Staging -Only $fix -SleuthServer $SleuthServer
  if ($LASTEXITCODE -ne 0) { Write-Host "REPAIR FAILED; see above" -ForegroundColor Red }
}

# ---- 3. new parts ----
$tag = "parts-{0:yyyy-MM}" -f (Get-Date)
$new = @($live | Where-Object { -not $manifest.parts.ContainsKey($_.Name) })
if ($MaxNew -gt 0) { $new = @($new | Select-Object -First $MaxNew) }
Note "new parts to back up: $($new.Count)"
if ($new.Count) { Ensure-Release $tag "Index parts $('{0:yyyy-MM}' -f (Get-Date))" }
foreach ($f in $new) {
  $copy = Join-Path $work $f.Name
  Copy-Item -LiteralPath $f.FullName -Destination $copy -Force
  $chk = & $SleuthServer check-parts $copy
  if ($LASTEXITCODE -ne 0) { Write-Host "NOT BACKED UP, not a whole database: $($f.Name): $chk" -ForegroundColor Red; Remove-Item -LiteralPath $copy; continue }
  $h = Sha $copy
  if ((Sha $f.FullName) -ne $h) { Write-Host "changed while copied, next run: $($f.Name)" -ForegroundColor Yellow; Remove-Item -LiteralPath $copy; continue }
  try { $pieces = Upload-Verified $copy $tag $f.Name } catch { Write-Host "UPLOAD FAILED: $($f.Name): $_" -ForegroundColor Red; Remove-Item -LiteralPath $copy; continue }
  $manifest.parts[$f.Name] = [pscustomobject]@{ size = $f.Length; sha256 = $h; release = $tag; pieces = $pieces; backed_up = (Get-Date).ToUniversalTime().ToString('o'); retired = $false }
  Remove-Item -LiteralPath $copy
  Note "backed up $($f.Name) ($([math]::Round($f.Length / 1MB, 1)) MB)"
}
# Parts merged away keep their backup (history), marked retired.
foreach ($name in @($manifest.parts.Keys)) {
  if (Test-Path -LiteralPath (Join-Path $text ([IO.Path]::ChangeExtension($name, 'retired')))) { $manifest.parts[$name].retired = $true }
  elseif (-not ($live | Where-Object Name -eq $name) -and -not ($RepairMissing -and $missing -contains $name)) { $manifest.parts[$name].retired = $true }
}

# ---- 4. catalog snapshot ----
$cat = Join-Path $IndexDir 'catalog'
if (Test-Path -LiteralPath $cat) {
  $ctag = "catalog-{0:yyyy-MM-dd}" -f (Get-Date)
  $zip = Join-Path $work "$ctag.zip"
  Remove-Item -LiteralPath $zip -ErrorAction SilentlyContinue
  try {
    Compress-Archive -Path (Join-Path $cat '*') -DestinationPath $zip -CompressionLevel Optimal
    Ensure-Release $ctag "Catalog $('{0:yyyy-MM-dd}' -f (Get-Date))"
    $pieces = Upload-Verified $zip $ctag "$ctag.zip"
    $manifest.catalogs = @(@($manifest.catalogs | Where-Object { $_.release -ne $ctag }) + [pscustomobject]@{ release = $ctag; size = (Get-Item $zip).Length; sha256 = (Sha $zip); pieces = $pieces })
    Remove-Item -LiteralPath $zip
    $old = @($manifest.catalogs | Sort-Object release)
    while ($old.Count -gt $KeepCatalogs) { Gh release delete $old[0].release -R $Repo --yes --cleanup-tag | Out-Null; $old = @($old | Select-Object -Skip 1) }
    $manifest.catalogs = $old
    Note "catalog snapshot $ctag backed up"
  } catch { Write-Host "catalog snapshot failed: $_" -ForegroundColor Yellow }
}

# ---- 5. manifest ----
$liveParts = @($manifest.parts.Values | Where-Object { -not $_.retired })
$doc = [ordered]@{
  format = 1
  written = (Get-Date).ToUniversalTime().ToString('o')
  index_dir = $IndexDir
  live_parts = $liveParts.Count
  live_bytes = [int64](($liveParts | Measure-Object size -Sum).Sum)
  corrupted_found = $corrupt
  missing_found = $missing
  parts = $manifest.parts
  catalogs = $manifest.catalogs
}
$json = $doc | ConvertTo-Json -Depth 8
$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
$sha = Gh api "repos/$Repo/contents/manifest.json" -q .sha
$body = @{ message = ("Backup {0:yyyy-MM-dd HH:mm}: {1} live parts" -f (Get-Date), $liveParts.Count); content = $b64 }
if ($LASTEXITCODE -eq 0 -and $sha) { $body.sha = "$sha".Trim() }
$tmp = Join-Path $work 'put.json'; # Without a byte-order mark: GitHub refuses JSON that starts with one.
[IO.File]::WriteAllText($tmp, ($body | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
Gh api -X PUT "repos/$Repo/contents/manifest.json" --input $tmp | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "could not write manifest.json" }
Remove-Item -LiteralPath $tmp
Note "manifest written: $($liveParts.Count) live parts, $([math]::Round($doc.live_bytes / 1GB, 2)) GB"
Stop-Transcript | Out-Null
if ($corrupt.Count -and -not $Repair) { exit 2 }
