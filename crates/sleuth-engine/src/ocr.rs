//! OCR of rendered PDF pages and scanned image files with PP-OCRv6 (ONNX
//! Runtime on the CPU, or DirectML where it has passed the check in `accel`).
//!
//! Large sheets are read in overlapping tiles so small print keeps its full
//! resolution. The overlap is wider than any tag, so every short line is
//! wholly inside at least one tile; duplicates from the overlap are removed.
//! Lines read with low confidence get a second opinion from the OCR engine
//! built into Windows.
//!
//! The steps are driven here one by one (detect, crop, orientation,
//! recognise) instead of through `oar_ocr`'s all-in-one call, which is what
//! makes the rest possible:
//!
//! - a page is rendered once (in bands, to bound memory) and tiles are cut
//!   from it, instead of one PDFium render per tile;
//! - text the detector finds in two tiles is recognised once, not twice;
//! - every region is recognised by itself, at its own width, instead of in
//!   batches of eight padded to the widest;
//! - the orientation classifier sees vertical text and lines that read badly,
//!   not every line;
//! - a line cut by a tile edge is also read whole, joined from its parts;
//! - every step is timed (see [`Timings`]).
//!
//! Detection itself is untouched: the same tiles, at the same resolution, as
//! Sleuth 1.0. It can be made cheaper (larger tiles scaled down, less
//! overlap; see [`OcrOptions`]), but every such setting was seen to lose some
//! faint or very small text on real drawings while gaining other, so none is
//! the default. `bench/results.md` has the measurements.

use crate::models::ModelSet;
use crate::pdf::Doc;
use crate::text::{Line, PageText};
use anyhow::{Result, anyhow, bail};
use image::{RgbImage, imageops};
use oar_ocr::core::config::{OrtGraphOptimizationLevel, OrtSessionConfig};
use oar_ocr::core::traits::OrtConfigurable;
use oar_ocr::core::traits::adapter::{AdapterBuilder, ModelAdapter};
use oar_ocr::core::traits::task::ImageTaskInput;
use oar_ocr::domain::adapters::{
    TextDetectionAdapter, TextDetectionAdapterBuilder, TextLineOrientationAdapter, TextLineOrientationAdapterBuilder, TextRecognitionAdapter,
    TextRecognitionAdapterBuilder,
};
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};
use oar_ocr::processors::{BoundingBox, LimitType, sort_quad_boxes};
use oar_ocr::utils::get_rotate_crop_image;
use sleuth_core::analyzer::{LOW_CONFIDENCE, sep_form};
use sleuth_core::protocol::{NormBox, TextSource};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Tile edge in pixels. Detection runs on whole tiles, so this sets peak memory.
pub const TILE: i32 = 1600;
/// Wider than any tag at 300 dpi, so a short line always fits wholly in one
/// tile. Every spot of a sheet is also seen by nearly two tiles on average,
/// and faint text that the detector misses in one it often finds in the
/// other: less overlap was measured to lose such text.
pub const OVERLAP: i32 = 512;
/// Text detection runs on the tile scaled to this side; recognition always
/// reads the full-resolution crop.
pub const DETECT_SIDE: u32 = 1600;
/// Megapixels of a page held in memory at once when it is rendered in bands.
pub const BAND_MPX: u32 = 24;
/// Pixels from a cut edge within which a region is suspected of being cut.
const EDGE: f32 = 6.0;
/// Pixels (of the detector) from a tile edge within which a region may be one
/// part of a line the edge cut. The detector's boxes stop a little short of
/// the edge, so this is wider than `EDGE`; joining also needs both parts to
/// span the strip the two tiles share, which chance does not produce.
const JOIN_EDGE: f32 = 12.0;
/// Longest line, as a multiple of its height, that parts are joined into.
/// The recogniser reads a line at 48 pixels high and squeezes whatever comes
/// out wider than 3200; longer lines stay in the overlapping parts the tiles
/// cut them into, as before.
const JOIN_MAX_RATIO: f32 = 60.0;
/// Below this, ask the Windows reader for a second opinion.
const SECOND_OPINION_BELOW: f32 = 0.85;
/// Below this, `Orient::Lazy` checks whether the line stands on its head.
const TURN_CHECK_BELOW: f32 = 0.9;
/// Below this, a line that two tiles found is also read from the other
/// tile's box.
const SECOND_LOOK_BELOW: f32 = 0.9;
/// Regions of a band cropped and recognised at a time.
const CROP_SLICE: usize = 512;
/// Crops per call of the orientation classifier when a band is read together.
const ORIENT_BATCH: usize = 32;
/// Largest image file read, in megapixels (A0 at 400 dpi is 248).
pub const MAX_IMAGE_MPX: u64 = 260;

/// Where the ONNX models run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    Cpu,
    /// DirectML on adapter 0.
    DirectMl,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Cpu => "CPU",
            Backend::DirectMl => "DirectML",
        }
    }
}

/// Which text regions go through the 0/180 degree orientation classifier
/// before recognition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orient {
    /// Every region (Sleuth 1.0).
    Always,
    /// Only regions taller than wide, where the direction of vertical text
    /// has to be decided.
    Vertical,
    /// Regions taller than wide before recognition, as `Vertical`; any other
    /// region only when it read with low confidence, and then the reading of
    /// the turned crop is taken if it is the more confident one. Text printed
    /// upright, which is nearly all of it, never meets the classifier.
    Lazy,
    Never,
}

/// How pages are read. The defaults are the pipeline in use;
/// [`OcrOptions::legacy`] is the one of Sleuth 1.0, kept for comparison runs
/// (`bench --legacy`, `bench --set key=value`).
///
/// Three groups of settings:
///
/// - what the detector is shown (`tile`, `overlap`, `detect_side`,
///   `even_tiles`, `scale_all`): changing these changes which text is found;
/// - how what it found is read (`pool`, `early_dedupe`, `stitch`,
///   `keep_parts`, `orient`, `rec_batch`, `rec_width`, `second_opinion`):
///   these change readings here and there;
/// - plumbing that changes nothing that is read (`threads`, `page_render`,
///   `band_mpx`, `det_batch`, `graph_opt`, `mem_pattern`, `ort_entries`).
///
/// The first two groups are part of [`config_signature`].
#[derive(Debug, Clone)]
pub struct OcrOptions {
    pub threads: usize,
    pub tile: i32,
    pub overlap: i32,
    pub detect_side: u32,
    pub backend: Backend,
    /// Render a page once (in bands of at most `band_mpx` megapixels) and cut
    /// the tiles from that bitmap. `false` asks PDFium for every tile
    /// separately, as Sleuth 1.0 did.
    pub page_render: bool,
    pub band_mpx: u32,
    /// Cut every axis into equal tiles (as few as cover it with at least
    /// `overlap` shared) instead of full tiles and a narrow last one. Off:
    /// the detector's result depends a little on everything in its tile, so
    /// another grid finds and misses other faint text than Sleuth 1.0 did.
    pub even_tiles: bool,
    /// When tiles are larger than `detect_side`, detect on every tile at the
    /// same scale (`detect_side / tile`), also on the smaller tiles at the
    /// right and bottom, which would otherwise be detected at a finer scale
    /// than the rest of the page. Without effect at the default tile size.
    pub scale_all: bool,
    /// Recognise the regions of a whole band together, from the band's own
    /// pixels. `false` recognises tile by tile (Sleuth 1.0).
    pub pool: bool,
    /// Before recognition, drop a region that another tile of the same band
    /// found on the same pixels. Needs `pool`.
    pub early_dedupe: bool,
    /// Join the two parts of a line that a tile edge cut, and read the line
    /// whole from the band as well. Needs `pool`.
    pub stitch: bool,
    /// When one line found lies inside a larger one, drop the smaller only
    /// if the larger holds its text; `false` keeps whichever of the two read
    /// with more confidence, as Sleuth 1.0 did, which can throw away a whole
    /// line for a fragment of it.
    pub keep_parts: bool,
    pub orient: Orient,
    /// Text regions per recognition call. A batch is padded to its widest
    /// region and the padding is computed like text: on one thread batches
    /// of eight were measured no faster than regions one by one, took half
    /// as much memory again, and make a region's reading depend on what it
    /// shared a batch with.
    pub rec_batch: usize,
    /// Tiles per detection call (same-sized tiles of one band).
    pub det_batch: usize,
    /// Narrowest picture, in pixels at 48 high, that the recogniser is given;
    /// shorter crops are padded up to it. 320 is what the model's authors use.
    pub rec_width: usize,
    /// Ask the Windows reader about lines read with low confidence.
    pub second_opinion: bool,
    /// Read every tile with `oar_ocr`'s all-in-one call, the very code path
    /// of Sleuth 1.0 (no step timings). Only for A/B runs: it proves that the
    /// steps driven here read the same as the call they replace.
    pub one_call: bool,
    /// ONNX Runtime graph optimisation level 0..=3; `None` leaves its default
    /// (all optimisations, measured to be what it applies).
    pub graph_opt: Option<u8>,
    /// ONNX Runtime memory-pattern planning; `None` leaves its default (on).
    /// Off by default here: with inputs of ever-changing size it plans, and
    /// keeps, a block per size, and switching it off took a third off the
    /// memory of a reader at no cost in time and with the same readings.
    pub mem_pattern: Option<bool>,
    /// Extra ONNX Runtime session entries (`session.intra_op.allow_spinning`, ...).
    pub ort_entries: Vec<(String, String)>,
}

impl Default for OcrOptions {
    fn default() -> Self {
        OcrOptions {
            threads: 2,
            tile: TILE,
            overlap: OVERLAP,
            detect_side: DETECT_SIDE,
            backend: Backend::Cpu,
            page_render: true,
            band_mpx: BAND_MPX,
            even_tiles: false,
            scale_all: false,
            pool: true,
            early_dedupe: true,
            stitch: true,
            keep_parts: true,
            orient: Orient::Lazy,
            rec_batch: 1,
            det_batch: 1,
            rec_width: 320,
            second_opinion: true,
            one_call: false,
            graph_opt: None,
            mem_pattern: Some(false),
            ort_entries: Vec::new(),
        }
    }
}

