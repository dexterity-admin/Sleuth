# Sleuth architecture

Sleuth 1.0 is one Rust engine with two ways to run it:

- **Standalone** (the default): every PC indexes and reads its own drives with
  `sleuth-worker.exe`. No server is needed, and nothing stops working when the
  network does.
- **Hybrid** (optional): an office server (`sleuth-server.exe`) runs the same
  engine over shared project folders and answers desktops over HTTPS. Desktops
  keep reading their own local folders and leave the server's folders to it.

There is no AI anywhere in Sleuth: no language models, no chat, no generated
summaries, no learning from users. OCR uses fixed, pinned PP-OCRv6 recognition
models whose hash is recorded with every reading; corrections typed by people
are stored as data and never change a model.

```
 Sleuth.exe (Tauri 2)  <-- named pipe, JSON lines -->  sleuth-worker.exe ----------------------.
   Search, Tags, Revisions,                              name indexer (journal or walk)         |
   Check readings, Monitor                               PDF reader (classify, OCR, derive)     | HTTPS /api/v1
                                                         searches, previews, workflows          | (optional)
                                                         office client  ------------------------'
                                                                                                v
                                                    sleuth-server.exe: same engine over project folders,
                                                    access keys, per-project permissions, audit log
```

## Reading a PDF

1. **Content hash** (BLAKE3). A file with the same content already read here is
   copied, not read again (also after a move or rename: removed files keep their
   readings for 7 days).
2. **Shared cache** lookup by `BLAKE3(content hash + pipeline fingerprint)`. The
   fingerprint covers the reader version, page rules, OCR model hash, OCR
   settings and resolution, so a changed model or setting never reuses old
   readings. Entries carry a checksum; damaged ones are ignored.
3. **Claim**: before reading, the PC creates `<key>.claim` in the shared cache
   (exclusive create, renewed every minute). Another PC that sees a live claim
   waits for the result instead of reading the same file at the same time. A
   claim left by a switched-off PC expires after 20 minutes.
4. **Page classes**, from what the PDF says about each page:

   | Class | What it is | OCR |
   |---|---|---|
   | searchable | plotted text, few or no images | none; the title block only if its text has no drawing number |
   | hybrid | text plus raster images (stamps, pasted scans, site mark-ups) | only the image areas |
   | scanned | images and (almost) no text | the whole page |
   | vector_outlined | many vector paths, no text (text plotted as outlines) | the whole page |
   | sparse | little of anything | the whole page; blank tiles skipped |
   | damaged | cannot be loaded | none; the reason is recorded and the other pages are read |

   OCR renders at 300 dpi in 1600 px tiles with a 512 px overlap; blank tiles are
   skipped, regions cut by a tile edge are deduplicated, and OCR lines that repeat
   text-layer text are dropped (the text layer is exact). Lines under 85 %
   confidence get a second reading from Windows OCR; when the two disagree both
   are kept and the line is marked uncertain.
5. **Where OCR runs**: the CPU always works. A graphics card (DirectML) is used
   only after it passed a check on that PC: the same fixed picture is read on
   both, the readings must match exactly and the card must be clearly faster. The
   result is remembered per adapter, driver, model and Sleuth version. A card that
   fails while reading is never used again and the file is read again on the CPU.
   CUDA and OpenVINO are not offered: they need runtimes Sleuth does not ship.
6. **Derived data**, by configurable rules (never guessed): engineering tags with
   every occurrence (raw reading, confidence, page, box) and title-block details
   (drawing number, revision, title, sheet, discipline, status) each with the line
   it came from. Changed rules re-derive from the stored text without OCR.

## Finding files

- **Local NTFS disks**: when the worker or server runs with administrator rights,
  the NTFS change journal tells which folders changed since the last pass
  (checkpoint = journal id + USN). Only those are listed again; folders created or
  moved in are walked whole. A recreated or wrapped journal, or a malformed
  record, means a full walk and a new checkpoint. Without administrator rights
  (a normal desktop) disks are walked on a schedule as before.
- **Network shares**: listings run on a few threads per file server, each with a
  timeout. A listing that times out leaves a helper thread waiting on Windows; at
  most 4 may be waiting per server, after which that server counts as not
  answering until they return. A failed or partial listing never deletes anything
  from the index. An interrupted walk resumes where it stopped.
- **Unchanged files produce no reading jobs**; a changed size or date queues the
  file again.

## Searching

- SQLite FTS5 trigram index over folded text (`lines_fts`) and file names
  (`names`). Folder, project and title-block filters are applied in SQL before
  any row limit, so many matches elsewhere cannot hide the ones in scope.
- Query language: spaces = all terms, `OR` / `|` = alternatives, filters
  `name:`, `rev:`, `dwg:`, `disc:`, `type:`, `ext:`.
- Every result is classified and explained, strictest first: **Exact** (as typed,
  standing on its own), **Separators ignored**, **Verify** (look-alike characters
  or a reading under 80 % confidence), **Partial** (inside a longer tag, or only
  part of the tag was read, e.g. hidden by a line). A partial or uncertain match
  is never shown as exact.
- Results are remembered until the index changes, paged 500 at a time, and search
  times are recorded (p50/p95 on the Monitor page).

## Index safety and upgrades

- One SQLite database in WAL mode with transactional writes. Schema changes are
  additive: 1.0 keeps `user_version` at 1 and records its level in `meta`, so
  Sleuth 0.1 can still open the index after a rollback. A copy of the 0.1 index
  (`index.v1.bak.db`) is kept before the first 1.0 upgrade.
- Reading jobs are leased (30 minutes); a worker that stops mid-file leaves an
  expiring lease and the file is picked up again.
- Settings in `%APPDATA%\Sleuth` and the index in `%LOCALAPPDATA%\Sleuth` are
  never touched by installs, updates or rollbacks.
- Updates and rollbacks come from this repository's releases and are checked
  against the SHA-256 in `latest.json`. Installers are not code-signed (an
  internal tool, by decision), so the checksum over HTTPS is the integrity check.

## What we learned from Everything 1.4.1.1024

Everything was analysed with REA and Ghidra 12.1.4 (x64 PE, 3,045 functions).
It reads the NTFS catalogue and follows the USN journal through a 64 KB buffer
with every record bounds-checked, rebuilds a volume when the journal id changes,
keeps privileged reads in a service, scans network drives with
`FindFirstFile`/`ReadDirectoryChanges` (lossy beyond 64 KB, so it rescans on a
schedule), saves its database through a `.tmp` and rename, and never indexes
file contents. Sleuth follows the journal and safety patterns, adds per-server
limits and resumable walks for shares, and indexes the text inside drawings.
