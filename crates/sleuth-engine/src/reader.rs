//! Read every page of one PDF: the text layer first, then OCR only where the
//! page needs it.
//!
//! Each page is classified from what the PDF says about it:
//!
//! | class            | what it is                              | OCR                         |
//! |------------------|-----------------------------------------|-----------------------------|
//! | searchable       | plotted text, few or no images          | none (title block if needed)|
//! | hybrid           | real text plus raster images (stamps,   | only the image areas        |
//! |                  | scanned mark-ups, pasted details)       |                             |
//! | scanned          | an image and (almost) no text           | whole page                  |
//! | vector_outlined  | many vector paths, no text: text was    | whole page                  |
//! |                  | plotted as outlines                     |                             |
//! | sparse           | little of anything                      | whole page (blank tiles are |
//! |                  |                                         | skipped)                    |
//! | damaged          | the page cannot be loaded               | none; reason recorded       |
//!
//! OCR lines that repeat text already in the text layer are dropped: the text
//! layer is exact, OCR is not.

use crate::ocr::{OcrReader, TileStats};
use crate::pdf::{Doc, PageFacts};
use crate::text::{FileText, Line, READER_VERSION};
use anyhow::Result;
use sleuth_core::analyzer::sep_form;
use sleuth_core::protocol::{NormBox, PageInfo, Provenance, TextSource};
use std::path::Path;
use std::time::Instant;

/// Minimum characters a text layer needs before a page is trusted as text.
pub const MIN_TEXT_CHARS: usize = 40;
/// Images covering at least this share of the page make a text page hybrid.
pub const HYBRID_IMAGE_COVERAGE: f32 = 0.01;
/// Images covering this share of a page with no text make it a scan.
pub const SCAN_IMAGE_COVERAGE: f32 = 0.3;
/// More vector paths than this, with no text, means outlined text.
pub const OUTLINED_PATHS: usize = 300;
/// Pages at least this long on their longer side (points) are drawings with
/// a title block (A3 and up).
pub const DRAWING_SIDE_PT: f32 = 1100.0;
/// Where title blocks sit: the bottom-right of the sheet.
const TITLE_BLOCK: NormBox = NormBox { x: 0.5, y: 0.62, w: 0.5, h: 0.38 };
/// Changes how pages are classified and merged; part of the fingerprint.
pub const PREPROCESS_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageClass {
    Searchable,
    Hybrid,
    Scanned,
    VectorOutlined,
    Sparse,
}

impl PageClass {
    pub fn name(self) -> &'static str {
        match self {
            PageClass::Searchable => "searchable",
            PageClass::Hybrid => "hybrid",
            PageClass::Scanned => "scanned",
            PageClass::VectorOutlined => "vector_outlined",
            PageClass::Sparse => "sparse",
        }
    }
}

/// Decide a page's class from its facts. Thresholds are signals combined
/// here; none of them alone forces whole-page OCR.
pub fn classify(f: &PageFacts) -> PageClass {
    let has_text = f.chars >= MIN_TEXT_CHARS;
    if has_text {
        if f.image_coverage >= HYBRID_IMAGE_COVERAGE { PageClass::Hybrid } else { PageClass::Searchable }
    } else if f.image_coverage >= SCAN_IMAGE_COVERAGE {
        PageClass::Scanned
    } else if f.paths >= OUTLINED_PATHS {
        PageClass::VectorOutlined
    } else if f.image_coverage > 0.0 {
        PageClass::Scanned
    } else {
        PageClass::Sparse
    }
}

pub struct ReadOptions<'a> {
    pub dpi: u32,
    pub cancel: &'a dyn Fn() -> bool,
    /// OCR the title block of plotted drawings whose text has no drawing
    /// number (see `derive::has_drawing_number`).
    pub title_block_ocr: bool,
    /// Decides whether a page's text already holds a drawing number.
    pub has_drawing_number: &'a dyn Fn(&[Line]) -> bool,
}

/// Everything that decides what reading produces. Two PCs with the same
/// fingerprint produce the same result for the same file, so the shared
/// cache can reuse it; anything else is read again.
pub fn pipeline_fingerprint(model: &str, model_hash: &str, ocr_config: &str, dpi: u32, rules: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(
        format!(
            "reader={READER_VERSION};pre={PREPROCESS_VERSION};schema=2;model={model}:{model_hash};ocr={ocr_config};dpi={dpi};text={MIN_TEXT_CHARS}/{HYBRID_IMAGE_COVERAGE}/{SCAN_IMAGE_COVERAGE}/{OUTLINED_PATHS};title={rules}"
        )
        .as_bytes(),
    );
    h.finalize().to_hex().to_string()
}

/// `ocr` is created lazily by the caller; pass `None` to read text layers only.
pub fn read_pdf(path: &Path, ocr: Option<&OcrReader>, opts: &ReadOptions) -> Result<FileText> {
    read_pdf_lanes(path, ocr, opts).map(|(text, _)| text)
}