impl OcrOptions {
    /// The pipeline of Sleuth 1.0.x, for A/B runs against the present one.
    pub fn legacy(threads: usize) -> OcrOptions {
        OcrOptions {
            threads,
            tile: 1600,
            overlap: 512,
            detect_side: 1600,
            backend: Backend::Cpu,
            page_render: false,
            band_mpx: BAND_MPX,
            even_tiles: false,
            scale_all: false,
            pool: false,
            early_dedupe: false,
            stitch: false,
            keep_parts: false,
            orient: Orient::Always,
            rec_batch: 8,
            det_batch: 1,
            rec_width: 320,
            second_opinion: true,
            one_call: false,
            graph_opt: None,
            mem_pattern: None,
            ort_entries: Vec::new(),
        }
    }

    /// Set one option by name, for benchmark flags (`tile=2400`,
    /// `orient=vertical`, `ort:session.intra_op.allow_spinning=0`, ...).
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let flag = || match value {
            "1" | "true" | "on" | "yes" => Ok(true),
            "0" | "false" | "off" | "no" => Ok(false),
            _ => Err(anyhow!("{key}: expected 1 or 0, got {value}")),
        };
        let bad = |_| anyhow!("{key}: not a number: {value}");
        match key {
            "threads" => self.threads = value.parse().map_err(bad)?,
            "tile" => self.tile = value.parse().map_err(bad)?,
            "overlap" => self.overlap = value.parse().map_err(bad)?,
            "detect" | "detect_side" => self.detect_side = value.parse().map_err(bad)?,
            "page_render" => self.page_render = flag()?,
            "band_mpx" => self.band_mpx = value.parse().map_err(bad)?,
            "even_tiles" => self.even_tiles = flag()?,
            "scale_all" => self.scale_all = flag()?,
            "pool" => self.pool = flag()?,
            "early_dedupe" => self.early_dedupe = flag()?,
            "stitch" => self.stitch = flag()?,
            "keep_parts" => self.keep_parts = flag()?,
            "orient" => {
                self.orient = match value {
                    "always" => Orient::Always,
                    "vertical" => Orient::Vertical,
                    "lazy" => Orient::Lazy,
                    "never" => Orient::Never,
                    _ => bail!("orient: expected always, vertical, lazy or never"),
                }
            }
            "rec_batch" => self.rec_batch = value.parse().map_err(bad)?,
            "det_batch" => self.det_batch = value.parse().map_err(bad)?,
            "rec_width" => self.rec_width = value.parse().map_err(bad)?,
            "second_opinion" => self.second_opinion = flag()?,
            "one_call" => self.one_call = flag()?,
            "graph_opt" => self.graph_opt = if value == "default" { None } else { Some(value.parse().map_err(bad)?) },
            "mem_pattern" => self.mem_pattern = if value == "default" { None } else { Some(flag()?) },
            k if k.starts_with("ort:") => self.ort_entries.push((k[4..].to_string(), value.to_string())),
            _ => bail!("unknown OCR option {key}"),
        }
        Ok(())
    }
}

/// Settings that change what OCR produces; part of the pipeline fingerprint.
pub fn config_signature(o: &OcrOptions) -> String {
    let orient = match o.orient {
        Orient::Always => "all",
        Orient::Vertical => "vert",
        Orient::Lazy => "lazy<0.9",
        Orient::Never => "none",
    };
    format!(
        "tile={}/{}/{}{};det=0.2/0.45/1.4/3000;second<{SECOND_OPINION_BELOW}{};edge={EDGE};read=2:{}{}{}{}{}/{orient}/{}/{}",
        o.tile,
        o.overlap,
        o.detect_side,
        match (o.even_tiles, o.scale_all) {
            (true, true) => "/even/scaled",
            (true, false) => "/even",
            (false, true) => "/scaled",
            (false, false) => "",
        },
        if o.second_opinion { "" } else { "(off)" },
        if o.page_render { "page" } else { "tile" },
        if o.pool { "+pool" } else { "" },
        if o.pool && o.early_dedupe { "+twins<0.9" } else { "" },
        if o.pool && o.stitch { "+stitch" } else { "" },
        if o.keep_parts { "+parts" } else { "" },
        o.rec_batch.max(1),
        o.rec_width,
    )
}

/// Tiles looked at and actually read for one page or region set.
#[derive(Debug, Clone, Copy, Default)]
pub struct TileStats {
    pub tiles: u32,
    pub blank: u32,
}

/// Where the time of this reader went since it was built (or since
/// [`OcrReader::take_timings`]), in seconds, with the counts that explain it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    /// Rendering page pixels (PDFium) or decoding an image file.
    pub render: f64,
    /// Cutting tiles out of a band and checking them for ink.
    pub tiles: f64,
    pub detect: f64,
    pub crop: f64,
    pub orient: f64,
    pub recognise: f64,
    /// The Windows reader's second opinion.
    pub second: f64,
    /// Everything, including what the fields above do not cover.
    pub total: f64,
    pub pages: u32,
    pub tiles_read: u32,
    pub tiles_blank: u32,
    /// Text regions the detector found.
    pub regions: u32,
    /// Regions not recognised because another tile had the same pixels.
    pub twins: u32,
    /// Lines cut by a tile edge that were joined and read whole.
    pub stitched: u32,
    pub oriented: u32,
    /// Recognitions, counting the second looks.
    pub recognised: u32,
    /// Unsure lines read again from the box another tile drew.
    pub second_looks: u32,
    pub second_asked: u32,
}

impl Timings {
    pub fn other(&self) -> f64 {
        (self.total - self.render - self.tiles - self.detect - self.crop - self.orient - self.recognise - self.second).max(0.0)
    }
}

#[allow(clippy::large_enum_variant)] // one per reader, never moved around
enum Engine {
    /// Detection, orientation and recognition driven one by one.
    Steps { det: TextDetectionAdapter, ori: Option<TextLineOrientationAdapter>, rec: TextRecognitionAdapter },
    /// `oar_ocr`'s all-in-one call (see [`OcrOptions::one_call`]).
    OneCall(Box<OAROCR>),
}

pub struct OcrReader {
    engine: Engine,
    pub model: String,
    pub model_hash: String,
    pub opts: OcrOptions,
    timings: Mutex<Timings>,
}

/// A text region the detector found, in the coordinates of the bitmap it is
/// cropped from.
struct Region {
    quad: BoundingBox,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    /// Which interior edges of its tile it touches (`CUT_*` bits).
    cut: u8,
    /// Which interior edges of its tile it comes near enough to be joined
    /// with what the next tile saw.
    near: u8,
    /// Which tile of the band found it.
    tile: usize,
    /// Made by [`stitch`] from the two parts of a line.
    joined: bool,
    /// The boxes other tiles drew around the same text (see [`drop_twins`]).
    spares: Vec<BoundingBox>,
}

const CUT_L: u8 = 1;
const CUT_T: u8 = 2;
const CUT_R: u8 = 4;
const CUT_B: u8 = 8;

impl Region {
    fn area(&self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    /// Lies square on the page (not text drawn at an angle).
    fn upright(&self) -> bool {
        self.quad.points.len() == 4 && self.quad.area() >= 0.8 * self.area()
    }

    /// Intersection with `o` over the smaller of the two boxes.
    fn overlap(&self, o: &Region) -> f32 {
        let w = (self.x1.min(o.x1) - self.x0.max(o.x0)).max(0.0);
        let h = (self.y1.min(o.y1) - self.y0.max(o.y0)).max(0.0);
        w * h / self.area().min(o.area()).max(1.0)
    }
}

/// Something that renders rectangles of a page: `(x, y, w, h)` in pixels of
/// the whole page.
type Render<'a> = dyn FnMut(i32, i32, i32, i32) -> Result<RgbImage> + 'a;

impl OcrReader {
    pub fn new(models: &ModelSet, threads: usize) -> Result<OcrReader> {
        Self::build(models, &OcrOptions { threads, ..Default::default() })
    }

    pub fn with_tile(models: &ModelSet, threads: usize, tile: i32, overlap: i32, detect_side: u32) -> Result<OcrReader> {
        Self::build(models, &OcrOptions { threads, tile, overlap, detect_side, backend: Backend::Cpu, ..Default::default() })
    }

    pub fn build(models: &ModelSet, o: &OcrOptions) -> Result<OcrReader> {
        let det = TextDetectionConfig {
            score_threshold: 0.2,
            box_threshold: 0.45,
            unclip_ratio: 1.4,
            max_candidates: 3000,
            limit_side_len: Some(o.detect_side),
            limit_type: Some(LimitType::Max),
            max_side_len: Some(o.detect_side),
        };
        let mut ort = OrtSessionConfig {
            intra_threads: Some(o.threads.max(1)),
            inter_threads: Some(1),
            optimization_level: o.graph_opt.map(|level| match level {
                0 => OrtGraphOptimizationLevel::DisableAll,
                1 => OrtGraphOptimizationLevel::Level1,
                2 => OrtGraphOptimizationLevel::Level2,
                _ => OrtGraphOptimizationLevel::All,
            }),
            enable_mem_pattern: o.mem_pattern,
            session_config_entries: (!o.ort_entries.is_empty()).then(|| o.ort_entries.iter().cloned().collect()),
            ..Default::default()
        };
        if o.backend == Backend::DirectMl {
            use oar_ocr::core::config::OrtExecutionProvider as P;
            // DirectML needs sequential execution and no memory patterns. A
            // node the GPU cannot run falls back to the CPU provider.
            ort.execution_providers = Some(vec![P::DirectML { device_id: Some(0) }, P::CPU]);
            ort.parallel_execution = Some(false);
            ort.enable_mem_pattern = Some(false);
        }
        let failed = |e: oar_ocr::core::OCRError| anyhow!("could not load OCR models on {}: {e}", o.backend.label());
        let mut opts = o.clone();
        if o.one_call {
            // Exactly what Sleuth 1.0 built.
            let mut b = OAROCRBuilder::new(models.det.clone(), models.rec.clone(), models.dict.clone()).text_detection_config(det).ort_session(ort).region_batch_size(o.rec_batch.max(1));
            if let Some(ori) = &models.textline_ori {
                b = b.with_text_line_orientation_classification(ori.clone());
            }
            (opts.page_render, opts.pool, opts.orient) = (false, false, Orient::Always);
            let engine = Engine::OneCall(Box::new(b.build().map_err(failed)?));
            return Ok(OcrReader { engine, model: models.name.clone(), model_hash: models.hash.clone(), opts, timings: Mutex::new(Timings::default()) });
        }
        let det = TextDetectionAdapterBuilder::new().with_ort_config(ort.clone()).with_config(det).build(models.det.clone()).map_err(failed)?;
        let ori = match &models.textline_ori {
            Some(p) => Some(TextLineOrientationAdapterBuilder::new().with_ort_config(ort.clone()).build(p.clone()).map_err(failed)?),
            None => None,
        };
        let dict = std::fs::read_to_string(&models.dict).map_err(|e| anyhow!("could not read the OCR dictionary {}: {e}", models.dict.display()))?;
        let rec = TextRecognitionAdapterBuilder::new()
            .character_dict(dict.lines().map(str::to_string).collect())
            .return_word_box(false)
            .model_input_shape([3, 48, o.rec_width.clamp(48, 3200)])
            .with_ort_config(ort)
            .build(models.rec.clone())
            .map_err(failed)?;
        Ok(OcrReader { engine: Engine::Steps { det, ori, rec }, model: models.name.clone(), model_hash: models.hash.clone(), opts, timings: Mutex::new(Timings::default()) })
    }

