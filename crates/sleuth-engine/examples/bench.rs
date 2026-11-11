//! Measure the reader on a corpus with known tags:
//! `cargo run --release --example bench -- <corpus dir> [--mode new|old] [--backend cpu|directml] [--quality standard|high]`
//!
//! The corpus folder holds PDFs and `truth.json` (`{"file.pdf": ["tag", ...]}`).
//! For every file it reports the page classes, OCR tiles read, time, and how
//! many known tags were found exactly (or with separators ignored), only as a
//! look-alike/low-confidence reading (verify), only in part (a few characters
//! not read, for example hidden by a line), or not at all.
//!
//! `--mode old` reproduces Sleuth 0.1: a page with under 40 text characters
//! or images over 40% of its area is read with OCR in full; any other page is
//! not read with OCR at all.
//!
//! Measuring the OCR pipeline itself:
//!
//! - `--threads N` sets the ONNX Runtime threads (default: logical CPUs - 1).
//!   Readers run as single-threaded processes, so `--threads 1` gives the
//!   figure that matters: CPU-seconds per page on one core.
//! - `--tile N --overlap N --detect N` override the tiling; `--set k=v,k=v`
//!   sets any other OCR option by name (see `OcrOptions::set`), and `--legacy`
//!   starts from the Sleuth 1.0 pipeline instead of the present defaults, so
//!   two pipelines can be compared on the same files.
//! - `--image FILE` reads one scanned image file (TIFF, JPEG, PNG, BMP)
//!   instead of a corpus. A corpus may hold image files too.
//! - `--dump FILE` writes every line read (text, confidence, box) as JSON, to
//!   compare two configurations line by line (`bench/compare_dumps.py`).
//! - `--pages LIST` reads single pages instead of a corpus: `LIST` is a text
//!   file of `path<TAB>page` lines (1-based page; whole page read with OCR).
//!   No corpus folder is given in this mode.
//! - `--repeat N` reads everything N times and reports the fastest pass.
//!
//! CPU time is the process's user + kernel time, so it does not depend on what
//! else the PC is doing as much as wall-clock time does. Every run ends with
//! where the OCR time went (render, detection, recognition, second opinion,
//! ...) and the peak memory of the process.
use sleuth_core::analyzer::{MatchClass, Term};
use sleuth_core::settings::{OcrQuality, Settings};
use sleuth_engine::derive::Deriver;
use sleuth_engine::ocr::{Backend, OcrOptions, OcrReader, Timings};
use sleuth_engine::pdf::Doc;
use sleuth_engine::text::{FileText, Line};
use sleuth_engine::{models, reader};
use std::collections::BTreeMap;
use std::time::Instant;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn num<T: std::str::FromStr>(name: &str) -> Option<T> {
    arg(name).and_then(|v| v.parse().ok())
}

/// User + kernel CPU seconds of this process so far, and its peak working set
/// in megabytes.
#[cfg(windows)]
mod proc {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MemCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(h: isize, creation: *mut FileTime, exit: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        fn K32GetProcessMemoryInfo(h: isize, counters: *mut MemCounters, cb: u32) -> i32;
    }

    pub fn cpu_seconds() -> f64 {
        let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
        unsafe {
            GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        }
        let t = |f: &FileTime| ((f.high as u64) << 32 | f.low as u64) as f64 / 1e7;
        t(&k) + t(&u)
    }

    /// `(peak working set, peak commit)` in megabytes.
    pub fn peak_mb() -> (f64, f64) {
        let mut m = MemCounters { cb: std::mem::size_of::<MemCounters>() as u32, ..Default::default() };
        unsafe {
            K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb);
        }
        (m.peak_working_set_size as f64 / 1_048_576.0, m.peak_pagefile_usage as f64 / 1_048_576.0)
    }
}

#[cfg(not(windows))]
mod proc {
    pub fn cpu_seconds() -> f64 {
        0.0
    }
    pub fn peak_mb() -> (f64, f64) {
        (0.0, 0.0)
    }
}

/// Sleuth 0.1's rule, for comparison.
fn read_old(path: &std::path::Path, r: &OcrReader, dpi: u32) -> anyhow::Result<(FileText, u32)> {
    let doc = Doc::open(path)?;
    let mut ft = FileText::default();
    let mut tiles = 0;
    for i in 0..doc.page_count() {
        let (mut page, f) = doc.analyze(i)?;
        if f.chars < 40 || f.image_coverage >= 0.4 {
            let (p, st) = r.read_regions(&doc, i, dpi, &[], &|| false)?;
            tiles += st.tiles - st.blank;
            page.lines.extend(p.lines);
        }
        ft.pages.push(page);
    }
    Ok((ft, tiles))
}

