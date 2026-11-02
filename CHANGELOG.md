# Changelog

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
