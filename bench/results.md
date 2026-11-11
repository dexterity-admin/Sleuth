# Reader benchmark results

## OCR pipeline: CPU time and memory per page

Synthetic corpus from `make_corpus.py` (12 files, 156 known tags), office
i7-6700, 12 November 2026, OCR on **one thread**. The readers of the next
version are single-threaded processes, so what decides throughput is
CPU-seconds per page on one core, and what decides how many readers a PC can
hold is the memory of one.

`--legacy` is the pipeline of Sleuth 1.0.2, kept in the code for comparison;
"now" is the pipeline in use. Read this table with its limits in mind: the PC
was heavily loaded by other work throughout, which makes every time larger
than on an idle PC and moves the CPU time of one and the same run by 10 to
15% from one hour to the next. So the two pipelines were run at the same
time, side by side. (Two earlier side-by-side passes, before the last
corrections to how duplicates are removed, gave the same picture: 580
against 507 CPU-seconds, and 216 against 148. Built with link-time
optimisation off; the time is spent inside ONNX Runtime either way.)

| | 1.0.2 pipeline | now |
|---|---:|---:|
| CPU-seconds, eleven older sheets (6 pages need OCR, 92 tiles) | 576 | 511 |
| CPU-seconds, `ISO-4001-01 dense.pdf` (1 page, 15 tiles) | 188 | 132 |
| Peak memory of the reader process | 1298 MB | 654 MB |
| Text regions the detector found (eleven sheets) | 501 | 501 |
| Regions put through the recogniser | 501 | 343 |
| Regions put through the orientation classifier | 501 | 61 |
| Tags found exactly (of 139 on the eleven sheets) | 136 | 137 |
| Found in part, "Partial, verify" (a drawn line crosses the tag) | 3 | 2 |
| Not found | 0 | 0 |
| Tags of the full sheet: exact / in part / not found (of 17) | 17 / 0 / 0 | 17 / 0 / 0 |

On sheets like the older eleven, with 80 pieces of text a page, detection is
seven to eight tenths of the time, and detection is deliberately untouched
(below), so the saving there is a ninth. The saving is in reading the text,
and shows where there is text to read: three tenths on the full sheet, and
real drawings are mostly as full as that or fuller. Memory halves
everywhere. The one tag more is `2"-D-1005-B2A` on `Scan_0042.pdf`, whose
size a drawn line crosses: one tile boxed it from the `2` on, its neighbour
from the dash on. The old pipeline kept the surer of the two readings, the
one without the size; now a reading goes only for one that holds its text.

What the new pipeline does differently:

- **Text two tiles both found is recognised once.** With 512 px of overlap a
  third of all regions are found twice. Two boxes count as the same text
  only if one covers the other end to end, to within 8 px: a box that starts
  a character further out holds other text and is read too. The box not read
  is kept, and a line that reads with low confidence, or as nothing
  searchable, is read from it as well, so an unsure line still gets its
  second look.
- **Regions are recognised one by one.** Before, the regions of a tile went
  in batches of eight, each batch padded to its widest region, and the
  padding is computed like text. One by one is no slower on one thread, takes
  less memory, and a line's reading no longer depends on its neighbours.
- **The orientation classifier sees vertical text and lines that read
  badly**, not every line. Text printed upright never needed it, and on the
  test sheets two upright lines were read wrongly because of it.
- **A line cut by a tile edge is also read whole**, joined from its parts,
  across three tiles if it is that long. The parts stay unless the joined
  line holds their text.
- **A line goes only for a line that stays and holds its text.** Before, of
  two overlapping readings the more confident won whatever their sizes: the
  fragment a neighbouring tile saw could displace the whole line, a small box
  inside a long line cut by a tile edge removed that line, and a piece that
  lay half inside another line went even if it ran on beyond it. Now a part
  goes when the whole line says what it says, a copy cut by a tile edge goes
  when the neighbouring tile saw the same stretch uncut, and of two different
  readings of one box the surer stays unless the other says more. Nothing
  else is removed, and a line that went for one that then went itself comes
  back.