fn dump_lines(lines: &[Line]) -> serde_json::Value {
    serde_json::Value::Array(
        lines
            .iter()
            .map(|l| {
                serde_json::json!({
                    "t": l.text, "c": l.conf, "alt": l.alt,
                    "src": format!("{:?}", l.source),
                    "b": [l.bbox.x, l.bbox.y, l.bbox.w, l.bbox.h],
                })
            })
            .collect(),
    )
}

fn options() -> anyhow::Result<OcrOptions> {
    let backend = if arg("--backend").as_deref() == Some("directml") { Backend::DirectMl } else { Backend::Cpu };
    let threads = num("--threads").unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1));
    let mut o = if std::env::args().any(|a| a == "--legacy") { OcrOptions::legacy(threads) } else { OcrOptions { threads, ..Default::default() } };
    o.backend = backend;
    if let Some(v) = num("--tile") {
        o.tile = v;
    }
    if let Some(v) = num("--overlap") {
        o.overlap = v;
    }
    if let Some(v) = num("--detect") {
        o.detect_side = v;
    }
    for kv in arg("--set").unwrap_or_default().split(',').filter(|s| !s.trim().is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--set wants key=value, got {kv}"))?;
        o.set(k.trim(), v.trim())?;
    }
    Ok(o)
}

/// Where the OCR time went, as a table.
fn print_timings(t: &Timings, passes: usize) {
    let n = passes.max(1) as f64;
    let total = t.total.max(1e-9);
    println!("
| OCR step | seconds | share |
|---|---:|---:|");
    for (name, v) in [
        ("render page pixels / decode image", t.render),
        ("cut tiles, skip blank ones", t.tiles),
        ("detect text", t.detect),
        ("crop regions", t.crop),
        ("orientation classifier", t.orient),
        ("recognise text", t.recognise),
        ("second opinion (Windows OCR)", t.second),
        ("everything else", t.other()),
    ] {
        println!("| {name} | {:.2} | {:.0}% |", v / n, v / total * 100.0);
    }
    println!("| **total** | **{:.2}** | |", t.total / n);
    let per = |v: u32| v as f64 / n;
    println!(
        "
{:.0} pages, {:.0} tiles ({:.0} blank), {:.0} regions detected, {:.0} dropped as seen twice, {:.0} joined across a tile edge, {:.0} through the orientation classifier, {:.0} recognitions ({:.0} of them second looks), {:.0} second opinions{}",
        per(t.pages),
        per(t.tiles_read),
        per(t.tiles_blank),
        per(t.regions),
        per(t.twins),
        per(t.stitched),
        per(t.oriented),
        per(t.recognised),
        per(t.second_looks),
        per(t.second_asked),
        if passes > 1 { format!(" (average of {passes} passes)") } else { String::new() }
    );
}

fn is_image(path: &std::path::Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("tif" | "tiff" | "jpg" | "jpeg" | "png" | "bmp"))
}

/// An image file read as a document: one page per frame.
fn read_image(path: &std::path::Path, r: &OcrReader) -> anyhow::Result<(FileText, u32)> {
    let (pages, stats) = r.read_image_file(path, &|| false)?;
    let mut ft = FileText { model: r.model.clone(), ..Default::default() };
    let mut tiles = 0;
    for (p, st) in pages.into_iter().zip(stats) {
        tiles += st.tiles - st.blank;
        ft.page_info.push(sleuth_core::protocol::PageInfo { page: p.page, class: "image".into(), chars: 0, image_coverage: 1.0, ocr_tiles: st.tiles, ocr_tiles_skipped: st.blank, ocr_millis: 0, note: String::new() });
        ft.pages.push(p);
    }
    Ok((ft, tiles))
}

