# Backing up the office index to GitHub

The office index (`U:\Sleuth-cache`) is what every PC searches. These scripts
keep a byte-identical copy of it in a **private** GitHub repository and put
back anything that goes missing or is damaged.

Why releases and not git: git refuses files over 100 MB and keeps every
version for ever. Index parts are SQLite files that never change once
written, so each is stored once, as a release asset (up to 2 GB each; larger
ones are split into 1.9 GB pieces), and checked by its SHA-256 from then on.

| File | Runs on | Does |
|---|---|---|
| `sleuth-index-backup.ps1` | the server PC, daily (Task Scheduler) | checks every backed-up part is still identical on the share (size + SHA-256), repairs a damaged one with `-Repair`, backs up new parts (copy, `sleuth-server check-parts`, hash, upload, download again and compare), snapshots the catalog (last 14 kept), writes `manifest.json` |
| `sleuth-index-restore.ps1` | any PC with `gh` | puts back every part that is missing or differs, verified, the way Sleuth publishes a part (`.tmp`, then renamed), so running PCs pick it up by themselves |
| `verify-index-backup.yml` | GitHub Actions in the backup repository | on every manifest and daily: every asset present with the recorded size, backup fresh (< 3 days); Sundays and on request: every asset downloaded and its SHA-256 compared. A failed run is e-mailed to the owner |

## Setting it up (once)

1. Create a **private** repository, e.g. `dexterity-admin/sleuth-index-backup`
   (the scripts refuse a public one). Put `verify-index-backup.yml` in its
   `.github/workflows/`.
2. On the server PC, sign `gh` in as dexterity-admin (`gh auth status`).
3. Run once by hand (first run uploads everything; check the log in the
   staging folder):

   ```
   powershell -ExecutionPolicy Bypass -File "C:\Program Files\Sleuth Server\backup\sleuth-index-backup.ps1" -IndexDir U:\Sleuth-cache -Repo dexterity-admin/sleuth-index-backup -Staging D:\SleuthBackup -Repair
   ```

4. Schedule the same command daily (Task Scheduler, run whether signed in
   or not, as the account `gh` is signed in for). Use the network path
   (`\\192.168.0.200\free exchange - updated\Sleuth-cache`) if the task runs
   without U: mapped.

## Putting it back

```
powershell -ExecutionPolicy Bypass -File sleuth-index-restore.ps1 -IndexDir U:\Sleuth-cache -Repo dexterity-admin/sleuth-index-backup -Staging D:\SleuthBackup
```

Only what is missing or differs is downloaded. `-Only name.db,...` limits it
to some parts; `-Catalog` also unpacks the newest catalog snapshot into
`catalog.restored` (swap it in with Sleuth stopped on every PC; the catalog
is rebuilt by listing the drives anyway).

## Limits to know

- Storage: GitHub does not charge for release assets, but very large repos
  may draw attention under its acceptable-use terms. The verify summary shows
  the total; parts merged away are kept (marked retired) as history.
- The backup is as fresh as its last run (daily). Readings made since are on
  the PCs that made them and are published again by them.
- A part missing from the share is usually merged away (its `.retired` note
  goes after a day); it is reported, and put back only with `-RepairMissing`.
