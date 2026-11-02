# Reader benchmark results

Synthetic corpus from `make_corpus.py` (10 files, 56 known tags), run on the
office i7-6700 (4 cores, 8 threads; OCR on 7 threads), 3 November 2026.

The same Sleuth 1.0 matcher scores both runs, so the only difference is how
pages are read: `new` is the 1.0 reader, `old` reproduces 0.1's rule for when
to run OCR.

| | 0.1 rule | 1.0 reader |
|---|---:|---:|
| Tags found exactly (or with separators ignored) | 51 | **53** |
| Found only in part, labelled Partial (a pipe line crosses the size) | 3 | 3 |
| Not found at all | 2 | **0** |
| OCR tiles read | 60 | 62 |
| Total reading time | 84.8 s | 88.2 s |

What changed and why:

- **The scanned mark-up on a plotted drawing is now read.** On
  `ISO-3001-01 with markup.pdf` the site mark-up covers about 4% of the sheet.
  0.1 only ran OCR when images covered 40% of a page, so `TT-3105` and
  `XV-3109` were never read. 1.0 classifies the page as *hybrid* and reads only
  the image area: 2 tiles, 1.6 s, both tags found.
- **Scanned sheets cost the same.** A scan has to be read in full either way
  (15 tiles, about 20 to 25 s per A3 sheet at 300 dpi on this CPU). The times
  above differ by run-to-run noise.
- **Plotted drawings cost nothing** in both versions, and 1.0 additionally reads
  only the title block when its text has no drawing number.

Three line numbers on the scans are crossed by a drawn pipe line, so OCR reads
`-P-1002-A1B` instead of `4"-P-1002-A1B`. The 1.0 matcher finds these as
**Partial, verify** with the reason; the 0.1 matcher did not find them at all.

## Graphics card

On this PC (NVIDIA GeForce GT 710, 2 GB) DirectML failed the check: text
detection did not run on the card, so Sleuth chose the CPU and recorded why
(Monitor → OCR runs on). No speed comparison with a working card has been made;
the 1.0 plan's target of 3× faster reading on difficult pages is **not
demonstrated** here and needs a PC with a DirectML-capable card and the private
corpus.

## Not measured yet

The plan's acceptance numbers (tag precision and recall on 100 to 200 real,
hand-checked pages; p95 search time on a full office index; hardware tiers)
need the private corpus described in [README.md](README.md). Search times on
this small index were 25 ms (p50) and 32 ms (p95).