    /// The scale detection runs at on a full tile: 1 when the tile is no
    /// larger than `detect_side`.
    fn detect_scale(&self) -> f32 {
        (self.opts.detect_side as f32 / self.opts.tile.max(1) as f32).min(1.0)
    }

    /// Time spent per step since the last call; resets the counters.
    pub fn take_timings(&self) -> Timings {
        std::mem::take(&mut *self.timings.lock().unwrap())
    }

    fn clock(&self, f: impl FnOnce(&mut Timings)) {
        f(&mut self.timings.lock().unwrap());
    }

    /// Detect and recognise one image as it is (no tiling, no second
    /// opinion): `(text, confidence, [x0, y0, x1, y1])`. Used by the backend
    /// check, which compares two backends on the same picture.
    pub fn recognize(&self, img: RgbImage) -> Result<Vec<(String, f32, [f32; 4])>> {
        if let Engine::OneCall(ocr) = &self.engine {
            let mut out = Vec::new();
            for res in ocr.predict(vec![img]).map_err(|e| anyhow!("OCR failed: {e}"))? {
                for r in res.text_regions {
                    let (Some(text), Some(conf)) = (r.text.as_deref(), r.confidence) else { continue };
                    if let Some(b) = region(r.bounding_box, 0, 0.0, 0.0).filter(|_| !text.trim().is_empty()) {
                        out.push((text.trim().to_string(), conf, [b.x0, b.y0, b.x1, b.y1]));
                    }
                }
            }
            return Ok(out);
        }
        let img = Arc::new(img);
        let regions: Vec<Region> = self
            .detect(std::slice::from_ref(&img))?
            .pop()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|quad| region(quad, 0, 0.0, 0.0))
            .collect();
        let read = self.recognise(&img, &regions, self.opts.orient, &|| false)?;
        Ok(read.into_iter().filter(|(_, text, _)| !text.trim().is_empty()).map(|(i, text, conf)| (text.trim().to_string(), conf, [regions[i].x0, regions[i].y0, regions[i].x1, regions[i].y1])).collect())
    }

    /// OCR one whole page. `cancel` is checked between tiles.
    pub fn read_page(&self, doc: &Doc, index: u32, dpi: u32, cancel: &dyn Fn() -> bool) -> Result<PageText> {
        self.read_regions(doc, index, dpi, &[], cancel).map(|(p, _)| p)
    }

    /// OCR parts of a page. `regions` (normalised) limit the tiles read; an
    /// empty list means the whole page.
    pub fn read_regions(&self, doc: &Doc, index: u32, dpi: u32, regions: &[NormBox], cancel: &dyn Fn() -> bool) -> Result<(PageText, TileStats)> {
        let started = Instant::now();
        let (pw, ph) = doc.page_size(index)?;
        let mut scale = dpi as f32 / 72.0;
        // Keep pathological page sizes (banners, A0+ at 400 dpi) bounded.
        let max_side = 20_000.0;
        if pw.max(ph) * scale > max_side {
            scale = max_side / pw.max(ph);
        }
        let (full_w, full_h) = ((pw * scale).ceil() as i32, (ph * scale).ceil() as i32);
        // Areas in pixels; a region is widened by half a tile overlap so text
        // straddling its border is read whole.
        let pad = self.opts.overlap / 2;
        let areas: Vec<(i32, i32, i32, i32)> = if regions.is_empty() {
            vec![(0, 0, full_w, full_h)]
        } else {
            merge_rects(
                regions
                    .iter()
                    .map(|r| {
                        let x0 = ((r.x * full_w as f32) as i32 - pad).max(0);
                        let y0 = ((r.y * full_h as f32) as i32 - pad).max(0);
                        let x1 = (((r.x + r.w) * full_w as f32).ceil() as i32 + pad).min(full_w);
                        let y1 = (((r.y + r.h) * full_h as f32).ceil() as i32 + pad).min(full_h);
                        (x0, y0, x1, y1)
                    })
                    .filter(|(x0, y0, x1, y1)| x1 > x0 && y1 > y0)
                    .collect(),
            )
        };
        let (found, stats) = if self.opts.page_render {
            // The page stays loaded, so PDFium decodes a scan once for all bands.
            let page = doc.raster(index, scale)?;
            self.read_areas(&mut |x, y, w, h| page.render(x, y, w, h), full_w, full_h, &areas, cancel)?
        } else {
            self.read_areas(&mut |x, y, w, h| doc.render_tile(index, scale, x, y, w, h), full_w, full_h, &areas, cancel)?
        };
        self.clock(|t| {
            t.total += started.elapsed().as_secs_f64();
            t.pages += 1;
        });
        Ok((page_text(index + 1, found, full_w, full_h), stats))
    }

    /// Tile the areas, read every tile, and remove what the overlaps read twice.
    fn read_areas(&self, render: &mut Render<'_>, full_w: i32, full_h: i32, areas: &[(i32, i32, i32, i32)], cancel: &dyn Fn() -> bool) -> Result<(Vec<Found>, TileStats)> {
        let tile_px = self.opts.tile.max(64);
        let step = (tile_px - self.opts.overlap).max(32);
        let band_px = self.opts.band_mpx.max(1) as i64 * 1_000_000;
        let mut found: Vec<Found> = Vec::new();
        let mut stats = TileStats::default();
        let mut timed_render = |x: i32, y: i32, w: i32, h: i32| -> Result<Arc<RgbImage>> {
            let t = Instant::now();
            let img = render(x, y, w, h)?;
            self.clock(|c| c.render += t.elapsed().as_secs_f64());
            Ok(Arc::new(img))
        };
        for &(ax0, ay0, ax1, ay1) in areas {
            let (xs, ys) = if self.opts.even_tiles {
                // Tile sizes that are a multiple of the detector's stride once scaled.
                let unit = (32.0 / self.detect_scale()).round() as i32;
                (even_cuts(ax0, ax1, tile_px, tile_px - step, unit), even_cuts(ay0, ay1, tile_px, tile_px - step, unit))
            } else {
                (cuts(ax0, ax1, tile_px, step), cuts(ay0, ay1, tile_px, step))
            };
            if !self.opts.page_render {
                for &(y, th) in &ys {
                    for &(x, tw) in &xs {
                        if cancel() {
                            bail!("cancelled");
                        }
                        let tile = timed_render(x, y, tw, th)?;
                        self.read_canvas(&tile, x, y, &[(x, y, tw, th)], full_w, full_h, &mut found, &mut stats, cancel)?;
                    }
                }
                continue;
            }
            // Bands of whole tile rows, as many rows as the memory budget holds.
            let mut r = 0;
            while r < ys.len() {
                let mut e = r + 1;
                while e < ys.len() && (ys[e].0 + ys[e].1 - ys[r].0) as i64 * (ax1 - ax0) as i64 <= band_px {
                    e += 1;
                }
                if cancel() {
                    bail!("cancelled");
                }
                let (by0, by1) = (ys[r].0, ys[e - 1].0 + ys[e - 1].1);
                let band = timed_render(ax0, by0, ax1 - ax0, by1 - by0)?;
                let tiles: Vec<(i32, i32, i32, i32)> = ys[r..e].iter().flat_map(|&(y, th)| xs.iter().map(move |&(x, tw)| (x, y, tw, th))).collect();
                self.read_canvas(&band, ax0, by0, &tiles, full_w, full_h, &mut found, &mut stats, cancel)?;
                r = e;
            }
        }
        Ok((dedupe(found, self.opts.keep_parts), stats))
    }

