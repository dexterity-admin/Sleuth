# Sleuth architecture

## What we learned from Everything 1.4.1.1024

Everything was analysed with REA and Ghidra 12.1.4 (x64 PE, 3,045 functions).

1. **Local NTFS/ReFS volumes.** Everything reads the file system's own catalogue (MFT enumeration with `FSCTL_*`) instead of walking folders, and stores `{FRN, parent FRN, name}`. It then follows the USN change journal through a 64 KB buffer.
   - Every journal record is bounds-checked ("record overflow", "bad file name offset", "bad record length").
   - Changes are batched under a lock and handed to an update thread.
   - A changed journal ID makes it rebuild that volume. A read error makes it retry after 30 s.
2. **Privileges.** Reading the journal needs administrator rights, so that runs in the `Everything` service (`\\.\PIPE\Everything Service`). The window runs as a normal user.
3. **Network drives are Everything's weak spot.** It scans them with `FindFirstFile`/`FindNextFile`, then watches them with ReadDirectoryChanges. That watch buffer is limited to 64 KB on network drives, and "changes can be missed", so it falls back to scheduled rescans.
4. **Saving the index.** The database is written to `Everything.db.tmp` and then renamed into place. It keeps pre-sorted arrays per column, checks them for corruption, and rebuilds when they are damaged.
5. **Searching.** Search is a linear scan of names in memory. Sorting is multithreaded: the list is split into chunks of 65,536, each chunk is sorted on a reused thread pool, and the chunks are then merged. Searches can be cancelled.
6. **File contents are not indexed.** `content:` reads every candidate file at search time, and scanned PDFs have no text to find.
7. **Staying responsive.** Background threads use `THREAD_MODE_BACKGROUND_BEGIN`, change bursts are debounced (1 s), and icons, thumbnails and previews each load on their own low-priority threads.

## How Sleuth differs

| Concern | Sleuth |
|---|---|
| Text inside scanned drawings | Each PDF's text layer is read first. Pages without usable text are OCRed with PP-OCRv6 at 300 dpi, in 1600 px tiles with a 512 px overlap. Duplicate reads from the overlap are removed. Lines below 85 % confidence get a second reading from Windows OCR; when the two disagree, both readings are kept and marked uncertain. |
| Tag spelling | Each line is compared three ways: exact, separators removed, and look-alike characters folded. Matches are labelled Exact, Separators ignored or Verify. Low-confidence OCR never counts as Exact. |
| Network drives | Everything is stored by UNC path, so drive letters may differ between PCs. A deep search lists folders on a few threads per file server, never per drive letter. It probes each share first, gives up on any listing that stalls, and lists every folder it skipped with the reason. |
| Weak PCs | OCR runs in a separate background process at low priority. It pauses while other programs are busy or the PC is on battery, uses half the cores while you work and more when the PC is idle, and frees the model after 2 minutes of no work. |
| Office-wide reuse | A shared cache folder (default `U:\Sleuth-cache`, resolved to its network path on each PC) stores each read PDF under its BLAKE3 hash, so each drawing is read once for the whole office. The worker re-checks the folder every 2 minutes; the window insists until it works. |
| Index safety | One SQLite database in WAL mode with transactional writes. A damaged or newer-format index is moved aside and rebuilt. |
| Updates | The NSIS installer installs per user. Updates come from this repository's GitHub releases and are checked against the SHA-256 in `latest.json`. Settings in `%APPDATA%` and the index in `%LOCALAPPDATA%` are never touched. The worker is stopped before files are replaced. |

## Processes

```
Sleuth.exe (Tauri 2, WebView2)  <-- named pipe \\.\pipe\Sleuth-<user>, JSON lines -->  sleuth-worker.exe
  Workbench UI, updater                                                                       name indexer thread (background mode)
                                                                                              PDF reader thread (OCR queue)
                                                                                              searches / deep searches / previews
```

## Measured on the office i7-6700 (8 threads)

- A vector-plotted piping isometric with no text layer took about 70 s per page to OCR at 300 dpi with the standard model. It read 274 lines, including the title block, line numbers and vertical text.
- At 200 dpi a line number on the sheet was missed, so 300 dpi stays the minimum.
