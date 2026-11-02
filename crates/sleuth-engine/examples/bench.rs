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
use sleuth_core::analyzer::{MatchClass, Term};
use sleuth_core::settings::{OcrQuality, Settings};
use sleuth_engine::derive::Deriver;
use sleuth_engine::ocr::{Backend, OcrOptions, OcrReader};
use sleuth_engine::pdf::Doc;
use sleuth_engine::text::FileText;
use sleuth_engine::{models, reader};
use std::collections::BTreeMap;
use std::time::Instant;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
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

fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("corpus folder"));
    let mode = arg("--mode").unwrap_or_else(|| "new".into());
    let backend = if arg("--backend").as_deref() == Some("directml") { Backend::DirectMl } else { Backend::Cpu };
    let quality = if arg("--quality").as_deref() == Some("high") { OcrQuality::High } else { OcrQuality::Standard };
    let truth: BTreeMap<String, Vec<String>> = serde_json::from_str(&std::fs::read_to_string(dir.join("truth.json"))?)?;
    let (set, _) = models::choose(quality)?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1);
    let t = Instant::now();
    let r = OcrReader::build(&set, &OcrOptions { threads, backend, ..Default::default() })?;
    println!("# Sleuth reader benchmark\n\nmode `{mode}`, {} on {}, {threads} threads, models loaded in {:.1} s\n", set.name, backend.label(), t.elapsed().as_secs_f32());
    println!("| file | page classes | OCR tiles | seconds | exact | verify | partial | missed |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    let d = Deriver::new(&Settings::default());
    let has = |l: &[sleuth_engine::text::Line]| d.has_drawing_number(l);
    let (mut tot_s, mut tot_tiles, mut tot_exact, mut tot_verify, mut tot_partial, mut tot_missed) = (0.0f64, 0u32, 0, 0, 0, 0);
    let mut missed_list = Vec::new();
    for (file, tags) in &truth {
        let path = dir.join(file);
        let t = Instant::now();
        let (ft, tiles) = if mode == "old" {
            read_old(&path, &r, 300)?
        } else {
            let ft = reader::read_pdf(&path, Some(&r), &reader::ReadOptions { dpi: 300, cancel: &|| false, title_block_ocr: true, has_drawing_number: &has })?;
            let tiles = ft.page_info.iter().map(|p| p.ocr_tiles - p.ocr_tiles_skipped).sum();
            (ft, tiles)
        };
        let secs = t.elapsed().as_secs_f64();
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
        println!("| {file} | {} | {tiles} | {secs:.1} | {exact} | {verify} | {partial} | {missed} |", if classes.is_empty() { "-".into() } else { classes.join(", ") });
        tot_s += secs;
        tot_tiles += tiles;
        tot_exact += exact;
        tot_verify += verify;
        tot_partial += partial;
        tot_missed += missed;
    }
    println!("| **total** | | **{tot_tiles}** | **{tot_s:.1}** | **{tot_exact}** | **{tot_verify}** | **{tot_partial}** | **{tot_missed}** |");
    if !missed_list.is_empty() {
        println!("\nNot found exactly: {}", missed_list.join("; "));
    }
    Ok(())
}
