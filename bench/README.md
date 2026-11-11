# Reader benchmark

Measures how Sleuth reads drawings: page classes, OCR work, time, and how many
known tags it finds. Nothing here is a claim about real drawings until it has
been run on them.

## Synthetic corpus (public, reproducible)

```bash
python bench/make_corpus.py .devdata/corpus
```

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus
```

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus --mode old
```

`--mode old` reproduces Sleuth 0.1's rule (whole-page OCR when a page has under
40 text characters or images over 40% of its area; no OCR otherwise), so the two
runs compare the same files on the same PC. `--backend directml` runs OCR through
DirectML instead of the CPU.

The corpus holds invented sheets only: scanned isometrics and a P&ID, a faded
site scan, plotted text documents, a plotted drawing with a scanned site mark-up
(hybrid), a plotted drawing with a small logo, two revisions of one drawing, a
scanned A2 general arrangement in small print (83 tags 1.8 to 2.5 mm high,
some vertical, boxed or touched by a leader line, spread so that some sit on
every tile border), and a scanned isometric as full as real ones are (a bill
of material of 30 rows, notes, dimensions, weld numbers: some 250 pieces of
text). `truth.json` lists every tag each file carries.

`images/` inside the corpus holds three of the scans again as loose image
files (PNG, JPEG, BMP and a three-page Group 4 TIFF) with their own
`truth.json`, for the reader's image support:

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus/images
```

## Measuring the OCR pipeline

Readers run as single-threaded processes, so the figure that matters is
CPU-seconds per page on one core, and the memory of one process:

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus --threads 1 --repeat 3
```

Every run ends with where the OCR time went (rendering, detection, orientation,
recognition, second opinion) and the peak memory of the process. CPU time is
the process's own user and kernel time; when the PC is busy, repeat and take
the fastest pass.

To compare two pipelines on the same files, give each run a `--dump` file and
compare them line by line:

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus --threads 1 --legacy --dump old.json
```

```bash
cargo run --release -p sleuth-engine --example bench -- .devdata/corpus --threads 1 --dump new.json
```

```bash
python bench/compare_dumps.py old.json new.json --show 40
```

`--legacy` is the Sleuth 1.0 pipeline (a PDFium render per tile, 512 px overlap,
recognition tile by tile). `--set key=value,...` changes single settings of
either pipeline: `tile`, `overlap`, `detect`, `even_tiles`, `page_render`,
`band_mpx`, `pool`, `early_dedupe`, `stitch`, `orient` (`always`, `vertical`,
`never`), `rec_batch`, `rec_width`, `det_batch`, `second_opinion`, `graph_opt`,
`mem_pattern`, `one_call` and `ort:<session entry>`. `compare_dumps.py` reports
the lines a candidate lost, changed and gained against a baseline, in the
matcher's own normal form (letters and digits, lower case). A speed-up is
accepted only if it loses no tag on the corpus and what it loses on real
drawings turns out, looked at, to be noise.

`--pages LIST` reads single pages of real files (`path<TAB>page` per line) with
whole-page OCR and prints numbers only, no file names. `--image FILE` reads one
image file.

## Real drawings (private)

The release targets in the 1.0 plan are judged on real drawings, which must not
go into this public repository. Put 100 to 200 representative pages in a private
folder (CAD isometrics, scanned isometrics, P&IDs, GAs, hybrid PDFs, poor scans,
A0/A1 sheets) with a `truth.json` of the tags checked by hand on each page, keep
a fixed holdout set you never tune against, and run the same command on it.

## Results

See [results.md](results.md) for the latest run on the synthetic corpus.