    /// Read the tiles of one bitmap. `(cx, cy)` is where the bitmap sits on
    /// the page; `tiles` are in page pixels and lie inside it.
    #[allow(clippy::too_many_arguments)]
    fn read_canvas(
        &self,
        canvas: &Arc<RgbImage>,
        cx: i32,
        cy: i32,
        tiles: &[(i32, i32, i32, i32)],
        full_w: i32,
        full_h: i32,
        out: &mut Vec<Found>,
        stats: &mut TileStats,
        cancel: &dyn Fn() -> bool,
    ) -> Result<()> {
        if let Engine::OneCall(ocr) = &self.engine {
            return self.read_tile_one_call(ocr, canvas, cx, cy, full_w, full_h, out, stats);
        }
        let mut regions: Vec<Region> = Vec::new();
        // Tiles waiting for detection: `(index, x, y, w, h, pixels)`.
        let mut waiting: Vec<(usize, i32, i32, i32, i32, Arc<RgbImage>)> = Vec::new();
        let det_batch = if self.opts.pool { self.opts.det_batch.max(1) } else { 1 };
        for (n, &(x, y, tw, th)) in tiles.iter().enumerate() {
            if cancel() {
                bail!("cancelled");
            }
            let t = Instant::now();
            let whole = x == cx && y == cy && tw as u32 == canvas.width() && th as u32 == canvas.height();
            let sub = if whole { Arc::clone(canvas) } else { Arc::new(cut_rgb(canvas, (x - cx) as u32, (y - cy) as u32, tw as u32, th as u32)) };
            let blank = is_blank(&sub);
            stats.tiles += 1;
            stats.blank += blank as u32;
            self.clock(|c| {
                c.tiles += t.elapsed().as_secs_f64();
                c.tiles_read += 1;
                c.tiles_blank += blank as u32;
            });
            if !blank {
                let scale = self.detect_scale();
                let sub = if self.opts.scale_all && scale < 1.0 {
                    let t = Instant::now();
                    let small = imageops::resize(sub.as_ref(), ((tw as f32 * scale).round() as u32).max(1), ((th as f32 * scale).round() as u32).max(1), imageops::FilterType::Triangle);
                    self.clock(|c| c.tiles += t.elapsed().as_secs_f64());
                    Arc::new(small)
                } else {
                    sub
                };
                waiting.push((n, x, y, tw, th, sub));
            }
            let last = n + 1 == tiles.len();
            // Detect when the batch is full, when the next tile has another
            // size (one call takes one size), or at the end.
            let full = waiting.len() >= det_batch || last || tiles.get(n + 1).is_some_and(|next| waiting.first().is_some_and(|w| (w.3, w.4) != (next.2, next.3)));
            if full && !waiting.is_empty() {
                let imgs: Vec<Arc<RgbImage>> = waiting.iter().map(|w| Arc::clone(&w.5)).collect();
                let boxes = self.detect(&imgs)?;
                for ((n, x, y, tw, th, seen), quads) in waiting.drain(..).zip(boxes) {
                    let (w, h) = (tw as f32, th as f32);
                    // Back from the detector's pixels to the tile's.
                    let (kx, ky) = (w / seen.width().max(1) as f32, h / seen.height().max(1) as f32);
                    for mut quad in quads {
                        if kx != 1.0 || ky != 1.0 {
                            for p in &mut quad.points {
                                (p.x, p.y) = (p.x * kx, p.y * ky);
                            }
                        }
                        let Some(mut r) = region(quad, n, (x - cx) as f32, (y - cy) as f32) else { continue };
                        // Interior edges are cuts; the page border is not.
                        let (lx0, ly0, lx1, ly1) = (r.x0 - (x - cx) as f32, r.y0 - (y - cy) as f32, r.x1 - (x - cx) as f32, r.y1 - (y - cy) as f32);
                        let edges = |m: f32| {
                            (if lx0 < m && x > 0 { CUT_L } else { 0 })
                                | (if ly0 < m && y > 0 { CUT_T } else { 0 })
                                | (if lx1 > w - m && x + tw < full_w { CUT_R } else { 0 })
                                | (if ly1 > h - m && y + th < full_h { CUT_B } else { 0 })
                        };
                        (r.cut, r.near) = (edges(EDGE), edges(JOIN_EDGE * kx.max(ky)));
                        regions.push(r);
                    }
                }
            }
            if !self.opts.pool || last {
                if self.opts.pool && self.opts.early_dedupe {
                    let twins = drop_twins(&mut regions);
                    self.clock(|c| c.twins += twins);
                }
                if self.opts.pool && self.opts.stitch {
                    let joined = stitch(&mut regions);
                    self.clock(|c| c.stitched += joined);
                }
                self.finish(canvas, cx, cy, &regions, out, cancel)?;
                regions.clear();
            }
        }
        Ok(())
    }

    /// Sleuth 1.0's reading of one tile, unchanged (see [`OcrOptions::one_call`]).
    #[allow(clippy::too_many_arguments)]
    fn read_tile_one_call(&self, ocr: &OAROCR, tile: &Arc<RgbImage>, ox: i32, oy: i32, full_w: i32, full_h: i32, out: &mut Vec<Found>, stats: &mut TileStats) -> Result<()> {
        stats.tiles += 1;
        self.clock(|c| c.tiles_read += 1);
        if is_blank(tile) {
            stats.blank += 1;
            self.clock(|c| c.tiles_blank += 1);
            return Ok(());
        }
        let (tw, th) = (tile.width() as f32, tile.height() as f32);
        let results = ocr.predict(vec![tile.as_ref().clone()]).map_err(|e| anyhow!("OCR failed: {e}"))?;
        let Some(res) = results.into_iter().next() else { return Ok(()) };
        for region in res.text_regions {
            let (Some(text), Some(conf)) = (region.text.as_deref(), region.confidence) else { continue };
            let text = text.trim();
            if !text.chars().any(char::is_alphanumeric) {
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
            let cut = (x0 < EDGE && ox > 0) || (y0 < EDGE && oy > 0) || (x1 > tw - EDGE && ox + (tw as i32) < full_w) || (y1 > th - EDGE && oy + (th as i32) < full_h);
            let mut f = Found { text: text.to_string(), conf, x0: x0 + ox as f32, y0: y0 + oy as f32, x1: x1 + ox as f32, y1: y1 + oy as f32, cut, joined: false, alt: None };
            if self.opts.second_opinion
                && conf < SECOND_OPINION_BELOW
                && let Some(other) = crate::winocr::read_crop(tile, x0, y0, x1, y1)
            {
                if sep_form(&other) == sep_form(&f.text) {
                    f.conf = f.conf.max(0.9);
                } else if !sep_form(&other).is_empty() {
                    f.alt = Some(other);
                }
            }
            out.push(f);
        }
        Ok(())
    }

    /// Recognise the regions of a bitmap, ask for second opinions, and add
    /// the lines found (in page coordinates) to `out`.
    fn finish(&self, canvas: &Arc<RgbImage>, cx: i32, cy: i32, regions: &[Region], out: &mut Vec<Found>, cancel: &dyn Fn() -> bool) -> Result<()> {
        for (i, text, conf) in self.recognise(canvas, regions, self.opts.orient, cancel)? {
            let r = &regions[i];
            let text = text.trim();
            // A reading with no letter or digit (specks, hatching, symbols)
            // can never be searched for.
            if !text.chars().any(char::is_alphanumeric) {
                continue;
            }
            let mut f = Found { text: text.to_string(), conf, x0: r.x0 + cx as f32, y0: r.y0 + cy as f32, x1: r.x1 + cx as f32, y1: r.y1 + cy as f32, cut: r.cut != 0, joined: r.joined, alt: None };
            if self.opts.second_opinion && conf < SECOND_OPINION_BELOW {
                let t = Instant::now();
                let other = crate::winocr::read_crop(canvas, r.x0, r.y0, r.x1, r.y1);
                self.clock(|c| {
                    c.second += t.elapsed().as_secs_f64();
                    c.second_asked += 1;
                });
                if let Some(other) = other {
                    if sep_form(&other) == sep_form(&f.text) {
                        // Two independent readers agree.
                        f.conf = f.conf.max(0.9);
                    } else if !sep_form(&other).is_empty() {
                        f.alt = Some(other);
                    }
                }
            }
            out.push(f);
        }
        Ok(())
    }

    /// Text boxes of each image, in reading order, in the image's own pixels.
    fn detect(&self, imgs: &[Arc<RgbImage>]) -> Result<Vec<Vec<BoundingBox>>> {
        let Engine::Steps { det, .. } = &self.engine else { bail!("the one-call reader has no separate steps") };
        let t = Instant::now();
        let det = det.execute(ImageTaskInput::from_arc_images(imgs.to_vec()), None).map_err(|e| anyhow!("OCR failed: {e}"))?;
        let mut out: Vec<Vec<BoundingBox>> = det.detections.into_iter().map(|d| sort_quad_boxes(&d.into_iter().map(|d| d.bbox).collect::<Vec<_>>())).collect();
        out.resize_with(imgs.len(), Vec::new);
        self.clock(|c| {
            c.detect += t.elapsed().as_secs_f64();
            c.regions += out.iter().map(|b| b.len() as u32).sum::<u32>();
        });
        Ok(out)
    }

    /// Crop, straighten and read regions of `canvas`: `(index into regions,
    /// text, confidence)` in the order of `regions`. A region whose crop
    /// fails is left out.
    fn recognise(&self, canvas: &Arc<RgbImage>, regions: &[Region], orient: Orient, cancel: &dyn Fn() -> bool) -> Result<Vec<(usize, String, f32)>> {
        if regions.is_empty() {
            return Ok(Vec::new());
        }
        let Engine::Steps { ori, rec, .. } = &self.engine else { bail!("the one-call reader has no separate steps") };
        // A whole band can hold thousands of regions; their crops are made,
        // read and dropped a slice at a time. Tile by tile there is one slice.
        let slice = if self.opts.pool { CROP_SLICE } else { usize::MAX };
        let mut out = Vec::with_capacity(regions.len());
        let mut first = 0;
        while first < regions.len() {
            let last = regions.len().min(first.saturating_add(slice));
            let t = Instant::now();
            // `(region, crop, width / height)`; a crop of vertical text comes back turned.
            let mut crops: Vec<(usize, Arc<RgbImage>, f32)> = Vec::with_capacity(last - first);
            for (i, r) in regions.iter().enumerate().take(last).skip(first) {
                if let Ok(img) = get_rotate_crop_image(canvas, &r.quad.points) {
                    let ratio = img.width() as f32 / img.height().max(1) as f32;
                    crops.push((i, Arc::new(img), ratio));
                }
            }
            first = last;
            self.clock(|c| c.crop += t.elapsed().as_secs_f64());
            let tall = |i: usize| regions[i].y1 - regions[i].y0 > regions[i].x1 - regions[i].x0;
            if let Some(ori) = ori {
                let ask: Vec<usize> = match orient {
                    Orient::Always => (0..crops.len()).collect(),
                    Orient::Vertical | Orient::Lazy => (0..crops.len()).filter(|&k| tall(crops[k].0)).collect(),
                    Orient::Never => Vec::new(),
                };
                self.turn_upright(ori, &mut crops, &ask)?;
            }
            let all: Vec<usize> = (0..crops.len()).collect();
            let mut read: Vec<Option<(String, f32)>> = vec![None; crops.len()];
            for (k, text, conf) in self.read_crops(rec, &crops, &all, cancel)? {
                read[k] = Some((text, conf));
            }
            if let (Some(ori), Orient::Lazy) = (ori, orient) {
                // Lines that read badly may stand on their heads: ask the
                // classifier about those, and read again the ones it turns.
                let doubt: Vec<usize> = (0..crops.len()).filter(|&k| !tall(crops[k].0) && read[k].as_ref().is_some_and(|r| r.1 < TURN_CHECK_BELOW)).collect();
                let turned = self.turn_upright(ori, &mut crops, &doubt)?;
                for (k, text, conf) in self.read_crops(rec, &crops, &turned, cancel)? {
                    if read[k].as_ref().is_none_or(|r| conf > r.1) {
                        read[k] = Some((text, conf));
                    }
                }
            }
            // A line that read badly and that another tile boxed a little
            // differently is read from that box too; the more confident
            // reading stays.
            let t = Instant::now();
            let mut spare: Vec<(usize, Arc<RgbImage>, f32)> = Vec::new();
            for k in 0..crops.len() {
                if read[k].as_ref().is_some_and(|r| r.1 < SECOND_LOOK_BELOW) {
                    for quad in &regions[crops[k].0].spares {
                        if let Ok(img) = get_rotate_crop_image(canvas, &quad.points) {
                            let ratio = img.width() as f32 / img.height().max(1) as f32;
                            spare.push((k, Arc::new(img), ratio));
                        }
                    }
                }
            }
            self.clock(|c| c.crop += t.elapsed().as_secs_f64());
            if !spare.is_empty() {
                let all: Vec<usize> = (0..spare.len()).collect();
                if let (Some(ori), false) = (ori, orient == Orient::Never) {
                    self.turn_upright(ori, &mut spare, &all)?;
                }
                for (e, text, conf) in self.read_crops(rec, &spare, &all, cancel)? {
                    let k = spare[e].0;
                    if read[k].as_ref().is_none_or(|r| conf > r.1) {
                        read[k] = Some((text, conf));
                    }
                }
                self.clock(|c| c.second_looks += all.len() as u32);
            }
            out.extend(crops.iter().zip(read).filter_map(|((i, _, _), r)| r.map(|(text, conf)| (*i, text, conf))));
        }
        Ok(out)
    }
}

impl OcrReader {
    /// Ask the orientation classifier about the crops `ask` and turn the ones
    /// that stand on their heads. Returns which were turned.
    fn turn_upright(&self, ori: &TextLineOrientationAdapter, crops: &mut [(usize, Arc<RgbImage>, f32)], ask: &[usize]) -> Result<Vec<usize>> {
        if ask.is_empty() {
            return Ok(Vec::new());
        }
        let t = Instant::now();
        let mut turned = Vec::new();
        // The classifier judges every crop by itself, so the batch size only
        // sets how much memory one call takes.
        let batch = if self.opts.pool { ORIENT_BATCH } else { usize::MAX };
        for chunk in ask.chunks(batch) {
            let input = ImageTaskInput::from_arc_images(chunk.iter().map(|&k| Arc::clone(&crops[k].1)).collect());
            let res = ori.execute(input, None).map_err(|e| anyhow!("OCR failed: {e}"))?;
            for (&k, classes) in chunk.iter().zip(res.classifications.iter()) {
                // Class 1 is text standing on its head.
                if classes.first().is_some_and(|c| c.class_id == 1) {
                    crops[k].1 = Arc::new(imageops::rotate180(crops[k].1.as_ref()));
                    turned.push(k);
                }
            }
        }
        self.clock(|c| {
            c.orient += t.elapsed().as_secs_f64();
            c.oriented += ask.len() as u32;
        });
        Ok(turned)
    }

