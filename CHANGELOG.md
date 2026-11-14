# Changelog

## Unreleased

- **Dexterity Connect** (the office messenger): right-click a result → *Share to Dexterity Connect…*
  opens the messenger's share dialog with the file attached and the text Sleuth found in it, ready
  to send to a colleague or a project Space.
- **`sleuth://search?q=<tag>` links**: the installer registers the link, and opening one shows
  Sleuth and runs that search (also when Sleuth is already running). Dexterity Connect turns line
  numbers (`6"-P-1001-A1B`) and drawing numbers (`ISO-1001-01`) in chat into these links.

## 1.3.0

13 November 2026.

- **A third of the memory:** the Sleuth reader kept a cache of up to 32 MB
  for every part of the office index it searched, and after a few searches
  across the office it held 3 GB or more; on a PC with little free memory it
  then stopped. Each part now keeps 2 MB, and all of them together stay under
  384 MB. Measured on the office index (218 parts, 21 GB, five searches): 1.0
  GB at most instead of 3.0 GB; a first search takes as long as before, a
  repeated one up to a few seconds longer.
- **No false "can't be reached" for a busy share:** while a PC opens the
  office index at start, the share can take over 10 seconds for Sleuth's
  test write, and Settings said the office index folder could not be
  reached. It now waits up to 30 seconds before saying so.
- **Sleuth Server is a window:** double-clicking `sleuth-server.exe` opens
  the Sleuth Server window instead of a console: first-time setup in one
  form (office index folder, projects, then one button for certificates,
  service and checks), then Overview (service, address, what it reads),
  People and PCs, Projects, Certificate and Log. Opened from a new version's
  folder it stops the service, updates the installed copy and starts it
  again. The console setup stays as `sleuth-server setup`, and every command
  for scripts is unchanged.
- **Connect this PC:** one button in Settings > Office server. The server
  leaves its address, its certificate and a join code in the office index
  folder; Sleuth reads them and the server gives the PC its own access key.
  Nothing is typed and IT sends no keys. Only PCs that can open the office
  index folder can join; joining can be switched off, limited to some
  projects, or given a new join code in the server window. Keys typed by
  hand still work.
- **No certificate file to copy:** the certificate travels in the office
  index folder with the address and is kept by each PC that connected. The
  server writes these details itself whenever it starts. (Copying the file
  by hand failed with "You need permission" even for an administrator,
  because the server's data folder is locked to elevated administrators.)
- **Whole-office reading progress:** the Monitor shows how far the whole
  office is (per cent read, still to read, documents and pages an hour from
  every PC together, PCs reading, time left with a finish date) next to this
  PC's own pace, share and time alone; the PC table adds pages an hour, share
  and an office total. The status bar's time left is the office's. Fixed: the
  office's pace showed only this PC's, because PCs reported every ten minutes
  or so while the office counted only those seen in the last three; PCs now
  report every minute and are counted for twenty.
- **No document read twice:** a document stays its reader's for three hours
  after it is read (it was 30 minutes, shorter than other PCs could take to
  see the reading on a busy office), and a PC that cannot reach the lease
  folder waits instead of reading without a lease.
- **Faster search:** rows found by file name show at once, from memory, while
  the text inside the read documents is searched; a search no longer runs
  twice when the index changed while it ran. Small parts of the office index
  are merged a few at a time on the PC's own disk (all of them at once over
  the network never finished, so searches had to look through hundreds).
- **Preview marks you cannot mistake for red-line:** matches are shown with
  a cyan-to-violet lens instead of red (drawings carry red clouds and
  comments): every match tinted, the current one ringed with lock-on corners
  and the rest of the page dimmed; stepping glides to the next mark. A quick
  page appears first and the sharp one replaces it; the last pages shown are
  kept, so going back is instant.
- **Title-block details you can rely on:** a value must fill its cell (no
  more Revision "DR" out of "DR'N.", or Sheet "1" out of "(SHEET 1 TO 6)"), a
  label must start its cell, a REV column of a revision table gives no
  revision, and status words count only as whole words on a line of their
  own (no "VOID" from "AVOID"). Values already in the office index that their
  own evidence does not support are no longer shown; the Details pane works
  them out again from the stored text. The evidence names the label a value
  was found next to.
- **Tray:** closing the window keeps Sleuth in the tray, reading on. Click
  the tray icon to open it; **Exit Sleuth** in its menu stops Sleuth and the
  reader in the background.
