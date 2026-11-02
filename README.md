# Sleuth

Internal tool of Dexterity Design Services Pvt. Ltd. Finds any tag number or text inside drawings and documents, including scanned PDFs, across local disks and mapped network drives. It shows the page with the match marked and opens the file.

## For users

1. Download `Sleuth_x.y.z_x64-setup.exe` from the [latest release](https://github.com/dexterity-admin/Sleuth/releases/latest) and run it. It installs for your Windows account only; no administrator rights are needed.

   Windows may show "Windows protected your PC" the first time, because the installer is not code-signed. Click **More info**, then **Run anyway**. Later updates install from inside Sleuth and do not show this.
2. Sleuth uses the shared reading cache at `U:\Sleuth-cache`, so a drawing read on one PC is not read again on another. If that folder doesn't work on your PC, Sleuth asks you to choose one until it does.
3. Under **Folders being read**, add the project folders whose PDFs should be searchable. Sleuth reads them in the background and pauses while you are busy.
4. Type a tag, for example `10"-P-1001-A1B` or `PSV1203`. Results show how they matched:
   - **Exact**: the text as typed.
   - **Separators ignored**: the same letters and digits, written with different dashes, quotes or spaces.
   - **Verify, OCR unsure**: the reading was uncertain, or a letter could be a digit (O/0, l/1, S/5, B/8). Check it on the preview.
5. If you don't know where a file is, press **Deep search**. It walks every ticked drive, and reads any PDFs it finds that were not read yet.

Settings live in `%APPDATA%\Sleuth\settings.json`. The index, OCR models and logs are in `%LOCALAPPDATA%\Sleuth\`. Updates never touch either folder.

## For developers

Requirements are Windows 10/11, Rust (stable, MSVC), Node 20+ and the Visual Studio C++ build tools. No keys, secrets or tokens are needed anywhere.

```powershell
./scripts/build.ps1          # tests, worker, installer -> target\release\bundle\nsis
cargo test -p sleuth-core -p sleuth-engine
cargo run --release -p sleuth-engine --example probe -- "path\to\drawing.pdf"   # read one PDF and print lines
```

Set `SLEUTH_DATA_DIR` to keep a test run's settings, index and logs in one folder instead of AppData.

| Path | What it is |
|---|---|
| `crates/sleuth-core` | Tag matcher, drive-letter/UNC map, settings, IPC protocol |
| `crates/sleuth-engine` | PDF text layer, OCR, SQLite/FTS5 index, drive walker, search service, shared cache |
| `apps/worker` | `sleuth-worker.exe`: background reader and index, named-pipe server |
| `apps/desktop` | `Sleuth.exe`: Tauri 2 window (Workbench UI in `ui/`) and the in-app updater |
| `scripts` | Build, vendor download, versioning, update manifest |
| `docs/ARCHITECTURE.md` | Design notes |

### Releases and updates

Everything runs on GitHub Actions with its built-in token.

- **Every push or pull request** builds and tests the code, and uploads the installer as a workflow artifact.
- **A push to `main`** also replaces the `dev` pre-release, for the Development update channel.
- **A tag `vX.Y.Z`** that matches the version in `Cargo.toml` publishes a release, for the Stable channel.

To release:

1. Bump `version` in `Cargo.toml`.
2. Add a `## X.Y.Z` section to `CHANGELOG.md`.
3. Commit, push, then tag `vX.Y.Z` and push the tag.

Each release carries `latest.json` with the installer's SHA-256. Sleuth checks for updates at start-up and every 6 hours. It downloads the installer over HTTPS and runs it only if the checksum matches.
