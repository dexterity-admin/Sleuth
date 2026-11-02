# Write the update manifest (latest.json) for one built installer. The app
# downloads the installer from DownloadUrl and checks it against this SHA-256.
# With -Previous (the latest.json of the release before), the manifest also
# names that release, so the app can offer a rollback to it.
param(
  [Parameter(Mandatory)][string]$Version,
  [Parameter(Mandatory)][string]$Installer,
  [Parameter(Mandatory)][string]$DownloadUrl,
  [string]$Notes = '',
  [string]$Previous = '',
  [string]$Out = 'latest.json'
)
$ErrorActionPreference = 'Stop'
$manifest = [ordered]@{
  version  = $Version
  notes    = $Notes
  url      = $DownloadUrl
  sha256   = (Get-FileHash -Algorithm SHA256 $Installer).Hash.ToLower()
  pub_date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
}
if ($Previous -and (Test-Path $Previous)) {
  $p = Get-Content -Raw $Previous | ConvertFrom-Json
  # Only a release with a checksum can be offered; never the same version.
  if ($p.version -and $p.url -and $p.sha256 -and $p.version -ne $Version) {
    $manifest.previous = [ordered]@{ version = $p.version; url = $p.url; sha256 = $p.sha256 }
  }
}
$manifest | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 $Out
Write-Host "Wrote $Out"
