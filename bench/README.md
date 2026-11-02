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
(hybrid), a plotted drawing with a small logo, and two revisions of one drawing.
`truth.json` lists every tag each file carries.

## Real drawings (private)

The release targets in the 1.0 plan are judged on real drawings, which must not
go into this public repository. Put 100 to 200 representative pages in a private
folder (CAD isometrics, scanned isometrics, P&IDs, GAs, hybrid PDFs, poor scans,
A0/A1 sheets) with a `truth.json` of the tags checked by hand on each page, keep
a fixed holdout set you never tune against, and run the same command on it.

## Results

See [results.md](results.md) for the latest run on the synthetic corpus.
