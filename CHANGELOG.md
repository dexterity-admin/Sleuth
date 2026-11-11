# Changelog

## 1.1.0

One office index instead of one per PC, every PC reading for everyone, and
every common file type read inside. Install over 1.0.x: each PC moves the
readings it already has into the office index and then deletes its old
index (5 to 6 GB on a busy PC); settings are kept.

- **One office index in the shared folder** (default `U:\Sleuth-cache`).
  Nothing large stays on a PC: its own disk holds settings and a few MB of
  work in progress. The index is made of files that are written once and
  never changed, so several PCs can add to it at the same time over the
  network without a database server and without damaging it.
- **One PC lists each share for the whole office**, and the others use its
  list; it carries on from where it stopped if it is switched off. Each PC
  still lists its own disks.
- **Every PC that is on reads for everyone.** A document is read by one PC
  only, and is searchable on every PC a minute or so after it is read. The
  office server, if there is one, reads day and night.
- **Every common file type is read inside**: Excel (`.xlsx`, `.xlsm`,
  `.xls`, `.xlsb`, `.ods`), Word (`.docx`, `.doc`), PowerPoint (`.pptx`,
  `.ppt`), Outlook emails (`.msg`, `.eml`), text and CSV, AutoCAD (`.dwg`,
  `.dxf`), MicroStation (`.dgn` V7 and V8, and PDS isometric natives
  `.i01`…), and scanned images (`.tif`, `.jpg`, `.png`, `.bmp`).
  AutoCAD files are read with LibreDWG 0.14, which now ships with Sleuth
  (GPLv3; licence and source link in the `libredwg` folder).
- **One row per document**: the PDF, DWG and DGN of the same drawing show on
  one result row, as chips that open each file, with the reason they were
  grouped (the same drawing number and revision in the title block, or the
  same file name beside each other). A switch shows every file on its own.
  Results are labelled by type and can be filtered by category.
- **Text preview for files without pages**: a spreadsheet's sheets, a
  model's text, an email, with the matching lines marked.
- **Reading is faster and never stops by itself.**
  - Documents that need no OCR (plotted PDFs and every type above but
    images) are read many at a time: {FAST_RATE}.
  - Scanned pages are read by separate reader processes at the lowest
    processor priority, so the PC stays as quick as before. The number of
    readers starts at the minimum while the PC is in use and doubles every
    30 seconds while it is free.
  - OCR itself does about a third less work on dense drawings ({OCR_GAIN})
    and needs {OCR_MEM} instead of 1.8 GB per reader, with the same text
    found.
  - A file that crashes or hangs its reader costs that file, not the reader.
- **You control both stages.** Pause and resume finding files and reading,
  start either at once (**Find files now**, **Read now**), choose **Full
  speed for 1 hour** or **Gentle**, and list one drive again from Monitor.
  A pause lasts until you resume it, across restarts, on this PC only.
- **Monitor shows the office**: every PC that is reading, what it does and
  how fast, the office's time left, and counts by file type.
- **Fixed:** the Settings label said reading pauses when other programs use
  the processor; it slows to the minimum and never stops.

## 1.0.2

Updates now find you. Install over 1.0.1 (or 1.0.0); settings and index are kept.

- **Sleuth asks when an update is ready.** It checks 10 seconds after it starts,
  every hour, and when the window comes back after an hour away. A newer
  release is downloaded and checked against its published SHA-256 in the
  background first; then a dialog asks to **Restart and update** or **Later**.
  Nobody has to open Settings and click **Check for updates**.
- **"Later" keeps the update in view.** An **Update x.y.z ready** button stays
  in the status bar, and the blue bar at the top, until it is installed. The
  dialog asks again at the next start.
- **Installing starts at once**, because the download is already done.
  Installers of older versions are deleted from the temp folder.
- **After the restart Sleuth says it was updated**, and a failed install says
  why and leaves Sleuth running, with a button to try again.

## 1.0.1

Fixes the indexing problems that left 1.0.0 with nothing to search, and shows
what Sleuth is doing at every moment. Install over 1.0.0: the index is upgraded
and repaired in place; settings are kept.

