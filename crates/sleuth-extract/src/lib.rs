//! Text extraction from office and CAD files.
//!
//! Every reader here is deterministic: it takes what the file says and
//! nothing else. A file that cannot be read gives a plain error saying why
//! (password protected, damaged, too large, timed out); a reading that is
//! incomplete says so in its page note. Nothing is guessed.
//!
//! | kind   | read by                                                        |
//! |--------|----------------------------------------------------------------|
//! | Excel  | `calamine` (.xlsx .xlsm .xlsb .xls .ods): one page per sheet   |
//! | Text   | .txt .csv .tsv .log, by byte-order mark or content             |
//! | Word   | .docx .docm from the XML; .doc through the Windows text filter |
//! | Slides | .pptx .pptm from the XML; .ppt through the Windows text filter |
//! | Email  | .msg (compound file) and .eml (RFC 822)                        |
//! | Dgn    | MicroStation V7 natively; V8 through Bentley's text filter     |
//! | Dwg    | .dwg through bundled LibreDWG (child process); ASCII .dxf      |
//!
//! PDFs and images are not handled here: pdfium and OCR live in the engine.

mod cad;
mod dgn;
mod dwg;
mod dxf;
mod email;
mod excel;
pub mod ifilter;
mod ooxml;
mod out;
mod plain;

use anyhow::{Context, Result};
use sleuth_core::kinds::{FileKind, extension};
use sleuth_core::text::FileText;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) use out::Out;

/// A line is cut here and continued in the next one.
pub const MAX_LINE_CHARS: usize = 1000;

/// External programs a reading may need.
#[derive(Debug, Clone, Default)]
pub struct Tools {
    /// Bundled LibreDWG `dwgread.exe`; `None` when this copy has none.
    pub dwgread: Option<PathBuf>,
}

impl Tools {
    /// Look for the bundled tools: beside the running program, in its
    /// `resources\libredwg` or `libredwg` folder, and (development builds)
    /// in the repository's `vendor\libredwg`.
    pub fn find() -> Tools {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            dirs.push(dir.clone());
            dirs.push(dir.join("resources").join("libredwg"));
            dirs.push(dir.join("libredwg"));
        }
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("vendor").join("libredwg"));
        let name = if cfg!(windows) { "dwgread.exe" } else { "dwgread" };
        Tools { dwgread: dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file()) }
    }
}

/// How much one file may cost.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Larger files are refused ("too large").
    pub max_bytes: u64,
    /// Reading stops here; the last page's note says so.
    pub max_lines: usize,
    /// For child processes and text filters, and checked inside the parsers.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_bytes: 256 << 20, max_lines: 400_000, timeout: Duration::from_secs(120) }
    }
}

/// What this PC can read beyond the built-in parsers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Abilities {
    /// A text filter is registered for `.doc` (it also reads `.ppt`).
    pub office_filter: bool,
    /// A text filter is registered for `.dgn` (MicroStation V8 files).
    pub dgn_filter: bool,
    /// LibreDWG is present.
    pub dwg: bool,
}

// One value per file read: the size of the larger variant costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Extracted {
    Text(FileText),
    /// This PC lacks what the file needs (no Office filter, no LibreDWG):
    /// leave it for a PC that has it. The string says what is missing.
    NotHere(String),
}

/// Decided from the registry and the tool path alone; no file is opened.
pub fn abilities(tools: &Tools) -> Abilities {
    Abilities { office_filter: ifilter::registered("doc"), dgn_filter: ifilter::registered("dgn"), dwg: tools.dwgread.as_deref().is_some_and(Path::is_file) }
}

pub(crate) const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
pub(crate) const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 3, 4];

/// The first bytes of the file (fewer when it is shorter).
pub(crate) fn head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n);
    std::fs::File::open(path)?.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// An Office file saved with a password is a compound file holding an
