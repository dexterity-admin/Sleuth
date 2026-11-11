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

/// What the page itself says about how it was made, used to decide whether
/// and where it needs OCR (see `reader::classify`).
#[derive(Debug, Clone, Default)]
pub struct PageFacts {
    /// Non-blank characters in the text layer.
    pub chars: usize,
    /// Raster images on the page (normalised, origin top-left).
    pub image_rects: Vec<NormBox>,
    /// Share of the page area covered by images (overlaps counted twice).
    pub image_coverage: f32,
    /// Vector path objects: many paths and no text usually means text was
    /// plotted as outlines.
    pub paths: usize,
    pub width_pt: f32,
    pub height_pt: f32,
}

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

    /// Text-layer lines of one page, and the facts used to classify it.
    pub fn analyze(&self, index: u32) -> Result<(PageText, PageFacts)> {
        let page = self.page(index)?;
        let (pw, ph) = (page.width().value.max(1.0), page.height().value.max(1.0));
        let mut segs: Vec<(String, NormBox)> = Vec::new();
        let mut facts = PageFacts { width_pt: pw, height_pt: ph, ..Default::default() };
        if let Ok(text) = page.text() {
            for seg in text.segments().iter() {
                let t = seg.text();
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    continue;
                }
                facts.chars += trimmed.chars().filter(|c| !c.is_whitespace()).count();
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
        let to_norm = |q: &PdfQuadPoints| {
            let (l, r) = (q.left().value.min(q.right().value), q.left().value.max(q.right().value));
            let (b, t) = (q.bottom().value.min(q.top().value), q.bottom().value.max(q.top().value));
            let x0 = (l / pw).clamp(0.0, 1.0);
            let x1 = (r / pw).clamp(0.0, 1.0);
            let y0 = (1.0 - t / ph).clamp(0.0, 1.0);
            let y1 = (1.0 - b / ph).clamp(0.0, 1.0);
            NormBox { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
        };
        for obj in page.objects().iter() {
            match obj.object_type() {
                PdfPageObjectType::Image => {
                    if let Ok(q) = obj.bounds() {
                        facts.image_rects.push(to_norm(&q));
                    }
                }
                PdfPageObjectType::Path => facts.paths += 1,
                PdfPageObjectType::XObjectForm => {
                    // Child bounds are in the form's own space; when a form
                    // holds an image, treat the whole form area as an image.
                    if let Some(form) = obj.as_x_object_form_object() {
                        let mut has_image = false;
                        for child in form.iter() {
                            match child.object_type() {
                                PdfPageObjectType::Image => has_image = true,
                                PdfPageObjectType::Path => facts.paths += 1,
                                _ => {}
                            }
                        }
                        if has_image && let Ok(q) = obj.bounds() {
                            facts.image_rects.push(to_norm(&q));
                        }
                    }
                }
                _ => {}
            }
        }
        // Overlapping images count twice; coverage is a signal, not a measure.
        facts.image_coverage = facts.image_rects.iter().map(|r| r.area()).sum::<f32>();
        let lines = merge_segments(segs)
            .into_iter()
            .map(|(text, bbox)| Line { text, conf: 1.0, source: TextSource::TextLayer, bbox, alt: false })
            .collect();
        Ok((PageText { page: index + 1, lines }, facts))
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

    /// Load a page to render rectangles of it at `scale` pixels per point.
    /// While it stays loaded PDFium keeps what it has decoded, so a scanned
    /// page read in several bands is decompressed once, not once per band.
    pub fn raster(&self, index: u32, scale: f32) -> Result<PageRaster<'_>> {
        Ok(PageRaster { page: self.page(index)?, scale })
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

/// One loaded page, rendered a rectangle at a time (see [`Doc::raster`]).
pub struct PageRaster<'a> {
    page: PdfPage<'a>,
    scale: f32,
}

impl PageRaster<'_> {
    /// Render a rectangle of the page; `(x, y, w, h)` are in output pixels of
    /// the whole-page render. PDFium draws into three bytes a pixel and the
    /// one copy taken out of it is turned into RGB in place, where
    /// [`Doc::render_tile`] goes through four copies at four bytes a pixel.
    pub fn render(&self, x: i32, y: i32, w: i32, h: i32) -> Result<RgbImage> {
        let mut bitmap = PdfBitmap::empty(w, h, PdfBitmapFormat::BGR).map_err(|e| anyhow!("{e}"))?;
        let cfg = PdfRenderConfig::new()
            .scale_page_by_factor(self.scale)
            .set_origin(-x, -y)
            .clear_before_rendering(true)
            .render_annotations(true)
            .render_form_data(true);
        self.page.render_into_bitmap_with_config(&mut bitmap, &cfg).map_err(|e| anyhow!("{e}"))?;
        let mut raw = bitmap.as_raw_bytes();
        drop(bitmap);
        let (w, h) = (w.max(0) as usize, h.max(0) as usize);
        bgr_rows_to_rgb(&mut raw, w, h)?;
        RgbImage::from_raw(w as u32, h as u32, raw).ok_or_else(|| anyhow!("rendered bitmap has the wrong size"))
    }
}

/// Turn PDFium's BGR rows (each padded to a multiple of four bytes) into
/// tightly packed RGB, in place.
fn bgr_rows_to_rgb(raw: &mut Vec<u8>, w: usize, h: usize) -> Result<()> {
    if h == 0 || w == 0 {
        raw.clear();
        return Ok(());
    }
    let stride = raw.len() / h;
    if stride < w * 3 || raw.len() != stride * h {
        return Err(anyhow!("rendered bitmap has the wrong size"));
    }
    for row in 0..h {
        if stride != w * 3 {
            raw.copy_within(row * stride..row * stride + w * 3, row * w * 3);
        }
        for px in raw[row * w * 3..(row + 1) * w * 3].as_chunks_mut::<3>().0 {
            px.swap(0, 2);
        }
    }
    raw.truncate(w * h * 3);
    Ok(())
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

    #[test]
    fn padded_bgr_rows_become_packed_rgb() {
        // Two pixels a row: six bytes of colour, two of padding.
        let mut raw = vec![1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0];
        bgr_rows_to_rgb(&mut raw, 2, 2).unwrap();
        assert_eq!(raw, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
        // Four pixels a row need no padding.
        let mut raw: Vec<u8> = (0..12).collect();
        bgr_rows_to_rgb(&mut raw, 4, 1).unwrap();
        assert_eq!(raw, vec![2, 1, 0, 5, 4, 3, 8, 7, 6, 11, 10, 9]);
        assert!(bgr_rows_to_rgb(&mut vec![0; 5], 2, 1).is_err());
    }
}
