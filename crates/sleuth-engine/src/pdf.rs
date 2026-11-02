//! PDF access through PDFium: text-layer lines with positions, the decision
//! whether a page needs OCR, and rendering of OCR tiles and previews.

use crate::text::{Line, PageText};
use anyhow::{Context, Result, anyhow};
use sleuth_core::protocol::{NormBox, TextSource};
use image::RgbImage;
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PDFIUM: OnceLock<std::result::Result<Pdfium, String>> = OnceLock::new();

/// Folders searched for `pdfium.dll`: next to the executable, then the
/// development `vendor` folder.
fn pdfium_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.to_path_buf());
        out.push(dir.join("resources"));
    }
    out.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium/bin"));
    out
}

pub fn pdfium() -> Result<&'static Pdfium> {
    let r = PDFIUM.get_or_init(|| {
        for dir in pdfium_candidates() {
            let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
            if lib.exists()
                && let Ok(b) = Pdfium::bind_to_library(&lib)
            {
                return Ok(Pdfium::new(b));
            }
        }
        Err("pdfium.dll was not found next to Sleuth".to_string())
    });
    r.as_ref().map_err(|e| anyhow!(e.clone()))
}

/// Minimum characters a text layer needs before a page is trusted without OCR.
const MIN_TEXT_CHARS: usize = 40;
/// A page whose images cover this share of its area is treated as a scan.
const SCAN_IMAGE_COVERAGE: f32 = 0.4;

pub struct Doc {
    doc: PdfDocument<'static>,
}

impl Doc {
    pub fn open(path: &Path) -> Result<Doc> {
        let doc = pdfium()?
            .load_pdf_from_file(path, None)
            .map_err(|e| match e {
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                    anyhow!("password protected")
                }
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::FormatError) => anyhow!("damaged or not a PDF"),
                other => anyhow!("cannot open PDF: {other}"),
            })?;
        Ok(Doc { doc })
    }

    pub fn page_count(&self) -> u32 {
        self.doc.pages().len() as u32
    }

    fn page(&self, index: u32) -> Result<PdfPage<'_>> {
        self.doc.pages().get(index as i32).with_context(|| format!("page {} missing", index + 1))
    }

    /// Page size in points (1/72 inch).
    pub fn page_size(&self, index: u32) -> Result<(f32, f32)> {
        let p = self.page(index)?;
        Ok((p.width().value, p.height().value))
    }

    /// Text-layer lines of one page, and whether the page still needs OCR.
    pub fn text_layer(&self, index: u32) -> Result<(PageText, bool)> {
        let page = self.page(index)?;
        let (pw, ph) = (page.width().value.max(1.0), page.height().value.max(1.0));
        let mut segs: Vec<(String, NormBox)> = Vec::new();
        let mut chars = 0usize;
        if let Ok(text) = page.text() {
            for seg in text.segments().iter() {
                let t = seg.text();
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    continue;
                }
                chars += trimmed.chars().filter(|c| !c.is_whitespace()).count();
                let b = seg.bounds();
                let nb = NormBox {
                    x: b.left().value / pw,
                    y: 1.0 - b.top().value / ph,
                    w: (b.right().value - b.left().value) / pw,
                    h: (b.top().value - b.bottom().value) / ph,
                };
                segs.push((trimmed.to_string(), nb));
            }
        }
        let mut image_area = 0.0f32;
        for obj in page.objects().iter() {
            if obj.object_type() == PdfPageObjectType::Image
                && let Ok(b) = obj.bounds()
            {
                image_area += (b.width().value * b.height().value).abs();
            }
        }
        let coverage = image_area / (pw * ph);
        let needs_ocr = chars < MIN_TEXT_CHARS || coverage >= SCAN_IMAGE_COVERAGE;
        let lines = merge_segments(segs)
            .into_iter()
            .map(|(text, bbox)| Line { text, conf: 1.0, source: TextSource::TextLayer, bbox, alt: false })
            .collect();
        Ok((PageText { page: index + 1, lines }, needs_ocr))
    }

    /// Render a rectangle of the page at `scale` pixels per point.
    /// `(x, y, w, h)` are in output pixels of the whole-page render.
    pub fn render_tile(&self, index: u32, scale: f32, x: i32, y: i32, w: i32, h: i32) -> Result<RgbImage> {
        let page = self.page(index)?;
        let mut bitmap = PdfBitmap::empty(w, h, PdfBitmapFormat::BGRA).map_err(|e| anyhow!("{e}"))?;
        let cfg = PdfRenderConfig::new()
            .scale_page_by_factor(scale)
            .set_origin(-x, -y)
            .clear_before_rendering(true)
            .render_annotations(true)
            .render_form_data(true);
        page.render_into_bitmap_with_config(&mut bitmap, &cfg).map_err(|e| anyhow!("{e}"))?;
        let img = bitmap.as_image().map_err(|e| anyhow!("{e}"))?;
        Ok(img.to_rgb8())
    }

    /// Whole-page render fitted inside `max_px` on its longer side.
    pub fn render_preview(&self, index: u32, max_px: u32) -> Result<image::RgbaImage> {
        let page = self.page(index)?;
        let (pw, ph) = (page.width().value, page.height().value);
        let scale = max_px as f32 / pw.max(ph);
        let cfg = PdfRenderConfig::new().scale_page_by_factor(scale).render_annotations(true);
        let bmp = page.render_with_config(&cfg).map_err(|e| anyhow!("{e}"))?;
        Ok(bmp.as_image().map_err(|e| anyhow!("{e}"))?.to_rgba8())
    }
}

/// Join text-layer segments that sit on one line and touch, so a tag split
/// into several runs (`10"`, `-P-1001`, `-A1B`) is matched as one string.
fn merge_segments(mut segs: Vec<(String, NormBox)>) -> Vec<(String, NormBox)> {
    segs.sort_by(|a, b| {
        let ya = a.1.y + a.1.h / 2.0;
        let yb = b.1.y + b.1.h / 2.0;
        ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal).then(a.1.x.partial_cmp(&b.1.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut out: Vec<(String, NormBox)> = Vec::new();
    for (text, b) in segs {
        if let Some((lt, lb)) = out.last_mut() {
            let same_row = ((lb.y + lb.h / 2.0) - (b.y + b.h / 2.0)).abs() < lb.h.max(b.h) * 0.5;
            let gap = b.x - (lb.x + lb.w);
            let close = gap > -lb.h && gap < lb.h.max(b.h) * 1.2;
            if same_row && close {
                if gap > lb.h.max(b.h) * 0.25 {
                    lt.push(' ');
                }
                lt.push_str(&text);
                let x0 = lb.x.min(b.x);
                let y0 = lb.y.min(b.y);
                let x1 = (lb.x + lb.w).max(b.x + b.w);
                let y1 = (lb.y + lb.h).max(b.y + b.h);
                *lb = NormBox { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
                continue;
            }
        }
        out.push((text, b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb(x: f32, y: f32, w: f32) -> NormBox {
        NormBox { x, y, w, h: 0.01 }
    }

    #[test]
    fn split_tag_is_joined() {
        let merged = merge_segments(vec![
            ("-A1B".into(), nb(0.30, 0.5, 0.02)),
            ("10\"".into(), nb(0.20, 0.5, 0.02)),
            ("-P-1001".into(), nb(0.22, 0.5, 0.08)),
            ("OTHER".into(), nb(0.20, 0.7, 0.05)),
        ]);
        assert_eq!(merged[0].0, "10\"-P-1001-A1B");
        assert_eq!(merged.len(), 2);
    }
}