    /// Recognise the crops `which`: `(crop, text, confidence)`. Crops of
    /// similar shape go into one call, so little of a batch is padding.
    fn read_crops(&self, rec: &TextRecognitionAdapter, crops: &[(usize, Arc<RgbImage>, f32)], which: &[usize], cancel: &dyn Fn() -> bool) -> Result<Vec<(usize, String, f32)>> {
        let t = Instant::now();
        let mut order = which.to_vec();
        order.sort_by(|&a, &b| crops[a].2.partial_cmp(&crops[b].2).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::with_capacity(order.len());
        for chunk in order.chunks(self.opts.rec_batch.max(1)) {
            if cancel() {
                bail!("cancelled");
            }
            let input = ImageTaskInput::from_arc_images(chunk.iter().map(|&k| Arc::clone(&crops[k].1)).collect());
            let res = rec.execute(input, None).map_err(|e| anyhow!("OCR failed: {e}"))?;
            out.extend(chunk.iter().zip(res.texts.into_iter().zip(res.scores)).map(|(&k, (text, score))| (k, text, score)));
        }
        self.clock(|c| {
            c.recognise += t.elapsed().as_secs_f64();
            c.recognised += order.len() as u32;
        });
        Ok(out)
    }

    /// Read a scanned image file (TIFF, JPEG, PNG or BMP) with the same
    /// tiling and OCR as a rendered PDF page, at the image's own resolution.
    /// A multi-page TIFF gives one page per frame, the other formats one
    /// page. Boxes are normalised to the image. An image over
    /// [`MAX_IMAGE_MPX`] megapixels is refused.
    pub fn read_image_file(&self, path: &Path, cancel: &dyn Fn() -> bool) -> Result<(Vec<PageText>, Vec<TileStats>)> {
        let frames = images::tiff_frames(path)?;
        let count = frames.as_ref().map_or(1, Vec::len);
        let (mut pages, mut stats) = (Vec::with_capacity(count), Vec::with_capacity(count));
        for n in 0..count {
            if cancel() {
                bail!("cancelled");
            }
            let started = Instant::now();
            let img = images::load(path, frames.as_ref().map(|f| f[n]))?;
            self.clock(|c| c.render += started.elapsed().as_secs_f64());
            let (w, h) = (img.width() as i32, img.height() as i32);
            let (found, st) = self.read_areas(&mut |x, y, tw, th| Ok(img.cut(x, y, tw, th)), w, h, &[(0, 0, w, h)], cancel)?;
            self.clock(|c| {
                c.total += started.elapsed().as_secs_f64();
                c.pages += 1;
            });
            pages.push(page_text(n as u32 + 1, found, w, h));
            stats.push(st);
        }
        Ok((pages, stats))
    }
}

/// Decoding image files into pixels to cut tiles from.
mod images {
    use super::MAX_IMAGE_MPX;
    use anyhow::{Context, Result, anyhow, bail};
    use image::{DynamicImage, GrayImage, ImageDecoder, ImageReader, RgbImage};
    use std::fs::File;
    use std::io::{BufReader, Read, Seek, SeekFrom};
    use std::path::Path;

    /// A decoded page. Grey scans (most drawing scans are) stay at one byte
    /// a pixel; only the tiles being read are widened to RGB.
    pub enum Pixels {
        Gray(GrayImage),
        Rgb(RgbImage),
    }

    impl Pixels {
        pub fn width(&self) -> u32 {
            match self {
                Pixels::Gray(i) => i.width(),
                Pixels::Rgb(i) => i.width(),
            }
        }

        pub fn height(&self) -> u32 {
            match self {
                Pixels::Gray(i) => i.height(),
                Pixels::Rgb(i) => i.height(),
            }
        }

        /// A rectangle of the page as RGB.
        pub fn cut(&self, x: i32, y: i32, w: i32, h: i32) -> RgbImage {
            let (x, y, w, h) = (x.max(0) as u32, y.max(0) as u32, w.max(0) as u32, h.max(0) as u32);
            match self {
                Pixels::Rgb(i) => super::cut_rgb(i, x, y, w, h),
                Pixels::Gray(i) => {
                    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
                    let (iw, raw) = (i.width() as usize, i.as_raw());
                    for row in y..(y + h).min(i.height()) {
                        let start = row as usize * iw + x as usize;
                        let end = (start + w as usize).min((row as usize + 1) * iw);
                        for &v in &raw[start..end] {
                            out.extend_from_slice(&[v, v, v]);
                        }
                        // A request past the right edge is filled with white.
                        out.resize(out.len() + (w as usize - (end - start)) * 3, 255);
                    }
                    out.resize(w as usize * h as usize * 3, 255);
                    RgbImage::from_raw(w, h, out).expect("buffer sized for the tile")
                }
            }
        }
    }

    /// One image of a TIFF file: where its directory starts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TiffFrame {
        offset: u64,
        big: bool,
        little_endian: bool,
    }

    fn within_budget(w: u32, h: u32) -> Result<()> {
        let px = w as u64 * h as u64;
        if px > MAX_IMAGE_MPX * 1_000_000 {
            bail!("image is {w} x {h} pixels ({} megapixels); the largest image read is {MAX_IMAGE_MPX} megapixels", px / 1_000_000);
        }
        if w == 0 || h == 0 {
            bail!("image has no pixels");
        }
        Ok(())
    }

    fn limits() -> image::Limits {
        let mut l = image::Limits::default();
        // Room for the largest allowed image at four bytes a pixel.
        l.max_alloc = Some(MAX_IMAGE_MPX * 1_000_000 * 4 + (64 << 20));
        l
    }

    /// Decode one page: frame `frame` of a TIFF, or the only image of another format.
    pub fn load(path: &Path, frame: Option<TiffFrame>) -> Result<Pixels> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let img = match frame {
            Some(f) => {
                let mut dec = image::codecs::tiff::TiffDecoder::new(BufReader::with_capacity(1 << 16, Patched::new(file, f))).map_err(|e| anyhow!("cannot read TIFF image: {e}"))?;
                let (w, h) = dec.dimensions();
                within_budget(w, h)?;
                dec.set_limits(limits()).map_err(|e| anyhow!("cannot read TIFF image: {e}"))?;
                DynamicImage::from_decoder(dec).map_err(|e| anyhow!("cannot read TIFF image: {e}"))?
            }
            None => {
                let mut reader = ImageReader::new(BufReader::with_capacity(1 << 16, file)).with_guessed_format()?;
                if reader.format().is_none() {
                    bail!("not an image Sleuth can read (TIFF, JPEG, PNG or BMP)");
                }
                reader.limits(limits());
                let mut dec = reader.into_decoder().map_err(|e| anyhow!("cannot read image: {e}"))?;
                let (w, h) = dec.dimensions();
                within_budget(w, h)?;
                // A photo taken with the camera turned says so in its EXIF data.
                let turn = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
                let mut img = DynamicImage::from_decoder(dec).map_err(|e| anyhow!("cannot read image: {e}"))?;
                img.apply_orientation(turn);
                img
            }
        };
        Ok(to_pixels(img))
    }

