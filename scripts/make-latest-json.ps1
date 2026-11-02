# Write the update manifest (latest.json) for one built installer. The app
# downloads the installer from DownloadUrl and checks it against this SHA-256.
param(
  [Parameter(Mandatory)][string]$Version,
  [Parameter(Mandatory)][string]$Installer,
  [Parameter(Mandatory)][string]$DownloadUrl,
  [string]$Notes = '',
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
$manifest | ConvertTo-Json | Set-Content -Encoding utf8 $Out
Write-Host "Wrote $Out"
