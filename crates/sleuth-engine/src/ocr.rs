//! OCR of rendered PDF pages with PP-OCRv6 (ONNX Runtime, CPU).
//!
//! Large sheets are rendered in overlapping tiles so small print keeps its
//! full resolution. The overlap is wider than any tag, so every short line is
//! wholly inside at least one tile; duplicates from the overlap are removed.
//! Lines read with low confidence get a second opinion from the OCR engine
//! built into Windows.

use crate::models::ModelSet;
use crate::pdf::Doc;
use crate::text::{Line, PageText};
use anyhow::{Result, anyhow};
use sleuth_core::analyzer::sep_form;
use sleuth_core::protocol::{NormBox, TextSource};
use image::RgbImage;
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::prelude::*;
use oar_ocr::processors::LimitType;

/// Tile edge in pixels. Detection runs on whole tiles, so this sets peak memory.
pub const TILE: i32 = 1600;
/// Wider than any tag at 300 dpi, so a short line always fits wholly in one tile.
pub const OVERLAP: i32 = 512;
/// Text detection runs on the tile scaled to this side; recognition always
/// reads the full-resolution crop.
pub const DETECT_SIDE: u32 = 1600;
/// Pixels from a cut edge within which a region is suspected of being cut.
const EDGE: f32 = 6.0;
/// Below this, ask the Windows reader for a second opinion.
const SECOND_OPINION_BELOW: f32 = 0.85;

pub struct OcrReader {
    ocr: OAROCR,
    pub model: String,
    tile: i32,
    overlap: i32,
}

impl OcrReader {
    pub fn new(models: &ModelSet, threads: usize) -> Result<OcrReader> {
        Self::with_tile(models, threads, TILE, OVERLAP, DETECT_SIDE)
    }

    pub fn with_tile(models: &ModelSet, threads: usize, tile: i32, overlap: i32, detect_side: u32) -> Result<OcrReader> {
        let det = TextDetectionConfig {
            score_threshold: 0.2,
            box_threshold: 0.45,
            unclip_ratio: 1.4,
            max_candidates: 3000,
            limit_side_len: Some(detect_side),
            limit_type: Some(LimitType::Max),
            max_side_len: Some(detect_side),
        };
        let mut ort = oar_ocr::core::config::OrtSessionConfig::default();
        ort.intra_threads = Some(threads.max(1));
        ort.inter_threads = Some(1);
        let mut b = OAROCRBuilder::new(models.det.clone(), models.rec.clone(), models.dict.clone())
            .text_detection_config(det)
            .ort_session(ort)
            .region_batch_size(8);
        if let Some(ori) = &models.textline_ori {
            b = b.with_text_line_orientation_classification(ori.clone());
        }
        let ocr = b.build().map_err(|e| anyhow!("could not load OCR models: {e}"))?;
        Ok(OcrReader { ocr, model: models.name.clone(), tile, overlap })
    }

    /// OCR one page. `cancel` is checked between tiles.
    pub fn read_page(&self, doc: &Doc, index: u32, dpi: u32, cancel: &dyn Fn() -> bool) -> Result<PageText> {
        let (pw, ph) = doc.page_size(index)?;
        let mut scale = dpi as f32 / 72.0;
        // Keep pathological page sizes (banners, A0+ at 400 dpi) bounded.
        let max_side = 20_000.0;
        if pw.max(ph) * scale > max_side {
            scale = max_side / pw.max(ph);
        }
        let (full_w, full_h) = ((pw * scale).ceil() as i32, (ph * scale).ceil() as i32);
        let (tile_px, step) = (self.tile, self.tile - self.overlap);
        let mut found: Vec<Found> = Vec::new();
        let mut y = 0;
        loop {
            let mut x = 0;
            let th = tile_px.min(full_h - y);
            loop {
                if cancel() {
                    return Err(anyhow!("cancelled"));
                }
                let tw = tile_px.min(full_w - x);
                let tile = doc.render_tile(index, scale, x, y, tw, th)?;
                if !is_blank(&tile) {
                    self.read_tile(tile, x, y, full_w, full_h, &mut found)?;
                }
                if x + tw >= full_w {
                    break;
                }
                x += step;
            }
            if y + th >= full_h {
                break;
            }
            y += step;
        }
        let kept = dedupe(found);
        let lines = kept
            .into_iter()
            .flat_map(|f| {
                let bbox = NormBox {
                    x: f.x0 / full_w as f32,
                    y: f.y0 / full_h as f32,
                    w: (f.x1 - f.x0) / full_w as f32,
                    h: (f.y1 - f.y0) / full_h as f32,
                };
                let main = Line { text: f.text.clone(), conf: f.conf, source: TextSource::Ocr, bbox, alt: false };
                let alt = f.alt.map(|t| Line { text: t, conf: f.conf, source: TextSource::Ocr, bbox, alt: true });
                std::iter::once(main).chain(alt)
            })
            .collect();
        Ok(PageText { page: index + 1, lines })
    }

