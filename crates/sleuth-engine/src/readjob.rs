//! One reading job, as a reader process runs it (`sleuth-worker --reader`).
//!
//! The worker hands files to reader processes in two lanes:
//! - **fast**: PDFs with a text layer and every type read by an extractor
//!   (spreadsheets, Word, email, CAD models …). No OCR model is loaded.
//! - **ocr**: scanned pages and images.
//!
//! A reader process holds no index and takes no decisions about the queue:
//! it opens the file, reads it and answers. A file that crashes its reader
//! takes only that process down; the worker records the file as unreadable.
//!
//! One PC reads a document. Before a reader opens a file on a share it
//! takes the document's lease in the office store, and when the file is
//! read it leaves the lease to run out by itself: by then the reading is
//! published and every PC has seen it. (Releasing each lease the moment it
//! is no longer needed cost the worker minutes of file-server calls.)

use crate::accel;
use crate::cache::{Claim, ClaimResult, SharedCache, hash_file};
use crate::derive::Deriver;
use crate::models;
use crate::ocr::{Backend, OcrOptions, OcrReader, config_signature};
use crate::pool::{doc_lease, path_key};
use crate::reader::{ReadOptions, pipeline_fingerprint, read_pdf, read_pdf_lanes};
use crate::text::{FileText, Line, READER_VERSION};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sleuth_core::kinds::FileKind;
use sleuth_core::protocol::Provenance;
use sleuth_core::settings::Settings;
use sleuth_extract::{Extracted, Limits, Tools};
use sleuth_office::{Acquire, Identity, Lease, Leases};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a document stays one PC's after its reader took it: longer
/// than reading it, publishing it (every ten minutes) and the other PCs
/// noticing (every minute) take together. Long OCR jobs renew it.
const DOC_LEASE: Duration = Duration::from_secs(30 * 60);

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Fast,
    Ocr,
}

impl Lane {
    pub fn name(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Ocr => "ocr",
        }
    }
}

/// What every reader process of a lane is told when it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Init {
    pub lane: Lane,
    pub settings: Box<Settings>,
    /// The office store's folder, for OCR results shared by content.
    pub cache_dir: Option<String>,
    /// Threads one OCR process may use.
    pub ocr_threads: usize,
    /// The PC name readings are signed with.
    pub pc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Canonical path.
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    /// Asked for by a person (a search, an open): shown first when done.
    #[serde(default)]
    pub urgent: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Read {
        hash: String,
        text: Box<FileText>,
    },
    /// The same content was read before by some PC (shared by content hash).
    Reused {
        hash: String,
        text: Box<FileText>,
    },
    /// A page needs OCR; the fast lane does not do that.
    NeedsOcr,
    /// Another PC is reading the same content right now.
    HeldBy {
        host: String,
        until: i64,
    },
    /// This PC lacks what the file needs; another PC may have it.
    NotHere {
        reason: String,
    },
    Failed {
        reason: String,
        /// May go away by itself (share unreachable, file locked).
        transient: bool,
    },
}

/// Settings of the OCR pipeline, and its fingerprint.
pub fn pipeline(s: &Settings, deriver: &Deriver, threads: usize) -> Result<(models::ModelSet, bool, OcrOptions, String)> {
    let (set, fetch_high) = models::choose(s.ocr_quality)?;
    let opts = OcrOptions { threads, ..Default::default() };
    let title = if s.ocr_missing_title_block { deriver.fields_hash.clone() } else { "off".into() };
    let fp = pipeline_fingerprint(&set.name, &set.hash, &config_signature(&opts), s.ocr_dpi, &title);
    Ok((set, fetch_high, opts, fp))
}

/// What a reader process keeps between jobs.
#[derive(Default)]
pub struct ReaderState {
    deriver: Option<Deriver>,
    ocr: Option<OcrReader>,
    /// DirectML failed here: this process stays on the CPU.
    cpu_only: bool,
    tools: Option<Tools>,
    /// The lease on the document being read.
    lease: std::cell::RefCell<Option<Lease>>,
}