/// `--image FILE`: one scanned image file.
fn run_image(path: &str, r: &OcrReader, repeat: usize, dump: &mut BTreeMap<String, serde_json::Value>) -> anyhow::Result<()> {
    let path = std::path::Path::new(path);
    let (mut wall, mut cpu) = (f64::MAX, f64::MAX);
    let mut last = None;
    for _ in 0..repeat {
        let (t, c) = (Instant::now(), proc::cpu_seconds());
        last = Some(read_image(path, r)?);
        wall = wall.min(t.elapsed().as_secs_f64());
        cpu = cpu.min(proc::cpu_seconds() - c);
    }
    let (ft, _) = last.expect("at least one pass");
    println!("| page | tiles | blank | lines |
|---:|---:|---:|---:|");
    for (p, info) in ft.pages.iter().zip(&ft.page_info) {
        println!("| {} | {} | {} | {} |", p.page, info.ocr_tiles, info.ocr_tiles_skipped, p.lines.len());
        dump.insert(format!("{}", p.page), dump_lines(&p.lines));
    }
    let n = ft.pages.len().max(1) as f64;
    println!("
{} page(s): {:.2} s wall, {:.2} CPU-seconds a page, best of {repeat} pass(es)", ft.pages.len(), wall / n, cpu / n);
    Ok(())
}

/// `--pages LIST`: whole-page OCR of single pages of real files.
fn run_pages(list: &str, r: &OcrReader, repeat: usize, dump: &mut BTreeMap<String, serde_json::Value>) -> anyhow::Result<()> {
    let mut pages: Vec<(String, u32)> = Vec::new();
    for l in std::fs::read_to_string(list)?.lines() {
        let l = l.trim_end_matches('\r');
        if l.trim().is_empty() || l.starts_with('#') {
            continue;
        }
        let (p, n) = l.rsplit_once('\t').ok_or_else(|| anyhow::anyhow!("expected path<TAB>page: {l}"))?;
        pages.push((p.to_string(), n.trim().parse()?));
    }
    println!("| # | size (pt) | tiles | blank | lines | wall s | CPU s |");
    println!("|---:|---|---:|---:|---:|---:|---:|");
    let mut best = vec![(f64::MAX, f64::MAX); pages.len()];
    for pass in 0..repeat {
        for (i, (path, page)) in pages.iter().enumerate() {
            let doc = Doc::open(std::path::Path::new(path))?;
            let (pw, ph) = doc.page_size(page - 1)?;
            let (t, c) = (Instant::now(), proc::cpu_seconds());
            let (pt, st) = r.read_regions(&doc, page - 1, 300, &[], &|| false)?;
            let (wall, cpu) = (t.elapsed().as_secs_f64(), proc::cpu_seconds() - c);
            best[i] = (best[i].0.min(wall), best[i].1.min(cpu));
            if pass == 0 {
                // Real file names stay out of the output: pages are numbered.
                println!("| {} | {pw:.0}x{ph:.0} | {} | {} | {} | {wall:.1} | {cpu:.1} |", i + 1, st.tiles, st.blank, pt.lines.len());
                dump.insert(format!("{:03}", i + 1), dump_lines(&pt.lines));
            }
        }
    }
    let (wall, cpu): (f64, f64) = (best.iter().map(|b| b.0).sum(), best.iter().map(|b| b.1).sum());
    let n = pages.len().max(1) as f64;
    println!("| **total, best of {repeat}** | {} pages | | | | **{wall:.1}** | **{cpu:.1}** |", pages.len());
    println!("\nPer page: {:.2} s wall, {:.2} CPU-seconds ({:.0} pages an hour per core)", wall / n, cpu / n, 3600.0 / (cpu / n));
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mode = arg("--mode").unwrap_or_else(|| "new".into());
    let quality = if arg("--quality").as_deref() == Some("high") { OcrQuality::High } else { OcrQuality::Standard };
    let repeat: usize = num("--repeat").unwrap_or(1).max(1);
    let (set, _) = models::choose(quality)?;
    let opts = options()?;
    let t = Instant::now();
    let r = OcrReader::build(&set, &opts)?;
    println!(
        "# Sleuth reader benchmark\n\nmode `{mode}`, {} on {}, {} threads, models loaded in {:.1} s\n\nOCR settings: `{}`\n",
        set.name,
        opts.backend.label(),
        opts.threads,
        t.elapsed().as_secs_f32(),
        sleuth_engine::ocr::config_signature(&opts)
    );
    let mut dump: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    if let Some(list) = arg("--pages") {
        run_pages(&list, &r, repeat, &mut dump)?;
    } else if let Some(image) = arg("--image") {
        run_image(&image, &r, repeat, &mut dump)?;
    } else {
        let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("corpus folder"));
        run_corpus(&dir, &mode, &r, repeat, &mut dump)?;
    }
    print_timings(&r.take_timings(), repeat);
    let (ws, commit) = proc::peak_mb();
    println!("\nPeak memory of this process: {ws:.0} MB working set, {commit:.0} MB committed");
    if let Some(path) = arg("--dump") {
        std::fs::write(&path, serde_json::to_string_pretty(&dump)?)?;
    }
    Ok(())
}

fn run_corpus(dir: &std::path::Path, mode: &str, r: &OcrReader, repeat: usize, dump: &mut BTreeMap<String, serde_json::Value>) -> anyhow::Result<()> {
    let truth: BTreeMap<String, Vec<String>> = serde_json::from_str(&std::fs::read_to_string(dir.join("truth.json"))?)?;
    println!("| file | page classes | OCR tiles | seconds | CPU s | exact | verify | partial | missed |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|");
    let d = Deriver::new(&Settings::default());
    let has = |l: &[Line]| d.has_drawing_number(l);
    let (mut tot_s, mut tot_cpu, mut tot_tiles, mut tot_exact, mut tot_verify, mut tot_partial, mut tot_missed) = (0.0f64, 0.0f64, 0u32, 0, 0, 0, 0);
    let mut ocr_pages = 0usize;
    let mut missed_list = Vec::new();
    for (file, tags) in &truth {
        let path = dir.join(file);
        let (mut secs, mut cpu) = (f64::MAX, f64::MAX);
        let mut last = None;
        for _ in 0..repeat {
            let (t, c) = (Instant::now(), proc::cpu_seconds());
            let (ft, tiles) = if is_image(&path) {
                read_image(&path, r)?
            } else if mode == "old" {
                read_old(&path, r, 300)?
            } else {
                let ft = reader::read_pdf(&path, Some(r), &reader::ReadOptions { dpi: 300, cancel: &|| false, title_block_ocr: true, has_drawing_number: &has })?;
                let tiles = ft.page_info.iter().map(|p| p.ocr_tiles - p.ocr_tiles_skipped).sum();
                (ft, tiles)
            };
            secs = secs.min(t.elapsed().as_secs_f64());
            cpu = cpu.min(proc::cpu_seconds() - c);
            last = Some((ft, tiles));
        }
        let (ft, tiles) = last.expect("at least one pass");
        ocr_pages += ft.page_info.iter().filter(|p| p.ocr_tiles > 0).count();
        for p in &ft.pages {
            dump.insert(format!("{file}|{}", p.page), dump_lines(&p.lines));
        }
        let classes: Vec<String> = ft.page_info.iter().map(|p| p.class.clone()).collect();
        let (mut exact, mut verify, mut partial, mut missed) = (0, 0, 0, 0);
        for tag in tags {
            let term = Term::new(tag).unwrap();
            let best = ft.pages.iter().flat_map(|p| &p.lines).filter_map(|l| term.classify(&l.text, l.conf)).min();
            match best {
                Some(MatchClass::Exact | MatchClass::SeparatorsIgnored) => exact += 1,
                Some(MatchClass::Uncertain) => verify += 1,
                Some(MatchClass::Partial) => {
                    partial += 1;
                    missed_list.push(format!("{file}: {tag} (part read)"));
                }
                None => {
                    missed += 1;
                    missed_list.push(format!("{file}: {tag}"));
                }
            }
        }
        println!("| {file} | {} | {tiles} | {secs:.1} | {cpu:.1} | {exact} | {verify} | {partial} | {missed} |", if classes.is_empty() { "-".into() } else { classes.join(", ") });
        tot_s += secs;
        tot_cpu += cpu;
        tot_tiles += tiles;
        tot_exact += exact;
        tot_verify += verify;
        tot_partial += partial;
        tot_missed += missed;
    }
    println!("| **total** | | **{tot_tiles}** | **{tot_s:.1}** | **{tot_cpu:.1}** | **{tot_exact}** | **{tot_verify}** | **{tot_partial}** | **{tot_missed}** |");
    if !missed_list.is_empty() {
        println!("\nNot found exactly: {}", missed_list.join("; "));
    }
    if ocr_pages > 0 {
        let per = tot_cpu / ocr_pages as f64;
        println!("\n{ocr_pages} pages needed OCR: {per:.2} CPU-seconds a page ({:.0} pages an hour per core), best of {repeat} pass(es)", 3600.0 / per);
    }
    Ok(())
}
