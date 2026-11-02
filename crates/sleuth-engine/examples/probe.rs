//! Read one PDF and print what was found: `cargo run --release --example probe -- <file.pdf> [dpi]`
use sleuth_core::settings::OcrQuality;
use sleuth_engine::{models, ocr::OcrReader, reader};
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("pdf path");
    let dpi: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let quality = match std::env::var("SLEUTH_QUALITY").as_deref() { Ok("high") => OcrQuality::High, _ => OcrQuality::Standard };
    let (set, _) = models::choose(quality)?;
    let t = Instant::now();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1);
    let tile: i32 = std::env::var("SLEUTH_TILE").ok().and_then(|v| v.parse().ok()).unwrap_or(sleuth_engine::ocr::TILE);
    let overlap: i32 = std::env::var("SLEUTH_OVERLAP").ok().and_then(|v| v.parse().ok()).unwrap_or(sleuth_engine::ocr::OVERLAP);
    let threads = std::env::var("SLEUTH_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(threads);
    let det: u32 = std::env::var("SLEUTH_DET").ok().and_then(|v| v.parse().ok()).unwrap_or(sleuth_engine::ocr::DETECT_SIDE);
    let r = OcrReader::with_tile(&set, threads, tile, overlap, det)?;
    eprintln!("model {} loaded in {:.1}s", r.model, t.elapsed().as_secs_f32());
    let t = Instant::now();
    let never = || false;
    let ft = reader::read_pdf(path.as_ref(), Some(&r), &reader::ReadOptions { dpi, cancel: &never })?;
    eprintln!("read {} page(s) in {:.1}s", ft.pages.len(), t.elapsed().as_secs_f32());
    for p in &ft.pages {
        for l in &p.lines {
            println!("p{} {:?} {:.2}{} [{:.3},{:.3} {:.3}x{:.3}] | {}", p.page, l.source, l.conf, if l.alt { " ALT" } else { "" }, l.bbox.x, l.bbox.y, l.bbox.w, l.bbox.h, l.text);
        }
    }
    Ok(())
}
