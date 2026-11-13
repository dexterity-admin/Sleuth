<#
.SYNOPSIS
  Make this PC (the office server PC, on all day) the one that backs up the
  Sleuth office index to GitHub. Run once, signed in at that PC.

.DESCRIPTION
  Expects the backup tools in <Home>\bin (gh.exe, sleuth-server.exe and the
  backup scripts). It:
    1. signs gh in to GitHub, if it is not already: paste the classic token
       (scopes repo and workflow) when gh asks; it is kept in this Windows
       user's Credential Manager, not in any file;
    2. checks the account can open the PRIVATE backup repository;
    3. registers the daily task "Sleuth index backup" (02:30, and at start-up
       when one was missed) to run whether or not anyone is signed in.
       Windows needs this account's password for that: type it in the
       Windows prompt; it is given to Task Scheduler and kept by Windows;
    4. writes backup-host.txt = this PC's name in the repository, so the
       task on any other PC stops doing backups;
    5. runs the task once now, and shows where its log is.

.EXAMPLE
  Right-click > Run with PowerShell, or:
  powershell -ExecutionPolicy Bypass -File D:\SleuthBackup\bin\setup-backup-host.ps1
#>
param(
  [string] $Home_ = 'D:\SleuthBackup',
  [string] $Repo = 'dexterity-admin/sleuth-index-backup',
  [string] $IndexDir = '\\192.168.0.200\free exchange - updated\Sleuth-cache',
  [string] $At = '02:30'
)
$ErrorActionPreference = 'Stop'
$bin = Join-Path $Home_ 'bin'
$gh = Join-Path $bin 'gh.exe'
function Step($m) { Write-Host ""; Write-Host "== $m" -ForegroundColor Cyan }
function Fail($m) { Write-Host "ERROR: $m" -ForegroundColor Red; Read-Host "Press Enter to close"; exit 1 }
function Gh { $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'; try { & $gh @args 2>$null } finally { $ErrorActionPreference = $old } }

# Administrator rights are needed to register a task that runs without sign-in.
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
  Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`""
  exit 0
}
foreach ($f in 'gh.exe', 'sleuth-server.exe', 'sleuth-index-backup.ps1', 'sleuth-index-restore.ps1') {
  if (-not (Test-Path -LiteralPath (Join-Path $bin $f))) { Fail "$f is missing from $bin" }
}
# The backup scripts call `gh`: put this folder first on PATH for them.
$env:PATH = "$bin;$env:PATH"

Step "1. GitHub sign-in"
Gh auth status --hostname github.com | Out-Null
if ($LASTEXITCODE -ne 0) {
  Write-Host "Copy the classic token (scopes: repo, workflow) with Ctrl+C - do NOT paste it here."
  Write-Host "Then press Enter. The token is read from the clipboard, never shown, and the clipboard is cleared."
  Write-Host "It is stored in Windows Credential Manager for $env:USERDOMAIN\$env:USERNAME."
  # gh --with-token reads stdin to end-of-input, so Enter alone never finishes it at a console,
  # and a masked Read-Host does not take a paste in the classic console. So: clipboard, then a closed pipe.
  $tok = $null
  for ($try = 1; $try -le 3; $try++) {
    [void](Read-Host "Press Enter once the token is copied")
    $tok = "$(Get-Clipboard -Raw)".Trim()
    if ($tok -match '^(ghp_|github_pat_)[A-Za-z0-9_]{20,}$') { break }
    Write-Host "The clipboard does not hold a GitHub token (it should start with ghp_). Copy it again." -ForegroundColor Yellow
    $tok = $null
  }
  if (-not $tok) { Fail "no token found on the clipboard" }
  try { Set-Clipboard -Value ' ' } catch {}
  $tok | & $gh auth login --hostname github.com --git-protocol https --with-token
  $tok = $null
  if ($LASTEXITCODE -ne 0) { Fail "gh did not accept the token" }
}
$who = Gh api user -q .login
Write-Host "Signed in to GitHub as $who"

Step "2. Backup repository"
$vis = Gh repo view $Repo --json visibility -q .visibility
if ($LASTEXITCODE -ne 0) { Fail "$who cannot open $Repo" }
if ($vis -ne 'PRIVATE') { Fail "$Repo is $vis; it must stay PRIVATE" }
Write-Host "$Repo is private and reachable."
if (-not (Test-Path -LiteralPath (Join-Path $IndexDir 'text'))) { Fail "the office index $IndexDir cannot be opened from this PC" }

Step "3. Daily task"
$user = "$env:USERDOMAIN\$env:USERNAME"
$cred = Get-Credential -UserName $user -Message "Windows password of $user, so the backup runs while nobody is signed in (kept by Task Scheduler)."
if (-not $cred) { Fail "no password given; the task was not registered" }
$arg = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command `"`$env:PATH='$bin;'+`$env:PATH; & '$bin\sleuth-index-backup.ps1' -IndexDir '$IndexDir' -Repo '$Repo' -Staging '$Home_' -SleuthServer '$bin\sleuth-server.exe' -Repair`""
$action = New-ScheduledTaskAction -Execute powershell.exe -Argument $arg
$triggers = @((New-ScheduledTaskTrigger -Daily -At $At))
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Hours 12) -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 30)
Register-ScheduledTask -TaskName 'Sleuth index backup' -Action $action -Trigger $triggers -Settings $settings -User $cred.UserName -Password $cred.GetNetworkCredential().Password -RunLevel Highest -Force `
  -Description "Backs up the Sleuth office index to the private GitHub repository $Repo." | Out-Null
Write-Host "Task 'Sleuth index backup' runs daily at $At (and later if the PC was off then)."

Step "4. This PC does the backups"
$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("$env:COMPUTERNAME`n"))
$sha = Gh api "repos/$Repo/contents/backup-host.txt" -q .sha
$body = @{ message = "Backups run on $env:COMPUTERNAME"; content = $b64 }
if ($LASTEXITCODE -eq 0 -and $sha) { $body.sha = "$sha".Trim() }
$tmp = Join-Path $Home_ 'host-put.json'
[IO.File]::WriteAllText($tmp, ($body | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
Gh api -X PUT "repos/$Repo/contents/backup-host.txt" --input $tmp | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "could not write backup-host.txt" }
Remove-Item -LiteralPath $tmp
Write-Host "backup-host.txt = $env:COMPUTERNAME; the task on other PCs now does nothing."

Step "5. First run"
Start-ScheduledTask -TaskName 'Sleuth index backup'
Write-Host "Started. Its log: $Home_\backup-<date>.log (a first run after another PC may upload many parts)."
Write-Host ""
Read-Host "Done. Press Enter to close"