- **Reading folders chosen by drive letter now work.** `Z:\` was compared with
  the network paths the index stores (`\\server\Project\...`), so no PDF in it
  was ever queued for reading. Letters are now translated, and an existing
  index is repaired when the worker starts.
- **The file list is complete.** A walk took subfolders it had only seen as
  already listed, so most of each drive was skipped and the walk "finished" in
  seconds. On the office drives a first walk now finds over 1.5 million files
  where 1.0.0 had 150,000. A walk stopped half-way resumes where it was.
- **Reading folders are listed first, and reading starts as soon as their
  drives are listed**, not after every drive. Until then only files you open
  or search for are read.
- **Drives sharing a file server take turns**: a deep share no longer starves
  the others (one drive had listed one folder in four minutes).
- **One share reached by two names** (a server name and its IP address) is
  walked once and shown once, not twice.
- **A busy PC reads slowly instead of not at all.** 1.0.0 paused reading
  whenever other programs used much CPU, so a PC that was always busy never
  read anything; now it reads on one thread.
- **Short network drops are retried** (the server check three times, a failed
  folder once more later) instead of marking a drive offline for hours.
- **The index is never thrown away because it was busy.** Only a damaged index
  is rebuilt; "database is locked" is waited out, and a folder listing is never
  dropped because of it.
- **The index's temporary log stays small.** It grew by gigabytes during a
  walk and could fill the disk. A walk now writes only files that changed
  (it rewrote every file of every drive each time), the log is folded back
  every minute, and writing waits whenever it passes 512 MB. A program that
  holds the index open is reported in Monitor.
- **File names are searched in memory**, as Everything does: every name is
  loaded at start (about 30 s for 2 million files) and kept up to date as
  the walk adds and removes files. On 1.7 million files `*.zip` takes under
  a second (42 s before), `x` 0.4 s (101 s before).
- **Progress you can see.** A status strip shows both stages, "Finding files"
  (folders listed, files found, per drive, running time) and "Reading PDFs"
  (read of total in your reading folders, waiting, time left, why it waits).
  Monitor has a table per drive and per reading folder; search says when
  results may still be incomplete; a new install opens on a welcome card that
  explains what happens next. Status no longer takes seconds on a big index.
- **File-name search works like Everything**: any length (`FS`, `x`, `_`,
  `&`), `*` and `?` wildcards, several words matched across name and folder,
  a pasted full path (`T:\...\file.pdf`) finds that file, exact names first,
  Unicode upper/lower case (`Обычный`), and name hits are no longer buried
  under PDF text hits. Searches no longer wait for the history to be saved.

## 1.0.0

Faster, more complete reading; search you can trust and explain; engineering
workflows; an optional office server. No AI features: everything is
deterministic and traceable to the page.

- **Smarter reading.** Each page is classified (searchable, hybrid, scanned,
  outlined text, sparse, damaged). Plotted drawings need no OCR; on hybrid
  drawings only the pasted images and scanned mark-ups are read, which 0.1
  skipped when they covered less than 40% of the sheet. A damaged page no longer
  fails the whole file.
- **Graphics card when it helps.** OCR can run through DirectML, but only after
  the card read a check picture exactly like the CPU and was clearly faster;
  otherwise, and after any failure, the CPU is used.
- **Search you can explain.** Every result says why it matched. A tag found only
  inside a longer tag (`PSV-120` in `PSV-1203`), or read only in part (a
  line-number crossed by a pipe), is labelled Partial and never shown as exact.
  `OR`, `name:`, `rev:`, `dwg:`, `disc:`, `type:`, `ext:`; projects; history and
  pinned searches; export to CSV and Excel; paging.
- **Folder limits are applied before result limits**, so matches elsewhere can
  no longer hide matches in the folder or project you chose.
- **Engineering data.** Tags (line numbers, instruments, valves, equipment,
  supports, welds, drawing numbers, specs) and title-block details (drawing
  number, revision, title, sheet, discipline, status) are taken from the text by
  configurable rules, each with the line it came from.
- **Tags view**: every tag, the documents and pages it appears on, with all
  occurrences marked on the page.
- **Revisions view**: drawings grouped by drawing number; compare two revisions
  to see tags added, removed and changed, side by side. Differences that could be
  OCR misreadings are marked Verify, never reported as changes.
- **Related documents**: other drawings that share tags with the one open.
- **Check readings**: low-confidence OCR lines with the image beside them; type
  what is printed. The original reading is kept; nothing is learned from it.
- **Monitor**: reading progress, page classes, OCR time per page, cache hits,
  search times, folders that were offline or partly listed, recent events.
- **Shared cache 2.0**: entries are keyed by content and the full reading
  pipeline (model hash, settings), carry a checksum, and PCs take a claim before
  reading so two PCs never read the same drawing at once. Entries from 0.1 are not
  reused.
- **Faster, safer discovery.** With administrator rights, local NTFS disks are
  followed through the change journal. Moved or renamed drawings are not read
  again. Interrupted walks resume. A hung file server cannot pile up threads.
  Partial or failed listings never remove files from the index.
- **Optional office server** (`Sleuth-Server_1.0.0_x64.zip`): the same engine
  over shared project folders, HTTPS with its own certificate, access keys,
  per-project permissions, audit log. Desktops search it alongside their own index
  and keep working when it is offline. See `docs/SERVER.md`.
- **Rollback**: Settings can reinstall the previous release; the 1.0 index stays
  readable by 0.1.
- The installer now carries the DirectML and Visual C++ runtime files the OCR
  engine loads, so it works on PCs that lack them.

## 0.1.1

- Fixed: empty banners showed at the top of the window on start-up.
- A tag typed with a space (`FV 2201`) is now found.
- "Keep reading in the background" now registers the reader to start with Windows from the first launch, not only after saving Settings.
- Website and a 21-page user guide (PDF).

## 0.1.0

- First internal release.
- Search tags and text inside PDFs, including scanned drawings (PP-OCRv6 with a Windows OCR second opinion).
- Separator-insensitive tag matching and labelled look-alike matches (O/0, l/1, S/5, B/8).
- Deep search across local and mapped drives with per-server limits, timeouts and offline detection.
- Page preview with the matched text marked.
- Shared reading cache (default `U:\Sleuth-cache`) so each PDF is read once for the whole office; the app keeps asking until it works on each PC.
- In-app updates from GitHub releases, checked by SHA-256; settings and index are kept.