/// As [`read_pdf`], also saying whether any page wanted OCR that was not
/// done because `ocr` is `None` (the fast lane then hands the file to the
/// OCR lane).
pub fn read_pdf_lanes(path: &Path, ocr: Option<&OcrReader>, opts: &ReadOptions) -> Result<(FileText, bool)> {
    let doc = Doc::open(path)?;
    let mut wants_ocr = false;
    let mut out = FileText { reader_version: READER_VERSION, model: String::new(), pages: Vec::new(), page_info: Vec::new(), provenance: Provenance::default() };
    let n = doc.page_count();
    let mut damaged = 0u32;
    for i in 0..n {
        if (opts.cancel)() {
            anyhow::bail!("cancelled");
        }
        let (mut page, facts) = match doc.analyze(i) {
            Ok(x) => x,
            Err(e) => {
                damaged += 1;
                out.page_info.push(PageInfo { page: i + 1, class: "damaged".into(), chars: 0, image_coverage: 0.0, ocr_tiles: 0, ocr_tiles_skipped: 0, ocr_millis: 0, note: format!("{e:#}"), ..Default::default() });
                continue;
            }
        };
        let class = classify(&facts);
        let mut info = PageInfo { page: i + 1, class: class.name().into(), chars: facts.chars as u32, image_coverage: facts.image_coverage, ocr_tiles: 0, ocr_tiles_skipped: 0, ocr_millis: 0, note: String::new(), width_pt: facts.width_pt, height_pt: facts.height_pt };
        let regions: Option<Vec<NormBox>> = match class {
            PageClass::Searchable => {
                let drawing = facts.width_pt.max(facts.height_pt) >= DRAWING_SIDE_PT;
                if opts.title_block_ocr && i == 0 && drawing && !(opts.has_drawing_number)(&page.lines) {
                    info.note = "no drawing number in the text; title block read with OCR".into();
                    Some(vec![TITLE_BLOCK])
                } else {
                    None
                }
            }
            PageClass::Hybrid => {
                info.note = format!("{} image area(s) read with OCR", facts.image_rects.len());
                Some(facts.image_rects.clone())
            }
            PageClass::Scanned | PageClass::VectorOutlined | PageClass::Sparse => Some(Vec::new()),
        };
        wants_ocr |= regions.is_some() && ocr.is_none();
        if let (Some(regions), Some(r)) = (regions, ocr) {
            let t = Instant::now();
            let (ocr_page, stats): (_, TileStats) = r.read_regions(&doc, i, opts.dpi, &regions, opts.cancel)?;
            info.ocr_millis = t.elapsed().as_millis() as u64;
            info.ocr_tiles = stats.tiles;
            info.ocr_tiles_skipped = stats.blank;
            out.model = r.model.clone();
            let native: Vec<Line> = page.lines.clone();
            page.lines.extend(ocr_page.lines.into_iter().filter(|l| !repeats_native(l, &native)));
        }
        out.page_info.push(info);
        out.pages.push(page);
    }
    if n > 0 && damaged == n {
        anyhow::bail!("damaged or not a PDF: no page could be loaded");
    }
    Ok((out, wants_ocr))
}

/// An OCR line sitting on text-layer text that says the same thing (or
/// covers it almost entirely) adds nothing.
fn repeats_native(l: &Line, native: &[Line]) -> bool {
    if l.source != TextSource::Ocr {
        return false;
    }
    let s = sep_form(&l.text);
    native.iter().any(|n| {
        let ov = l.bbox.overlap(&n.bbox);
        ov >= 0.5 && (sep_form(&n.text).contains(&s) || ov >= 0.9)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(chars: usize, cov: f32, paths: usize) -> PageFacts {
        PageFacts { chars, image_coverage: cov, paths, width_pt: 1190.0, height_pt: 842.0, image_rects: if cov > 0.0 { vec![NormBox { x: 0.1, y: 0.1, w: 0.2, h: 0.2 }] } else { vec![] } }
    }

    #[test]
    fn classes() {
        assert_eq!(classify(&facts(500, 0.0, 4000)), PageClass::Searchable);
        assert_eq!(classify(&facts(500, 0.05, 10)), PageClass::Hybrid);
        // a scanner's invisible text layer over a full-page image is still hybrid:
        // the image area (the whole page) is read
        assert_eq!(classify(&facts(800, 1.0, 0)), PageClass::Hybrid);
        assert_eq!(classify(&facts(0, 0.98, 0)), PageClass::Scanned);
        assert_eq!(classify(&facts(5, 0.0, 2000)), PageClass::VectorOutlined);
        assert_eq!(classify(&facts(0, 0.0, 3)), PageClass::Sparse);
        assert_eq!(classify(&facts(10, 0.1, 3)), PageClass::Scanned);
    }

    #[test]
    fn ocr_repeating_native_text_is_dropped() {
        let b = NormBox { x: 0.1, y: 0.1, w: 0.2, h: 0.02 };
        let native = vec![Line { text: "PSV-1203".into(), conf: 1.0, source: TextSource::TextLayer, bbox: b, alt: false }];
        let same = Line { text: "PSV-12O3".into(), conf: 0.9, source: TextSource::Ocr, bbox: b, alt: false };
        let elsewhere = Line { bbox: NormBox { x: 0.6, ..b }, ..same.clone() };
        assert!(repeats_native(&same, &native), "covers the same text box");
        assert!(!repeats_native(&elsewhere, &native));
        let part = Line { text: "1203".into(), bbox: NormBox { x: 0.2, w: 0.1, ..b }, ..same.clone() };
        assert!(repeats_native(&part, &native));
    }

    #[test]
    fn fingerprint_changes_with_model_and_config() {
        let a = pipeline_fingerprint("m", "h1", "tile=1", 300, "r");
        assert_eq!(a, pipeline_fingerprint("m", "h1", "tile=1", 300, "r"));
        assert_ne!(a, pipeline_fingerprint("m", "h2", "tile=1", 300, "r"));
        assert_ne!(a, pipeline_fingerprint("m", "h1", "tile=1", 400, "r"));
        assert_ne!(a, pipeline_fingerprint("m", "h1", "tile=2", 300, "r"));
    }
}