    /// Grey stays grey, everything else becomes RGB; transparency is laid on
    /// white paper.
    fn to_pixels(img: DynamicImage) -> Pixels {
        match img {
            DynamicImage::ImageLuma8(g) => Pixels::Gray(g),
            DynamicImage::ImageRgb8(c) => Pixels::Rgb(c),
            DynamicImage::ImageLuma16(_) => Pixels::Gray(img.to_luma8()),
            other if other.color().has_alpha() => {
                let rgba = other.into_rgba8();
                let (w, h) = rgba.dimensions();
                let mut out = Vec::with_capacity(w as usize * h as usize * 3);
                for p in rgba.pixels() {
                    let a = p[3] as u32;
                    for c in 0..3 {
                        out.push(((p[c] as u32 * a + 255 * (255 - a) + 127) / 255) as u8);
                    }
                }
                Pixels::Rgb(RgbImage::from_raw(w, h, out).expect("buffer sized for the image"))
            }
            other => Pixels::Rgb(other.into_rgb8()),
        }
    }

    /// The full-size images of a TIFF file in order, or `None` when the file
    /// is not a TIFF. Thumbnails and masks stored as extra directories are
    /// left out.
    pub fn tiff_frames(path: &Path) -> Result<Option<Vec<TiffFrame>>> {
        let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut head = [0u8; 16];
        let mut got = 0;
        while got < head.len() {
            match file.read(&mut head[got..])? {
                0 => break,
                n => got += n,
            }
        }
        let little_endian = match &head[..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Ok(None),
        };
        let u16_at = |b: &[u8]| if little_endian { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
        let u32_at = |b: &[u8]| if little_endian { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) };
        let u64_at = |b: &[u8]| {
            let a: [u8; 8] = b[..8].try_into().expect("eight bytes");
            if little_endian { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
        };
        let big = match u16_at(&head[2..4]) {
            42 if got >= 8 => false,
            43 if got >= 16 => true,
            _ => return Ok(None),
        };
        let mut next = if big { u64_at(&head[8..16]) } else { u32_at(&head[4..8]) as u64 };
        let (count_len, entry_len, next_len) = if big { (8usize, 20usize, 8usize) } else { (2, 12, 4) };
        let mut frames: Vec<TiffFrame> = Vec::new();
        let mut first = None;
        let mut seen = std::collections::HashSet::new();
        // A damaged chain ends the list; what was found before it is still read.
        while next != 0 && next < len && seen.insert(next) && seen.len() <= 100_000 {
            let frame = TiffFrame { offset: next, big, little_endian };
            first.get_or_insert(frame);
            file.seek(SeekFrom::Start(next))?;
            let mut count = [0u8; 8];
            if file.read_exact(&mut count[..count_len]).is_err() {
                break;
            }
            let entries = if big { u64_at(&count) } else { u16_at(&count) as u64 };
            if entries == 0 || entries > 65_535 {
                break;
            }
            let mut dir = vec![0u8; entries as usize * entry_len + next_len];
            if file.read_exact(&mut dir).is_err() {
                break;
            }
            let mut extra = false;
            for e in dir[..entries as usize * entry_len].chunks_exact(entry_len) {
                let value = &e[if big { 12 } else { 8 }..];
                let v = if u16_at(&e[2..4]) == 3 { u16_at(value) as u32 } else { u32_at(value) };
                match u16_at(&e[..2]) {
                    // NewSubfileType: bit 0 reduced-resolution copy, bit 2 transparency mask.
                    254 => extra |= v & 0b101 != 0,
                    // SubfileType (old): 2 is a reduced-resolution copy.
                    255 => extra |= v == 2,
                    _ => {}
                }
            }
            if !extra {
                frames.push(frame);
            }
            let tail = &dir[entries as usize * entry_len..];
            next = if big { u64_at(tail) } else { u32_at(tail) as u64 };
        }
        if frames.is_empty() {
            frames.extend(first);
        }
        if frames.is_empty() {
            bail!("TIFF file holds no image");
        }
        Ok(Some(frames))
    }

    /// A TIFF file seen with its header pointing at another directory, so a
    /// decoder that reads "the first image" reads the frame wanted.
    struct Patched {
        file: File,
        at: u64,
        bytes: [u8; 8],
        len: usize,
    }

    impl Patched {
        fn new(file: File, f: TiffFrame) -> Patched {
            let mut bytes = [0u8; 8];
            let (at, len) = if f.big {
                bytes = if f.little_endian { f.offset.to_le_bytes() } else { f.offset.to_be_bytes() };
                (8, 8)
            } else {
                let o = f.offset as u32;
                bytes[..4].copy_from_slice(&if f.little_endian { o.to_le_bytes() } else { o.to_be_bytes() });
                (4, 4)
            };
            Patched { file, at, bytes, len }
        }
    }

    impl Read for Patched {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.file.stream_position()?;
            let n = self.file.read(buf)?;
            let (start, end) = (pos.max(self.at), (pos + n as u64).min(self.at + self.len as u64));
            if start < end {
                buf[(start - pos) as usize..(end - pos) as usize].copy_from_slice(&self.bytes[(start - self.at) as usize..(end - self.at) as usize]);
            }
            Ok(n)
        }
    }

    impl Seek for Patched {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(to)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A little-endian TIFF with one uncompressed grey strip per frame.
        fn tiff(frames: &[(u32, u32, u8, Option<u32>)]) -> Vec<u8> {
            let mut out = vec![b'I', b'I', 42, 0, 0, 0, 0, 0];
            let mut link = 4usize;
            for &(w, h, shade, subfile) in frames {
                let data = out.len() as u32;
                out.extend(std::iter::repeat_n(shade, (w * h) as usize));
                if out.len() % 2 == 1 {
                    out.push(0);
                }
                let ifd = out.len() as u32;
                out[link..link + 4].copy_from_slice(&ifd.to_le_bytes());
                let mut tags: Vec<(u16, u16, u32)> = vec![(256, 4, w), (257, 4, h), (258, 3, 8), (259, 3, 1), (262, 3, 1), (273, 4, data), (277, 3, 1), (278, 4, h), (279, 4, w * h)];
                if let Some(s) = subfile {
                    tags.insert(0, (254, 4, s));
                }
                out.extend((tags.len() as u16).to_le_bytes());
                for (tag, kind, value) in tags {
                    out.extend(tag.to_le_bytes());
                    out.extend(kind.to_le_bytes());
                    out.extend(1u32.to_le_bytes());
                    out.extend(value.to_le_bytes());
                }
                link = out.len();
                out.extend(0u32.to_le_bytes());
            }
            out
        }

        fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let p = std::env::temp_dir().join(format!("sleuth-ocr-test-{}-{name}", std::process::id()));
            std::fs::write(&p, bytes).unwrap();
            p
        }

        #[test]
        fn every_tiff_frame_is_a_page_and_thumbnails_are_not() {
            let p = temp("frames.tif", &tiff(&[(40, 30, 200, None), (8, 6, 90, Some(1)), (20, 50, 10, Some(0))]));
            let frames = tiff_frames(&p).unwrap().expect("a TIFF");
            assert_eq!(frames.len(), 2, "the reduced-resolution copy is not a page");
            let (a, b) = (load(&p, Some(frames[0])).unwrap(), load(&p, Some(frames[1])).unwrap());
            assert_eq!((a.width(), a.height()), (40, 30));
            assert_eq!((b.width(), b.height()), (20, 50));
            assert_eq!(a.cut(0, 0, 2, 1).into_raw(), vec![200; 6]);
            assert_eq!(b.cut(18, 49, 2, 1).into_raw(), vec![10; 6]);
            let _ = std::fs::remove_file(p);
        }

        #[test]
        fn other_files_are_not_tiff() {
            let p = temp("not.tif", b"\x89PNG\r\n\x1a\n and more");
            assert!(tiff_frames(&p).unwrap().is_none());
            let _ = std::fs::remove_file(p);
        }

        #[test]
        fn a_looping_tiff_ends() {
            let mut bytes = tiff(&[(4, 4, 1, None)]);
            let n = bytes.len();
            // Point the last directory back at itself.
            let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            bytes[n - 4..].copy_from_slice(&ifd.to_le_bytes());
            let p = temp("loop.tif", &bytes);
            assert_eq!(tiff_frames(&p).unwrap().unwrap().len(), 1);
            let _ = std::fs::remove_file(p);
        }

        #[test]
        fn oversized_images_are_refused_with_the_reason() {
            let e = within_budget(20_000, 20_000).unwrap_err().to_string();
            assert!(e.contains("megapixels"), "{e}");
            assert!(within_budget(9933, 14043).is_ok(), "A0 at 300 dpi is read");
        }

        #[test]
        fn grey_tiles_are_widened_and_padded_white() {
            let g = Pixels::Gray(GrayImage::from_raw(2, 2, vec![0, 50, 100, 150]).unwrap());
            assert_eq!(g.cut(1, 0, 2, 2).into_raw(), vec![50, 50, 50, 255, 255, 255, 150, 150, 150, 255, 255, 255]);
        }
    }
}

/// A rectangle of an image, clipped to it and copied row by row. (The image
/// crate's own crop copies pixel by pixel, which took 0.15 s for every tile
/// cut from a band.)
fn cut_rgb(img: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> RgbImage {
    let (x, y) = (x.min(img.width()), y.min(img.height()));
    let (w, h) = (w.min(img.width() - x), h.min(img.height() - y));
    let (stride, raw) = (img.width() as usize * 3, img.as_raw());
    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 3;
        out.extend_from_slice(&raw[start..start + w as usize * 3]);
    }
    RgbImage::from_raw(w, h, out).expect("buffer sized for the rectangle")
}

/// A detected box moved by `(dx, dy)`, with its upright bounds. `None` for a
/// box without points.
fn region(quad: BoundingBox, tile: usize, dx: f32, dy: f32) -> Option<Region> {
    if quad.points.is_empty() {
        return None;
    }
    let quad = if dx == 0.0 && dy == 0.0 { quad } else { quad.translate(dx, dy) };
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &quad.points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(Region { quad, x0, y0, x1, y1, cut: 0, near: 0, tile, joined: false, spares: Vec::new() })
}