- **Settings never fall back to defaults:** settings are flushed to disk
  before they replace the old file and kept as a last good copy, which is
  used when the file is found empty or damaged (a PC switched off while
  saving). Reading folders survive installs, updates, restarts and power
  cuts. The window no longer saves settings it has not finished loading.
- **Back by itself after the office folder drops:** while U: (or the office
  index folder) is away, Sleuth checks it every 10 seconds instead of every
  two minutes and carries on as soon as it answers; the office index's parts
  are opened 16 at a time (one by one, a few hundred of them left search
  empty for minutes). A search made while it is still opening says so and
  runs again by itself. The office server is tried every 15 seconds while
  offline.
- **Failed documents tried again:** once a reading folder has nothing else
  waiting, documents that could not be read are read again (each at most
  every 12 hours), so text missed through a locked file, a half-copied file
  or a dropped share is captured.
- **Office lock files not read:** `~$name.docx` / `.xlsx` / `.pptx`, the
  small files Office keeps beside a document someone has open, are found by
  name only and no longer counted as failed documents.
- **This PC's disks:** **Read C:**, **Read D:** under "Folders being read" add
  a local disk as a reading folder. **Read folders that need administrator
  rights...** restarts the reader with administrator rights; Windows asks
  for them (Sleuth never sees the password). Without them, folders Windows
  keeps closed are skipped as before. The rights last until Sleuth exits.
- **Fixed: the office server stopped at once after setup** ("cannot read
  server.json: Access is denied"). Setup's lock-down of the data folder left
  every file in it with no permissions at all. The folder now gets the
  permissions and its files inherit them; opening the new Sleuth Server
  window repairs a folder set up by 1.2.0 and starts the service.
- **Office index backup to GitHub** (`tools/backup`): a daily script on the
  server PC copies every new part, byte for byte, to a private GitHub
  repository (release assets, SHA-256 in a manifest), checks the parts on the
  share are still identical and puts back a damaged one; a workflow there
  checks every asset's size daily and its hash weekly; a restore script puts
  back whatever is missing. New: `sleuth-server check-parts`.

## 1.2.0

- **Office server setup by double-click:** `sleuth-server.exe` opened with no
  arguments now runs a guided setup instead of closing at once. It asks for
  administrator rights, installs itself in Program Files, and walks through
  the index folder, projects, certificates, access keys and the Windows
  service, checks the server answers, and ends with what to enter on each PC.
  Running it again opens a menu (add or remove people, check, restart, renew).
- **Server found by itself:** setup leaves the server's address in the office
  index folder next to its certificate. Settings > Office server offers it
  with **Use it**, so a PC needs only its access key. A "Where do these come
  from?" note explains each field.
- **Office index folder settles once set:** when the saved folder works it is
  shown as set up, with **Change...** to edit it, and the red outline goes as
  soon as a test succeeds.
- **Drive letter or network path:** U:\Sleuth-cache stays the default; on a
  PC where U: is not mapped or does not answer, Sleuth uses the same folder by
  its network path. For any other drive letter, the network path it led to is
  remembered while it works and used the same way.

## 1.1.1

- **Fixed:** an email or a text file no longer gets a drawing number or
  revision from the documents it mentions ("added in line list rev 4"). Such
  details already in the office index from 1.1.0 are no longer shown or used
  to group drawings; nothing has to be read again.
- **Fixed:** the time left said "about less than a minute"; it now says
  "less than a minute".

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
    images) are read many at a time: one PC read 2,000 spreadsheets, Word and
    text files on a network share in about 3 minutes (roughly 40,000 small
    files an hour). Larger documents take longer.
  - Scanned pages are read by separate reader processes at the lowest
    processor priority, so the PC stays as quick as before. The number of
    readers starts at the minimum while the PC is in use and doubles every
    30 seconds while it is free.
  - OCR reads real scanned drawings about 1.3 to 1.4 times as fast and
    needs about 0.8 GB instead of 1.8 GB per reader. Where it finds text on
    a page is exactly as before; on 30 real drawings no text found before
    was lost, and short text that 1.0 read upside down (900 as 006) is now
    read the right way up. Documents read by 1.0 keep their readings; new
    and changed files are read the new way.
  - Scanned pages still take minutes of processor time each on a PC: one
    PC reads a few dozen scanned pages an hour per processor core. The
    office reads many more by every PC reading at once.
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
