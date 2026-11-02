//! Read every page of one PDF: text layer first, OCR only where needed.

use crate::ocr::OcrReader;
use crate::pdf::Doc;
use crate::text::{FileText, READER_VERSION};
use anyhow::Result;
use std::path::Path;

pub struct ReadOptions<'a> {
    pub dpi: u32,
    pub cancel: &'a dyn Fn() -> bool,
}

/// `ocr` is created lazily by the caller; pass `None` to read text layers only.
pub fn read_pdf(path: &Path, ocr: Option<&OcrReader>, opts: &ReadOptions) -> Result<FileText> {
    let doc = Doc::open(path)?;
    let mut out = FileText { reader_version: READER_VERSION, model: String::new(), pages: Vec::new() };
    for i in 0..doc.page_count() {
        if (opts.cancel)() {
            anyhow::bail!("cancelled");
        }
        let (mut page, needs_ocr) = doc.text_layer(i)?;
        if needs_ocr && let Some(r) = ocr {
            let ocr_page = r.read_page(&doc, i, opts.dpi, opts.cancel)?;
            out.model = r.model.clone();
            page.lines.extend(ocr_page.lines);
        }
        out.pages.push(page);
    }
    Ok(out)
}