/// Where tiles start along one axis and how long each is: full tiles `step`
/// apart, the last one shortened to end at `a1`.
fn cuts(a0: i32, a1: i32, tile: i32, step: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let mut p = a0;
    loop {
        let len = tile.min(a1 - p);
        out.push((p, len));
        if p + len >= a1 {
            return out;
        }
        p += step;
    }
}

/// Equal tiles along one axis: as few as cover `a0..a1` with at least
/// `overlap` shared between neighbours, all of one length (a multiple of
/// `unit` where the tile size allows), evenly spread.
fn even_cuts(a0: i32, a1: i32, tile: i32, overlap: i32, unit: i32) -> Vec<(i32, i32)> {
    let len = a1 - a0;
    if len <= tile {
        return vec![(a0, len)];
    }
    let n = (len - overlap + (tile - overlap) - 1) / (tile - overlap);
    let need = (len + (n - 1) * overlap + n - 1) / n;
    let unit = unit.max(1);
    let t = ((need + unit - 1) / unit * unit).min(tile);
    (0..n).map(|i| (a0 + ((len - t) as i64 * i as i64 / (n - 1) as i64) as i32, t)).collect()
}

/// Two tiles of one band see the same pixels in their overlap, so they find
/// the same text twice. Where two regions from different tiles cover nearly
/// the same box, only one is recognised at first: not the one cut by a tile
/// edge, else the larger. Both would be read from the same pixels of the
/// band. The other box is kept with it as a spare: if the line reads with
/// low confidence it is read from the spare too and the better reading
/// stays, which is what reading every tile's regions used to give.
/// Returns how many were dropped.
///
/// Regions that only partly coincide are all read and left to [`dedupe`],
/// which knows what each one says: a part cut by a tile edge must not be
/// dropped for the larger box that covers it before it is known that the
/// larger box reads as anything.
fn drop_twins(regions: &mut Vec<Region>) -> u32 {
    let n = regions.len();
    let mut drop = vec![false; n];
    for i in 0..n {
        if drop[i] {
            continue;
        }
        for j in (i + 1)..n {
            if drop[j] || regions[i].tile == regions[j].tile {
                continue;
            }
            let (a, b) = (&regions[i], &regions[j]);
            if a.overlap(b) < 0.85 || a.area().min(b.area()) < 0.7 * a.area().max(b.area()) {
                continue;
            }
            let lose_i = match (a.cut != 0, b.cut != 0) {
                (true, false) => true,
                (false, true) => false,
                _ => a.area() < b.area(),
            };
            let (keep, lose) = if lose_i { (j, i) } else { (i, j) };
            drop[lose] = true;
            let mut spares = std::mem::take(&mut regions[lose].spares);
            spares.push(regions[lose].quad.clone());
            regions[keep].spares.append(&mut spares);
            if lose_i {
                break;
            }
        }
    }
    let mut k = 0;
    regions.retain(|_| {
        let keep = !drop[k];
        k += 1;
        keep
    });
    drop.iter().filter(|d| **d).count() as u32
}

/// How much of the longer of two spans the two share.
fn span_shared(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0) / (a1 - a0).max(b1 - b0).max(1.0)
}

/// Join the parts of a line that a tile edge cut in two: one tile saw it up
/// to its right (or bottom) edge, the next tile from its left (or top) edge
/// on, and both parts cover the strip the two tiles share at the same height.
/// The joined box is added to the regions and read whole from the band, so a
/// line longer than the tile overlap is no longer only there in pieces. The
/// parts stay and are read as well: [`dedupe`] drops them when the joined
/// line turns out to hold their text. Only upright boxes are joined.
/// Returns how many joins were made.
fn stitch(regions: &mut Vec<Region>) -> u32 {
    let mut joined = 0;
    loop {
        let open: Vec<usize> = (0..regions.len()).filter(|&i| regions[i].near != 0 && regions[i].upright()).collect();
        let mut pair = None;
        'find: for &i in &open {
            for &j in &open {
                let (a, b) = (&regions[i], &regions[j]);
                if i == j || a.tile == b.tile {
                    continue;
                }
                // `a` is the left part of the line, `b` the right part...
                let side = a.near & CUT_R != 0 && b.near & CUT_L != 0 && a.x0 < b.x0 && a.x1 < b.x1 && b.x0 <= a.x1 && span_shared(a.y0, a.y1, b.y0, b.y1) >= 0.7;
                // ...or `a` the upper part and `b` the lower.
                let below = a.near & CUT_B != 0 && b.near & CUT_T != 0 && a.y0 < b.y0 && a.y1 < b.y1 && b.y0 <= a.y1 && span_shared(a.x0, a.x1, b.x0, b.x1) >= 0.7;
                let (w, h) = (a.x1.max(b.x1) - a.x0.min(b.x0), a.y1.max(b.y1) - a.y0.min(b.y0));
                if (side || below) && w.max(h) <= JOIN_MAX_RATIO * w.min(h).max(1.0) {
                    pair = Some((i, j, side));
                    break 'find;
                }
            }
        }
        let Some((i, j, side)) = pair else { return joined };
        let (a, b) = (&regions[i], &regions[j]);
        // The joined box is cut only where its outer ends were.
        let ends = |a: u8, b: u8| if side { (a & CUT_L) | (b & CUT_R) | ((a | b) & (CUT_T | CUT_B)) } else { (a & CUT_T) | (b & CUT_B) | ((a | b) & (CUT_L | CUT_R)) };
        let (x0, y0, x1, y1) = (a.x0.min(b.x0), a.y0.min(b.y0), a.x1.max(b.x1), a.y1.max(b.y1));
        let whole = Region { quad: BoundingBox::from_coords(x0, y0, x1, y1), x0, y0, x1, y1, cut: ends(a.cut, b.cut), near: ends(a.near, b.near), tile: a.tile, joined: true, spares: Vec::new() };
        // The parts are done; the joined line may go on into a third tile.
        (regions[i].near, regions[j].near) = (0, 0);
        regions.push(whole);
        joined += 1;
    }
}