impl ReaderState {
    /// Run one job. Never panics outward: a panic inside a reader library
    /// is caught and reported as a failure of that file.
    pub fn handle(&mut self, init: &Init, job: Job) -> Outcome {
        match take_lease(init, &job.path) {
            Ok(lease) => *self.lease.borrow_mut() = lease,
            Err(held) => return held,
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(init, &job)));
        let outcome = match r {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => Outcome::Failed { reason: friendly(&e), transient: is_transient(&e) },
            Err(_) => {
                // Whatever state the library was in is not trusted again.
                self.ocr = None;
                Outcome::Failed { reason: "the reader crashed on this file".into(), transient: false }
            }
        };
        if let Some(lease) = self.lease.borrow_mut().take() {
            match &outcome {
                // Done with for good: nobody opens it again before its
                // reading (or its failure) is in the office index.
                Outcome::Read { .. } | Outcome::Reused { .. } | Outcome::Failed { transient: false, .. } => lease.leave(),
                // Somebody else's turn, now or later.
                _ => lease.release(),
            }
        }
        outcome
    }

    fn run(&mut self, init: &Init, job: &Job) -> Result<Outcome> {
        let path = Path::new(&job.path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if self.deriver.is_none() {
            self.deriver = Some(Deriver::new(&init.settings));
        }
        match (FileKind::of(&name), init.lane) {
            (FileKind::Other, _) => bail!("this file type is not read"),
            (FileKind::Pdf, Lane::Fast) => self.pdf_text(init, path),
            (FileKind::Pdf, Lane::Ocr) => self.with_ocr(init, path, false),
            (FileKind::Image, Lane::Fast) => Ok(Outcome::NeedsOcr),
            (FileKind::Image, Lane::Ocr) => self.with_ocr(init, path, true),
            (kind, _) => self.extract(init, path, kind),
        }
    }

    fn provenance(&self, init: &Init, pipeline: &str, model: &str, model_hash: &str, backend: &str, obtained: &str) -> Provenance {
        Provenance {
            pipeline: pipeline.into(),
            reader_version: READER_VERSION,
            model: model.into(),
            model_hash: model_hash.into(),
            backend: backend.into(),
            dpi: init.settings.ocr_dpi,
            read_by: init.pc.clone(),
            read_at: now(),
            obtained: obtained.into(),
        }
    }

    /// Spreadsheets, Word, PowerPoint, email, text and CAD models.
    fn extract(&mut self, init: &Init, path: &Path, kind: FileKind) -> Result<Outcome> {
        let hash = hash_file(path).map_err(|e| anyhow!("cannot open file: {e}"))?;
        let tools = self.tools.get_or_insert_with(Tools::find);
        // Large AutoCAD drawings take LibreDWG minutes; the lane's own time
        // limit is longer still.
        let limits = Limits { timeout: Duration::from_secs(if kind == FileKind::Dwg { 600 } else { 180 }), ..Limits::default() };
        match sleuth_extract::extract(path, kind, tools, &limits)? {
            Extracted::Text(mut text) => {
                text.provenance = self.provenance(init, &format!("extract-{READER_VERSION}"), "", "", "", "read here");
                Ok(Outcome::Read { hash, text: Box::new(text) })
            }
            Extracted::NotHere(reason) => Ok(Outcome::NotHere { reason }),
        }
    }

    /// A PDF's text layer. Any page that wants OCR sends the file to the
    /// OCR lane instead (which reads the whole file once, text layer too).
    fn pdf_text(&mut self, init: &Init, path: &Path) -> Result<Outcome> {
        let hash = hash_file(path).map_err(|e| anyhow!("cannot open file: {e}"))?;
        let s = &init.settings;
        let deriver = self.deriver.as_ref().expect("set in run");
        let has_dwg = |lines: &[Line]| deriver.has_drawing_number(lines);
        let opts = ReadOptions { dpi: s.ocr_dpi, cancel: &|| false, title_block_ocr: s.ocr_missing_title_block, has_drawing_number: &has_dwg };
        let (mut text, wants_ocr) = read_pdf_lanes(path, None, &opts)?;
        if wants_ocr {
            return Ok(Outcome::NeedsOcr);
        }
        let (_, _, _, fp) = pipeline(s, deriver, init.ocr_threads.max(1))?;
        text.provenance = self.provenance(init, &fp, "", "", "", "read here");
        Ok(Outcome::Read { hash, text: Box::new(text) })
    }

    /// A PDF with scanned pages, or an image file.
    fn with_ocr(&mut self, init: &Init, path: &Path, image: bool) -> Result<Outcome> {
        let hash = hash_file(path).map_err(|e| anyhow!("cannot open file: {e}"))?;
        let s = &init.settings;
        let threads = init.ocr_threads.max(1);
        let (set, _, mut opts, fp) = pipeline(s, self.deriver.as_ref().expect("set in run"), threads)?;
        let shared = init.cache_dir.as_ref().map(SharedCache::new);
        if let Some(c) = &shared
            && let Some(mut text) = c.get(&hash, &fp)
        {
            text.provenance.obtained = "shared cache".into();
            return Ok(Outcome::Reused { hash, text: Box::new(text) });
        }
        let claim: Option<Claim> = match shared.as_ref().map(|c| c.claim(&hash, &fp)) {
            Some(ClaimResult::HeldBy { host, until }) => return Ok(Outcome::HeldBy { host, until }),
            Some(ClaimResult::Mine(c)) => Some(c),
            _ => None,
        };
        for attempt in 0..2 {
            if self.ocr.is_none() {
                let choice = if attempt == 0 && !self.cpu_only {
                    accel::choose(s.ocr_backend, &set, threads)
                } else {
                    accel::Choice { backend: Backend::Cpu, label: "CPU".into(), note: "CPU after a DirectML failure".into() }
                };
                opts.backend = choice.backend;
                let built = OcrReader::build(&set, &opts).or_else(|e| {
                    if opts.backend == Backend::Cpu {
                        return Err(e);
                    }
                    accel::record_failure(&format!("{e:#}"));
                    opts.backend = Backend::Cpu;
                    OcrReader::build(&set, &opts)
                })?;
                self.ocr = Some(built);
            }
            let reader = self.ocr.as_ref().expect("built above");
            let renewed = std::cell::Cell::new(Instant::now());
            let cancel = || {
                if renewed.get().elapsed() > Duration::from_secs(60) {
                    renewed.set(Instant::now());
                    if let Some(c) = &claim {
                        c.renew();
                    }
                    if let Some(l) = self.lease.borrow_mut().as_mut() {
                        l.renew();
                    }
                }
                false
            };
            let backend = reader.opts.backend;
            let r = if image { read_image(reader, path, &cancel) } else { self.read_scanned_pdf(init, reader, path, &cancel) };
            match r {
                Ok(mut text) => {
                    let model = text.model.clone();
                    text.provenance = self.provenance(init, &fp, &model, if model.is_empty() { "" } else { &set.hash }, if model.is_empty() { "" } else { backend.label() }, "read here");
                    if let Some(c) = &shared
                        && let Err(e) = c.put(&hash, &fp, &text)
                    {
                        tracing::warn!("shared cache write failed: {e:#}");
                    }
                    return Ok(Outcome::Read { hash, text: Box::new(text) });
                }
                // The graphics card failed: never use it again in this
                // process, and read this file again on the CPU.
                Err(e) if attempt == 0 && backend == Backend::DirectMl && format!("{e:#}").contains("OCR failed") => {
                    accel::record_failure(&format!("{e:#}"));
                    self.cpu_only = true;
                    self.ocr = None;
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow!("could not read with OCR"))
    }

    fn read_scanned_pdf(&self, init: &Init, reader: &OcrReader, path: &Path, cancel: &dyn Fn() -> bool) -> Result<FileText> {
        let s = &init.settings;
        let deriver = self.deriver.as_ref().expect("set in run");
        let has_dwg = |lines: &[Line]| deriver.has_drawing_number(lines);
        let opts = ReadOptions { dpi: s.ocr_dpi, cancel, title_block_ocr: s.ocr_missing_title_block, has_drawing_number: &has_dwg };
        read_pdf(path, Some(reader), &opts)
    }
}

/// Take the lease of a document on a share. `Err` is the answer to give
/// when another PC has it. Files on this PC's own disks need none, and
/// without the office folder there is nobody to coordinate with.
fn take_lease(init: &Init, path: &str) -> Result<Option<Lease>, Outcome> {
    if !path.starts_with(r"\\") {
        return Ok(None);
    }
    let Some(store) = &init.cache_dir else { return Ok(None) };
    let me = Identity { host: init.pc.clone(), user: std::env::var("USERNAME").unwrap_or_default(), pid: std::process::id() };
    match Leases::new(Path::new(store).join("leases"), me).acquire(&doc_lease(path_key(path)), DOC_LEASE) {
        Acquire::Mine(l) => Ok(Some(l)),
        Acquire::HeldBy(h) => Err(Outcome::HeldBy { host: h.host, until: h.until }),
        Acquire::Unavailable(_) => Ok(None),
    }
}

/// An image file (a scan saved as TIFF, JPEG, PNG or BMP): every frame is
/// a page read with OCR.
fn read_image(reader: &OcrReader, path: &Path, cancel: &dyn Fn() -> bool) -> Result<FileText> {
    use sleuth_core::protocol::PageInfo;
    let t = Instant::now();
    let (pages, stats) = reader.read_image_file(path, cancel)?;
    let per_page = t.elapsed().as_millis() as u64 / pages.len().max(1) as u64;
    let page_info = pages
        .iter()
        .zip(stats.iter())
        .map(|(p, st)| PageInfo {
            page: p.page,
            class: "image".into(),
            chars: p.lines.iter().map(|l| l.text.chars().count() as u32).sum(),
            image_coverage: 1.0,
            ocr_tiles: st.tiles,
            ocr_tiles_skipped: st.blank,
            ocr_millis: per_page,
            ..Default::default()
        })
        .collect();
    Ok(FileText { reader_version: READER_VERSION, model: reader.model.clone(), pages, page_info, provenance: Provenance::default() })
}

/// Errors that may go away by themselves: the file could not be opened or
/// read (network share unreachable, file locked by another program).
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()) || format!("{e:#}").contains("cannot open file")
}

pub fn friendly(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    if s.contains("password") {
        "password protected".into()
    } else if s.contains("damaged") {
        "damaged or not a file of its type".into()
    } else if let Some(rest) = s.strip_prefix("cannot open file: ") {
        format!("could not be opened: {}", rest.chars().take(160).collect::<String>())
    } else {
        s.chars().take(200).collect()
    }
}

/// Run this process as a reader until the worker closes its input.
pub fn serve() -> Result<()> {
    let mut state = ReaderState::default();
    sleuth_lanes::serve::<Init, Job, Outcome>(|init, job| state.handle(init, job))
}
