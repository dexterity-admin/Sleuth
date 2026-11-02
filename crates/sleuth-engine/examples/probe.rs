//! Read one PDF and print what was found:
//! `cargo run --release --example probe -- <file.pdf> [dpi]`
//!
//! Environment: `SLEUTH_QUALITY=high`, `SLEUTH_BACKEND=directml`,
//! `SLEUTH_TILE`, `SLEUTH_OVERLAP`, `SLEUTH_THREADS`, `SLEUTH_DET`.
use sleuth_core::settings::{OcrQuality, Settings};
use sleuth_engine::derive::Deriver;
use sleuth_engine::ocr::{Backend, OcrOptions, OcrReader};
use sleuth_engine::{models, reader};
use std::time::Instant;

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("pdf path");
    let dpi: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let quality = match std::env::var("SLEUTH_QUALITY").as_deref() {
        Ok("high") => OcrQuality::High,
        _ => OcrQuality::Standard,
    };
    let (set, _) = models::choose(quality)?;
    let t = Instant::now();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1);
    let opts = OcrOptions {
        threads: env("SLEUTH_THREADS", cores),
        tile: env("SLEUTH_TILE", sleuth_engine::ocr::TILE),
        overlap: env("SLEUTH_OVERLAP", sleuth_engine::ocr::OVERLAP),
        detect_side: env("SLEUTH_DET", sleuth_engine::ocr::DETECT_SIDE),
        backend: if std::env::var("SLEUTH_BACKEND").as_deref() == Ok("directml") { Backend::DirectMl } else { Backend::Cpu },
    };
    let r = OcrReader::build(&set, &opts)?;
    eprintln!("model {} on {} loaded in {:.1}s", r.model, opts.backend.label(), t.elapsed().as_secs_f32());
    let t = Instant::now();
    let never = || false;
    let d = Deriver::new(&Settings::default());
    let has = |l: &[sleuth_engine::text::Line]| d.has_drawing_number(l);
    let ft = reader::read_pdf(path.as_ref(), Some(&r), &reader::ReadOptions { dpi, cancel: &never, title_block_ocr: true, has_drawing_number: &has })?;
    eprintln!("read {} page(s) in {:.1}s", ft.pages.len(), t.elapsed().as_secs_f32());
    for i in &ft.page_info {
        eprintln!("page {}: {} ({} chars, images {:.0}%), {} tiles ({} blank), {} ms {}", i.page, i.class, i.chars, i.image_coverage * 100.0, i.ocr_tiles, i.ocr_tiles_skipped, i.ocr_millis, i.note);
    }
    for p in &ft.pages {
        for l in &p.lines {
            println!("p{} {:?} {:.2}{} [{:.3},{:.3} {:.3}x{:.3}] | {}", p.page, l.source, l.conf, if l.alt { " ALT" } else { "" }, l.bbox.x, l.bbox.y, l.bbox.w, l.bbox.h, l.text);
        }
    }
    Ok(())
}