fn page_text(page: u32, found: Vec<Found>, full_w: i32, full_h: i32) -> PageText {
    let lines = found
        .into_iter()
        .flat_map(|f| {
            let bbox = NormBox { x: f.x0 / full_w as f32, y: f.y0 / full_h as f32, w: (f.x1 - f.x0) / full_w as f32, h: (f.y1 - f.y0) / full_h as f32 };
            let main = Line { text: f.text.clone(), conf: f.conf, source: TextSource::Ocr, bbox, alt: false };
            let alt = f.alt.map(|t| Line { text: t, conf: f.conf, source: TextSource::Ocr, bbox, alt: true });
            std::iter::once(main).chain(alt)
        })
        .collect();
    PageText { page, lines }
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
    /// Read from a box joined out of two parts (see [`stitch`]).
    joined: bool,
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
/// long lines that no tile holds whole are not lost. Of two readings of the
/// same box the more confident stays.
///
/// With `keep_parts`, a line lying inside a clearly larger one is not taken
/// for a second reading of it: it is dropped only when the larger line holds
/// its text (and would be found as surely), and kept otherwise. Without it
/// the more confident of the two stays whatever their sizes, as in Sleuth
/// 1.0, where a fragment could displace the whole line.
///
/// A line joined from two parts is a guess that the parts belong together,
/// so it never removes a part by its position alone, only by holding the
/// part's text.
fn dedupe(mut found: Vec<Found>, keep_parts: bool) -> Vec<Found> {
    let n = found.len();
    let mut drop = vec![false; n];
    for i in 0..n {
        if !found[i].cut {
            continue;
        }
        for j in 0..n {
            if i != j && !drop[j] && !found[j].joined && found[i].overlap(&found[j]) >= 0.5 && (!found[j].cut || found[j].area() > found[i].area()) {
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
            if drop[j] || found[i].overlap(&found[j]) < 0.6 {
                continue;
            }
            let same_box = found[i].area().min(found[j].area()) >= 0.7 * found[i].area().max(found[j].area());
            let worse = if keep_parts && !same_box {
                let (part, whole) = if found[i].area() < found[j].area() { (i, j) } else { (j, i) };
                // Dropping the part must not turn a sure match into an unsure one.
                let as_sure = found[whole].conf >= LOW_CONFIDENCE || found[part].conf < LOW_CONFIDENCE;
                if as_sure && sep_form(&found[whole].text).contains(&sep_form(&found[part].text)) { part } else { continue }
            } else if found[j].conf > found[i].conf {
                i
            } else {
                j
            };
            drop[worse] = true;
            if worse == i {
                break;
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

/// Join overlapping rectangles so no area is read twice.
fn merge_rects(mut rects: Vec<(i32, i32, i32, i32)>) -> Vec<(i32, i32, i32, i32)> {
    loop {
        let mut merged = false;
        'outer: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (rects[i], rects[j]);
                if a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3 {
                    rects[i] = (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3));
                    rects.remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            return rects;
        }
    }
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
        Found { text: text.into(), conf, x0, y0: 0.0, x1, y1: 10.0, cut, joined: false, alt: None }
    }

    #[test]
    fn cut_copy_is_dropped_whole_copy_kept() {
        let out = dedupe(vec![f("10\"-P-12", 1500.0, 1600.0, true, 0.9), f("10\"-P-1001-A1B", 1500.0, 1700.0, false, 0.95)], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "10\"-P-1001-A1B");
    }

    #[test]
    fn long_cut_line_without_whole_copy_is_kept() {
        let out = dedupe(vec![f("A VERY LONG NOTE", 100.0, 1600.0, true, 0.9)], true);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn overlapping_regions_merge() {
        let r = merge_rects(vec![(0, 0, 10, 10), (5, 5, 20, 20), (50, 50, 60, 60)]);
        assert_eq!(r, vec![(0, 0, 20, 20), (50, 50, 60, 60)]);
    }

    fn r(x0: f32, y0: f32, x1: f32, y1: f32, cut: u8, tile: usize) -> Region {
        Region { quad: BoundingBox::from_coords(x0, y0, x1, y1), x0, y0, x1, y1, cut, near: cut, tile, joined: false, spares: Vec::new() }
    }

    #[test]
    fn a_tile_is_cut_as_the_image_crate_cuts_it() {
        let img = RgbImage::from_fn(13, 9, |x, y| image::Rgb([x as u8, y as u8, (x * y) as u8]));
        for (x, y, w, h) in [(0, 0, 13, 9), (3, 2, 5, 4), (10, 7, 3, 2), (11, 6, 9, 9), (13, 9, 2, 2)] {
            assert_eq!(cut_rgb(&img, x, y, w, h), imageops::crop_imm(&img, x, y, w, h).to_image(), "{x},{y} {w}x{h}");
        }
    }

    #[test]
    fn tiles_follow_the_old_grid() {
        // A3 at 300 dpi, 1600 px tiles, 512 px overlap: five columns, three rows.
        assert_eq!(cuts(0, 4961, 1600, 1088), vec![(0, 1600), (1088, 1600), (2176, 1600), (3264, 1600), (4352, 609)]);
        assert_eq!(cuts(0, 3508, 1600, 1088), vec![(0, 1600), (1088, 1600), (2176, 1332)]);
        assert_eq!(cuts(100, 900, 1600, 1088), vec![(100, 800)]);
    }

    #[test]
    fn even_tiles_cover_the_axis_with_the_overlap_asked_for() {
        for (len, tile, overlap) in [(4961, 1600, 256), (3508, 1600, 256), (7016, 2400, 384), (1601, 1600, 512), (9933, 1600, 192), (1200, 1600, 256)] {
            let v = even_cuts(10, 10 + len, tile, overlap, 32);
            assert_eq!(v[0].0, 10);
            assert_eq!(v.last().map(|t| t.0 + t.1), Some(10 + len), "{len}/{tile}/{overlap}: ends at the edge");
            assert!(v.iter().all(|t| t.1 == v[0].1 && t.1 <= tile), "{len}/{tile}/{overlap}: one size");
            assert!(v.windows(2).all(|w| w[0].0 + w[0].1 - w[1].0 >= overlap), "{len}/{tile}/{overlap}: overlap kept: {v:?}");
        }
        assert_eq!(even_cuts(0, 4961, 1600, 256, 32).len(), 4);
        assert_eq!(even_cuts(0, 4961, 1600, 256, 32)[0].1 % 32, 0);
        // 2400 px tiles detected at 1600: sizes in steps of 48 page pixels.
        assert_eq!(even_cuts(0, 4961, 2400, 384, 48)[0].1 % 48, 0);
    }

    #[test]
    fn the_same_box_from_two_tiles_is_read_once() {
        let mut v = vec![r(1100.0, 50.0, 1300.0, 80.0, 0, 0), r(1101.0, 49.0, 1302.0, 81.0, 0, 1), r(200.0, 50.0, 400.0, 80.0, 0, 0)];
        assert_eq!(drop_twins(&mut v), 1);
        assert_eq!(v.len(), 2);
        // The box that was not read is kept for a second look.
        assert_eq!((v[0].spares.len(), v[1].spares.len()), (1, 0));
        // A box three tiles found keeps both others.
        let mut v = vec![r(1100.0, 50.0, 1300.0, 80.0, 0, 0), r(1101.0, 49.0, 1302.0, 81.0, 0, 1), r(1099.0, 50.0, 1301.0, 82.0, 0, 2)];
        assert_eq!(drop_twins(&mut v), 2);
        assert_eq!((v.len(), v[0].spares.len()), (1, 2));
        // Within one tile nothing is a twin.
        let mut v = vec![r(10.0, 10.0, 100.0, 40.0, 0, 0), r(11.0, 10.0, 101.0, 40.0, 0, 0)];
        assert_eq!(drop_twins(&mut v), 0);
    }

    #[test]
    fn a_cut_copy_inside_a_whole_one_is_not_read() {
        // Tile 0 saw nearly all of the line, up to its edge at x = 1600; tile 1 saw all of it.
        let mut v = vec![r(1300.0, 50.0, 1600.0, 80.0, CUT_R, 0), r(1298.0, 49.0, 1640.0, 81.0, 0, 1)];
        assert_eq!(drop_twins(&mut v), 1);
        assert_eq!(v[0].x1, 1640.0);
        // A small whole box inside a large one is kept: it may be other text.
        let mut v = vec![r(1500.0, 50.0, 1560.0, 80.0, 0, 0), r(1448.0, 49.0, 1800.0, 81.0, 0, 1)];
        assert_eq!(drop_twins(&mut v), 0);
        // So is a short cut part inside a much larger box: the large box may
        // read as nothing, and then the part is all there is.
        let mut v = vec![r(1088.0, 50.0, 1190.0, 80.0, CUT_L, 1), r(190.0, 40.0, 1190.0, 120.0, 0, 0)];
        assert_eq!(drop_twins(&mut v), 0);
    }

    #[test]
    fn a_line_cut_by_a_tile_edge_is_joined() {
        // Tiles at x = 0..1600 and 1344..2944 (256 px shared). A line from
        // 1000 to 2000 is whole in neither.
        let mut v = vec![r(1000.0, 500.0, 1600.0, 530.0, CUT_R, 0), r(1344.0, 501.0, 2000.0, 531.0, CUT_L, 1), r(1000.0, 600.0, 1600.0, 630.0, CUT_R, 0)];
        assert_eq!(stitch(&mut v), 1);
        // The joined line is added; its parts stay to be read too.
        assert_eq!(v.len(), 4);
        assert_eq!((v[3].x0, v[3].x1, v[3].cut, v[3].joined), (1000.0, 2000.0, 0, true));
        assert_eq!((v[0].cut, v[1].cut), (CUT_R, CUT_L));
        // Parts at different heights are different lines.
        let mut v = vec![r(1000.0, 500.0, 1600.0, 530.0, CUT_R, 0), r(1344.0, 520.0, 2000.0, 550.0, CUT_L, 1)];
        assert_eq!(stitch(&mut v), 0);
    }

    #[test]
    fn parts_of_a_line_too_long_to_read_whole_stay_apart() {
        // 30 px high and 2900 px long: wider than the recogniser's input.
        let mut v = vec![r(100.0, 500.0, 1600.0, 530.0, CUT_R, 0), r(1344.0, 500.0, 3000.0, 530.0, CUT_L, 1)];
        assert_eq!(stitch(&mut v), 0);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn a_line_across_three_tiles_is_joined_twice() {
        let mut v = vec![r(1000.0, 500.0, 1600.0, 540.0, CUT_R, 0), r(1344.0, 500.0, 2944.0, 540.0, CUT_L | CUT_R, 1), r(2688.0, 500.0, 3000.0, 540.0, CUT_L, 2)];
        assert_eq!(stitch(&mut v), 2);
        assert_eq!((v.len(), v[4].x0, v[4].x1, v[4].cut), (5, 1000.0, 3000.0, 0));
    }

    #[test]
    fn signature_tells_the_pipelines_apart() {
        let old = OcrOptions::legacy(1);
        let mut new = old.clone();
        new.pool = true;
        new.page_render = true;
        assert_ne!(config_signature(&old), config_signature(&new));
        new.threads = 7;
        let same = config_signature(&new);
        new.stitch = true;
        assert_ne!(same, config_signature(&new));
        // Threads and ONNX Runtime plumbing do not change what is read.
        let mut plumbing = old.clone();
        plumbing.threads = 4;
        plumbing.graph_opt = Some(3);
        assert_eq!(config_signature(&old), config_signature(&plumbing));
    }

    #[test]
    fn overlap_duplicates_keep_best() {
        for keep_parts in [false, true] {
            let out = dedupe(vec![f("PSV-1203", 10.0, 90.0, false, 0.8), f("PSV-1203", 11.0, 91.0, false, 0.97)], keep_parts);
            assert_eq!(out.len(), 1);
            assert!(out[0].conf > 0.9);
        }
    }

    #[test]
    fn a_fragment_does_not_displace_the_whole_line() {
        // One tile read the start of a line (its box stopped short of the
        // edge, so it is not marked cut) a little more confidently than the
        // next tile read all of it.
        let lines = || vec![f("DRG. TITLE:", 1240.0, 1434.0, false, 0.99), f("DRG. TITLE: PIPING ISOMETRIC", 1239.0, 1664.0, false, 0.97)];
        let old = dedupe(lines(), false);
        assert_eq!(old.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["DRG. TITLE:"], "Sleuth 1.0 kept the fragment");
        let new = dedupe(lines(), true);
        assert_eq!(new.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["DRG. TITLE: PIPING ISOMETRIC"]);
    }

    #[test]
    fn a_joined_line_removes_its_parts_only_by_holding_their_text() {
        let part = |text: &str, x0: f32, x1: f32| f(text, x0, x1, true, 0.97);
        let whole = |text: &str| Found { joined: true, ..f(text, 1000.0, 2000.0, false, 0.95) };
        // Read well: the two cut parts go.
        let out = dedupe(vec![part("STUD BOLT A193", 1000.0, 1600.0), part("A193 B7 / A194 2H", 1344.0, 2000.0), whole("STUD BOLT A193 B7 / A194 2H")], true);
        assert_eq!(out.len(), 1);
        // Read badly: the parts stay, as they did before lines were joined.
        let out = dedupe(vec![part("STUD BOLT A193", 1000.0, 1600.0), part("A193 B7 / A194 2H", 1344.0, 2000.0), whole("5TUO B0LT")], true);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn other_text_inside_a_larger_box_is_kept() {
        // A tag inside the box of a longer, differently read line is not a
        // second reading of that line.
        let out = dedupe(vec![f("FV-2201", 100.0, 200.0, false, 0.9), f("TO E-102 VIA FV-22O1 BYPASS", 0.0, 600.0, false, 0.99)], true);
        assert_eq!(out.len(), 2);
        // Held by the larger line, but only in an unsure reading: the sure one stays.
        let out = dedupe(vec![f("FV-2201", 100.0, 200.0, false, 0.95), f("TO E-102 VIA FV-2201", 0.0, 600.0, false, 0.6)], true);
        assert_eq!(out.len(), 2);
        let out = dedupe(vec![f("FV-2201", 100.0, 200.0, false, 0.95), f("TO E-102 VIA FV-2201", 0.0, 600.0, false, 0.9)], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "TO E-102 VIA FV-2201");
    }
}