- **ONNX Runtime plans no memory patterns.** With inputs of ever-changing
  size it kept a block per size. Same readings, same time, a third less
  memory.

What it does not do: **detection is untouched.** The same tiles, each
rendered by itself exactly as before, at the same resolution, so the detector
is shown the pixels it was always shown and finds the same boxes: the count
of regions found is equal on every sheet here, and on real drawings. On this
CPU, loaded as it was, a 1600 px tile cost the detector about four
CPU-seconds and a line of text cost the recogniser about a quarter of one;
those are the networks themselves, and no arrangement of the code around
them makes them cheaper.

### Tried and left out

Each of these was measured on this corpus and on real drawings, which cannot
be published; the verdicts rest on both. All of them remain reachable with
`bench --set`.

| Setting | Effect on synthetic sheets | Why it is not the default |
|---|---|---|
| The page rendered once, in bands, and tiles cut from it (`page_render=1`) | Output identical byte for byte; rendering falls from 4% of the time to 1% | On real scans PDFium resamples the image a little differently for a band than for a tile. The detector then found some faint text it had missed and missed some it had found, short handwritten entries among it. Not worth 3%. |
| Tiles of 2400 px scaled to 1600 for detection (`tile=2400,scale_all=1`) | 30 to 45% less CPU time, no tag lost | On real drawings it found text the default misses and missed text the default finds: faint labels and lettering about 1 mm high. Losing any is not acceptable. |
| Tiles of 3200 px scaled to 1600 (`tile=3200,scale_all=1`) | about 65% less CPU time, no tag lost | The same, more so. |
| 256 px overlap instead of 512 (`overlap=256`) | 8 to 30% less CPU time, no tag lost | Every spot is then seen by one tile and a third on average instead of nearly two. Faint text that one tile misses and its neighbour finds was lost on real drawings. |
| Equal tiles instead of full ones and a narrow last one (`even_tiles=1`) | no tag lost | The detector's answer depends a little on everything in its tile; another grid found and missed other faint text on real drawings. |
| Recognition in batches of 8 (`rec_batch=8`) | no faster | More memory, and readings that depend on the batch. |
| Recogniser input padded to 160 px instead of 320 (`rec_width=160`) | 8% less CPU time | 2 of 85 lines read differently; 320 is what the model was trained with. |
| Orientation classifier never (`orient=never`) | 14% less CPU time | Vertical tags are read upside down. |
| Two tiles per detection call (`det_batch=2`) | no faster | Two thirds more memory. |
| Two ONNX Runtime threads (`--threads 2`) | 20 to 23% more CPU time for the same work | One thread per reader process is the cheapest way to use a core. |
| ONNX Runtime graph optimisation, spinning and allocator settings | none | It already applies every graph optimisation; with only the basic ones the same work took 38% longer. |
| Finding blank tiles from a small render before the full one | not built | Rendering is a few per cent of the time; blank tiles are skipped before detection as before. |
| Second opinion from the Windows reader | left as it is | Well under 1% of the time. It now runs once the band has been read, on fewer lines, since nothing is read twice. |

### Image files

`OcrReader::read_image_file` reads scanned TIFF, JPEG, PNG and BMP files with
the same tiling, at the image's own resolution; a TIFF gives one page per
frame. `make_corpus.py` writes three of the scans again as image files
(`images/`): a PNG, a JPEG, a BMP and a three-page Group 4 TIFF. Of their 25
tags, 21 were found exactly and 4 in part (the same line-crossed sizes as in
the PDF versions), none missed. An image of more than 260 megapixels is
refused with the reason.

## Page classes: 0.1 rule against the 1.0 reader

Ten files of the corpus (56 known tags; the small-print and the full sheet
came later), office i7-6700, OCR on 7 threads, 3 November 2026.

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
