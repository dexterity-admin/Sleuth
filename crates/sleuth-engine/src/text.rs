//! What reading a file produces: lines of text with where they sit on the page.

use sleuth_core::protocol::{NormBox, TextSource};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    /// Reader confidence 0..1 (1.0 for the PDF text layer).
    pub conf: f32,
    pub source: TextSource,
    pub bbox: NormBox,
    /// A second reader disagreed; this is its reading of the same box.
    #[serde(default)]
    pub alt: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageText {
    /// 1-based.
    pub page: u32,
    pub lines: Vec<Line>,
}

/// Bump when reading changes enough that old results should be redone.
pub const READER_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileText {
    pub reader_version: u32,
    /// OCR model used, e.g. `pp-ocrv6-small`; empty when no page needed OCR.
    pub model: String,
    pub pages: Vec<PageText>,
}
