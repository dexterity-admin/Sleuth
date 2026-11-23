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

## The copy of the office index on each PC (1.4)

The office index is a folder of write-once SQLite files ("parts") on the
shared drive. Searching them there meant hundreds of small random reads over
the network for every word typed, and opening them all after a start took
longer than people wait (409 parts, 66 GB on 23 Nov 2026: no text result
seven minutes after a start). Since 1.4 every PC keeps a copy:

- **Where**: `local_index_dir` in the settings. Empty, Sleuth picks a local
  disk once and remembers it: an SSD before a hard disk, a disk with 100 GB
  free before a fuller one, a second disk before the Windows disk
  (`<disk>:\Sleuth-index`, or `%LOCALAPPDATA%\Sleuth\index` on the Windows
  disk). `"off"` searches the network folder as before.
- **How**: a part never changes, so a copy of the same size that opens as a
  part is the part. Two threads copy what is missing, newest first, through a
  temporary name; 8 GB always stay free on the disk. A part that cannot be
  copied (no room, three failed tries) is searched over the network, so no
  document is ever missing from a search because of the copy. A copy that
  fails because the shared folder went away is not counted as a failure.
- **Start**: the copies are opened straight away, before the shared folder is
  even checked.
- **Shared folder away**: the copy keeps answering; the status says so.
- **Upkeep**: copies of parts the office merged away are deleted; a copy that
  fails while being searched is thrown away and copied again.
- File lists and the parts pool are kept by separate threads, so names are
  searchable a second or two after a start whatever the index is doing.

## Searching in two passes (1.4)

The quick pass takes at most 150 rows per term from each part and each file
list, on every core; it sets a flag when any query reached its limit. When
none did (a tag, a number), its rows are the whole answer and are remembered
as such. Otherwise they are sent as provisional rows and the full pass
(2,000 rows per term and part) replaces them, on one thread that sleeps as
long as it worked. The full pass checks a cancel flag between parts.

## Sleuth's share of a desktop (1.4)

- Reader processes sit in one Windows job object with a hard ceiling of 8 %
  of the whole processor; 1 % while a search runs. At most one OCR reader
  and two text readers, in every speed mode. No ceiling on their memory (with
  one, the OCR models did not load); OCR waits while the PC has under 1.5 GB
  left to give (free memory or commit, whichever is less).
- A desktop search takes a fixed number of rows per term from all parts
  together (4,000 in the quick pass, 40,000 in the full pass), shared out by
  part size, so its work does not grow with the number of parts. Each part
  still costs about 3 ms of page reads per term: fewer, larger parts (the
  merge) are what make a search cheaper.
- A ceiling on the worker process as a whole was tried and removed: once the
  readers had used the share, Windows held back the threads that answer the
  window too, and a request waited over a minute.
- The worker's own long jobs (merging, the full pass of a search) run on one
  thread at half time. The office server is not limited.
- Readers end with the worker (job object), and each also watches the
  worker's process and ends itself when it is gone. **Exit Sleuth** asks the
  worker to stop, which ends its readers and then itself without running
  exit handlers; whatever is left after five seconds is ended by force.

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
