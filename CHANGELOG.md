# Changelog

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