    fn read_tile(&self, tile: RgbImage, ox: i32, oy: i32, full_w: i32, full_h: i32, out: &mut Vec<Found>) -> Result<()> {
        let (tw, th) = (tile.width() as f32, tile.height() as f32);
        let results = self.ocr.predict(vec![tile.clone()]).map_err(|e| anyhow!("OCR failed: {e}"))?;
        let Some(res) = results.into_iter().next() else { return Ok(()) };
        for region in res.text_regions {
            let (Some(text), Some(conf)) = (region.text.as_deref(), region.confidence) else { continue };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let pts = &region.bounding_box.points;
            if pts.is_empty() {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in pts {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            // Interior edges are cuts; the page border is not.
            let cut = (x0 < EDGE && ox > 0)
                || (y0 < EDGE && oy > 0)
                || (x1 > tw - EDGE && ox + (tw as i32) < full_w)
                || (y1 > th - EDGE && oy + (th as i32) < full_h);
            let mut f = Found {
                text: text.to_string(),
                conf,
                x0: x0 + ox as f32,
                y0: y0 + oy as f32,
                x1: x1 + ox as f32,
                y1: y1 + oy as f32,
                cut,
                alt: None,
            };
            if conf < SECOND_OPINION_BELOW
                && let Some(other) = crate::winocr::read_crop(&tile, x0, y0, x1, y1)
            {
                if sep_form(&other) == sep_form(&f.text) {
                    // Two independent readers agree.
                    f.conf = f.conf.max(0.9);
                } else if !sep_form(&other).is_empty() {
                    f.alt = Some(other);
                }
            }
            out.push(f);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Found {
    text: String,
    conf: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    cut: bool,
    alt: Option<String>,
}

impl Found {
    fn area(&self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }
    /// Intersection over the smaller box.
    fn overlap(&self, o: &Found) -> f32 {
        let w = (self.x1.min(o.x1) - self.x0.max(o.x0)).max(0.0);
        let h = (self.y1.min(o.y1) - self.y0.max(o.y0)).max(0.0);
        let small = self.area().min(o.area()).max(1.0);
        w * h / small
    }
}

/// Remove regions read twice in tile overlaps. A region touching a cut edge
/// is dropped when another tile saw the same area; otherwise it is kept so
/// long lines that no tile holds whole are not lost.
fn dedupe(mut found: Vec<Found>) -> Vec<Found> {
    let n = found.len();
    let mut drop = vec![false; n];
    for i in 0..n {
        if !found[i].cut {
            continue;
        }
        for j in 0..n {
            if i != j && !drop[j] && found[i].overlap(&found[j]) >= 0.5 && (!found[j].cut || found[j].area() > found[i].area()) {
                drop[i] = true;
                break;
            }
        }
    }
    for i in 0..n {
        if drop[i] {
            continue;
        }
        for j in (i + 1)..n {
            if !drop[j] && found[i].overlap(&found[j]) >= 0.6 {
                let worse = if found[j].conf > found[i].conf { i } else { j };
                drop[worse] = true;
                if worse == i {
                    break;
                }
            }
        }
    }
    let mut k = 0;
    found.retain(|_| {
        let keep = !drop[k];
        k += 1;
        keep
    });
    found
}

/// A tile with almost no dark pixels has nothing to read.
fn is_blank(img: &RgbImage) -> bool {
    let mut dark = 0usize;
    let total = (img.width() * img.height()) as usize;
    for (i, p) in img.pixels().enumerate() {
        if i % 7 != 0 {
            continue;
        }
        if (p[0] as u16 + p[1] as u16 + p[2] as u16) < 600 {
            dark += 1;
            if dark > total / 7 / 2000 + 4 {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(text: &str, x0: f32, x1: f32, cut: bool, conf: f32) -> Found {
        Found { text: text.into(), conf, x0, y0: 0.0, x1, y1: 10.0, cut, alt: None }
    }

    #[test]
    fn cut_copy_is_dropped_whole_copy_kept() {
        let out = dedupe(vec![f("10\"-P-12", 1500.0, 1600.0, true, 0.9), f("10\"-P-1001-A1B", 1500.0, 1700.0, false, 0.95)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "10\"-P-1001-A1B");
    }

    #[test]
    fn long_cut_line_without_whole_copy_is_kept() {
        let out = dedupe(vec![f("A VERY LONG NOTE", 100.0, 1600.0, true, 0.9)]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn overlap_duplicates_keep_best() {
        let out = dedupe(vec![f("PSV-1203", 10.0, 90.0, false, 0.8), f("PSV-1203", 11.0, 91.0, false, 0.97)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].conf > 0.9);
    }
}