/// `EncryptedPackage` stream instead of the zip.
pub(crate) fn encrypted_package(path: &Path) -> bool {
    cfb::open(path).is_ok_and(|cf| cf.is_stream("/EncryptedPackage"))
}

/// Read the text inside one file. PDF and Image are not handled here (pdfium
/// and OCR live in the engine).
///
/// The reader is chosen by `kind`, then by the extension and the file's
/// first bytes, so a `.doc` that is really a `.docx` is still read.
pub fn extract(path: &Path, kind: FileKind, tools: &Tools, limits: &Limits) -> Result<Extracted> {
    if matches!(kind, FileKind::Pdf | FileKind::Image | FileKind::Other) {
        anyhow::bail!("not handled by sleuth-extract");
    }
    let size = std::fs::metadata(path).with_context(|| "cannot open the file")?.len();
    if size > limits.max_bytes {
        anyhow::bail!("too large: {} MB", size.div_ceil(1 << 20));
    }
    if size == 0 {
        anyhow::bail!("damaged: the file is empty");
    }
    let deadline = Instant::now() + limits.timeout;
    let ext = extension(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let magic = head(path, 8).with_context(|| "cannot open the file")?;
    let is_zip = magic.starts_with(&ZIP_MAGIC);
    let is_ole = magic.starts_with(&OLE_MAGIC);
    let mut out = Out::new(limits, deadline);
    match kind {
        FileKind::Excel => excel::read(path, &ext, &mut out)?,
        FileKind::Text => plain::read(path, &ext, &mut out)?,
        FileKind::Word | FileKind::Slides => {
            let word = kind == FileKind::Word;
            if is_zip {
                if word { ooxml::read_docx(path, &mut out)? } else { ooxml::read_pptx(path, &mut out)? }
            } else if is_ole && encrypted_package(path) {
                anyhow::bail!("password protected");
            } else if !matches!(ext.as_str(), "doc" | "ppt") {
                // A zip-based name on something that is not a zip (an
                // editing lock file, a legacy file renamed).
                anyhow::bail!("damaged or not the format its name says: a .{ext} file is a zip, and this is not");
            } else {
                // The legacy binary formats (and whatever else carries the
                // extension, such as RTF saved as .doc) go to the filter
                // registered for the extension.
                let (class, missing) = if word {
                    ("document", "Word .doc needs the Office text filter, which this PC does not have")
                } else {
                    ("slide", "PowerPoint .ppt needs the Office text filter, which this PC does not have")
                };
                match ifilter::ifilter_text(path, deadline)? {
                    Some(lines) => {
                        out.filter_page(class, &lines);
                        if !word {
                            out.note("all slides together: the text filter does not separate them");
                        }
                    }
                    None => return Ok(Extracted::NotHere(missing.into())),
                }
            }
        }
        FileKind::Email => {
            if is_ole {
                email::read_msg(path, &mut out)?
            } else {
                email::read_eml(path, &mut out)?
            }
        }
        FileKind::Dgn => {
            if is_ole {
                match ifilter::ifilter_text(path, deadline)? {
                    Some(lines) => out.filter_page("model", &lines),
                    None => return Ok(Extracted::NotHere("MicroStation V8 files need Bentley's text filter, which this PC does not have".into())),
                }
            } else {
                dgn::read(path, &mut out)?
            }
        }
        FileKind::Dwg => {
            if dxf::looks_like_dxf(path, &ext)? {
                dxf::read(path, &mut out)?
            } else {
                let Some(dwgread) = tools.dwgread.as_deref().filter(|p| p.is_file()) else {
                    return Ok(Extracted::NotHere("AutoCAD files need LibreDWG, which is not installed with this copy".into()));
                };
                dwg::read(path, dwgread, &mut out)?
            }
        }
        FileKind::Pdf | FileKind::Image | FileKind::Other => anyhow::bail!("not handled by sleuth-extract"),
    }
    Ok(Extracted::Text(out.finish()))
}
