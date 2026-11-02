//! Messages between the Sleuth window and `sleuth-worker.exe`.
//!
//! Transport: a per-user named pipe carrying one JSON object per line. The
//! window sends [`Envelope<Request>`]; the worker answers with any number of
//! [`Envelope<Event>`] carrying the same `id`, ending with [`Event::Done`] or
//! [`Event::Error`]. Unsolicited events (status changes) use `id = 0`.
//!
//! The same types travel over the office server's HTTP API, so a result from
//! the server looks exactly like a local one apart from [`Hit::origin`].

use crate::analyzer::MatchClass;
use crate::paths::Drive;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};

pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let safe: String = user.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\Sleuth-{safe}")
}

/// 2: Sleuth 1.0 (projects, tags, revisions, corrections, office server).
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: u64,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum Request {
    Hello { protocol: u32 },
    /// Search the index (names and read PDFs). With `deep`, also walk
    /// `roots` live and read any unread PDFs found on the way.
    Search {
        query: String,
        #[serde(default)]
        roots: Vec<String>,
        /// Limit to one project's folders (by name).
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        deep: bool,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    Cancel { search_id: u64 },
    /// Render one page as PNG with the matched boxes.
    Preview {
        path: String,
        page: u32,
        query: String,
        max_px: u32,
        /// Also mark every occurrence of this engineering tag (normalised).
        #[serde(default)]
        tag: Option<String>,
        /// The file comes from the office server's index.
        #[serde(default)]
        office: bool,
    },
    Status,
    Drives,
    GetSettings,
    SetSettings { settings: Box<Settings> },
    RescanNow,
    /// Read one file right away (used by the "Read now" action).
    ReadNow { path: String },
    /// Test a shared cache folder before saving it.
    CheckCache { dir: String },
    /// Distinct engineering tags, most documents first.
    Tags {
        #[serde(default)]
        category: Option<String>,
        /// Tags whose normalised form contains this (separators ignored).
        #[serde(default)]
        contains: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default = "default_page")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        /// Ask the office server instead of this PC's index.
        #[serde(default)]
        office: bool,
    },
    /// Every place one tag was found.
    TagOccurrences {
        norm: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        office: bool,
    },
    /// Drawings grouped by drawing number with their revisions.
    Revisions {
        #[serde(default)]
        contains: Option<String>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default = "default_page")]
        limit: usize,
        #[serde(default)]
        office: bool,
    },
    /// Compare the tags of two revisions of a drawing.
    Compare {
        old: String,
        new: String,
        #[serde(default)]
        office: bool,
    },
    /// Documents that share tags with this one.
    Related {
        path: String,
        #[serde(default)]
        office: bool,
    },
    /// Title-block details, page classes and how the file was read.
    DocInfo {
        path: String,
        #[serde(default)]
        office: bool,
    },
    /// Lines read with low confidence, for checking and correcting.
    LowConfidence {
        #[serde(default)]
        path: Option<String>,
        #[serde(default = "default_page")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    /// A rectangle of a page as PNG (correction studio).
    Crop { path: String, page: u32, bbox: NormBox, max_px: u32 },
    /// Record a human-checked reading of one line. The original stays.
    Correct { line_id: i64, text: String },
    /// Remove a correction made earlier.
    Uncorrect { correction_id: i64 },
    /// Index health for the monitor page.
    Monitor,
    History,
    Pin { query: String, pinned: bool },
    /// Test an office server address and access key before saving them.
    /// An empty `token` tests the key already saved.
    CheckServer { url: String, token: String, ca_file: String },
    /// Save the office server connection (the key is stored encrypted for
    /// this Windows user). An empty `url` returns to standalone use.
    SetServer { url: String, token: String, ca_file: String },
    Shutdown,
}

fn default_limit() -> usize {
    500
}

fn default_page() -> usize {
    200
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSource {
    FileName,
    TextLayer,
    Ocr,
    /// Found by name during a deep search, waiting to be read.
    Pending,
    /// Typed in by a person who checked the drawing (correction studio).
    Verified,
    /// Title-block details (drawing number, revision).
    Metadata,
}

/// Rectangle in page coordinates normalised to 0..1, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct NormBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NormBox {
    pub fn area(&self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }
    /// Intersection over the smaller box (1.0 when one contains the other).
    pub fn overlap(&self, o: &NormBox) -> f32 {
        let w = ((self.x + self.w).min(o.x + o.w) - self.x.max(o.x)).max(0.0);
        let h = ((self.y + self.h).min(o.y + o.h) - self.y.max(o.y)).max(0.0);
        let small = self.area().min(o.area());
        if small <= 0.0 { 0.0 } else { w * h / small }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// This PC's index.
    #[default]
    Local,
    /// The office server's index.
    Office,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// UNC (network) or local path, as stored.
    pub path: String,
    /// Path with this PC's drive letter where one is mapped.
    pub display_path: String,
    pub name: String,
    /// 1-based page, absent for file-name matches.
    pub page: Option<u32>,
    /// The text that matched, exactly as read.
    pub found: String,
    pub class: MatchClass,
    pub source: TextSource,
    /// 0..1; 1.0 for file names and text layers.
    pub confidence: f32,
    /// Number of matching lines in the file.
    pub hits: u32,
    pub size: u64,
    /// Unix seconds.
    pub modified: i64,
    /// Why this is a match, in words.
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub origin: Origin,
    /// Document id on the office server (for its page previews).
    #[serde(default)]
    pub doc_id: Option<i64>,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootState {
    Waiting,
    Searching,
    Done,
    Offline,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootProgress {
    pub root: String,
    pub label: String,
    pub state: RootState,
    pub folders_done: u64,
    pub files_seen: u64,
    pub hits: u64,
    /// Folders given up on (timeout or access denied), with the reason.
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    /// Standalone: no office server configured.
    #[default]
    NotConfigured,
    Connected,
    /// Configured but not answering; office results are unavailable.
    Offline,
    /// Answering, but refusing this PC (key, certificate or version).
    Degraded,
    /// Connected and still reading its first pass.
    Syncing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerStatus {
    pub state: ServerState,
    pub message: String,
    pub version: String,
    /// Folders the server reads that this user may search.
    pub roots: Vec<String>,
    pub projects: Vec<String>,
    /// Unix seconds of the last successful contact.
    pub last_ok: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub files_indexed: u64,
    pub pdfs_read: u64,
    pub pdfs_waiting: u64,
    pub pdfs_failed: u64,
    /// Pages read per hour over the last hour, for the status bar.
    pub pages_per_hour: u64,
    /// "reading", "paused: PC busy", "idle", "loading models"...
    pub reader_state: String,
    pub current_file: Option<String>,
    pub ocr_model: String,
    pub worker_version: String,
    #[serde(default)]
    pub cache: CacheCheck,
    /// "CPU" or "DirectML (adapter name)".
    #[serde(default)]
    pub ocr_backend: String,
    #[serde(default)]
    pub server: ServerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    /// Folder reachable and writable.
    Ok,
    /// No folder chosen, or its drive letter is not mapped on this PC.
    #[default]
    NotSet,
    /// The server answers but the folder cannot be created or written.
    NoAccess,
    /// The server does not answer right now (off-site, network down).
    Offline,
    /// Switched off by IT in settings.json.
    Disabled,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheCheck {
    pub state: CacheState,
    /// Network path actually used.
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failed {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagSummary {
    /// Upper-case letters and digits: the tag's identity.
    pub norm: String,
    /// The most common way it is written.
    pub display: String,
    pub category: String,
    pub documents: u32,
    pub occurrences: u32,
    /// Lowest reading confidence among its occurrences.
    pub min_conf: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
    pub path: String,
    pub display_path: String,
    pub name: String,
    pub page: u32,
    /// As read on the page.
    pub raw: String,
    pub conf: f32,
    pub source: TextSource,
    pub bbox: NormBox,
    pub drawing_no: Option<String>,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrawingMeta {
    pub drawing_no: Option<String>,
    pub title: Option<String>,
    pub revision: Option<String>,
    pub project: Option<String>,
    pub discipline: Option<String>,
    pub sheet: Option<String>,
    pub status: Option<String>,
    /// Where each value came from: (field, "page 1 title block: DRAWING No. ISO-1001-01").
    pub evidence: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocRev {
    pub path: String,
    pub display_path: String,
    pub name: String,
    pub revision: Option<String>,
    pub title: Option<String>,
    pub modified: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionGroup {
    pub drawing_no: String,
    /// Oldest revision first.
    pub docs: Vec<DocRev>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    /// A tag in the same place reads differently.
    Changed,
    /// The difference may only be an OCR misreading; check both pages.
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagChange {
    pub kind: ChangeKind,
    pub category: String,
    /// Reading in the old revision.
    pub old: Option<String>,
    /// Reading in the new revision.
    pub new: Option<String>,
    pub old_page: Option<u32>,
    pub new_page: Option<u32>,
    pub old_box: Option<NormBox>,
    pub new_box: Option<NormBox>,
    pub old_conf: Option<f32>,
    pub new_conf: Option<f32>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Related {
    pub path: String,
    pub display_path: String,
    pub name: String,
    pub drawing_no: Option<String>,
    /// Shared tags (display form), most specific first, at most 12.
    pub shared: Vec<String>,
    pub shared_count: u32,
    /// First page where a shared tag appears.
    #[serde(default)]
    pub page: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInfo {
    pub page: u32,
    /// searchable, scanned, hybrid, vector_outlined, sparse, damaged
    pub class: String,
    pub chars: u32,
    pub image_coverage: f32,
    pub ocr_tiles: u32,
    pub ocr_tiles_skipped: u32,
    pub ocr_millis: u64,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provenance {
    /// BLAKE3 of everything that decides what reading produces.
    pub pipeline: String,
    pub reader_version: u32,
    pub model: String,
    pub model_hash: String,
    pub backend: String,
    pub dpi: u32,
    pub read_by: String,
    pub read_at: i64,
    /// "read here", "shared cache", "copy of an identical file", "office server"
    pub obtained: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LowLine {
    pub line_id: i64,
    pub path: String,
    pub display_path: String,
    pub name: String,
    pub page: u32,
    pub text: String,
    /// The other reader's reading when the two disagreed.
    pub alt: Option<String>,
    pub conf: f32,
    pub bbox: NormBox,
    /// A correction already recorded for this line.
    pub corrected: Option<String>,
    pub correction_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub query: String,
    pub at: i64,
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Monitor {
    pub status: Status,
    pub db_bytes: u64,
    pub pages_total: u64,
    pub pages_by_class: Vec<(String, u64)>,
    pub ocr_millis_per_page: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_claims_waited: u64,
    pub search_p50_ms: u64,
    pub search_p95_ms: u64,
    pub searches: u64,
    pub tags: u64,
    pub tag_occurrences: u64,
    pub drawings_with_metadata: u64,
    pub corrections: u64,
    /// (root, state, unix seconds of last complete pass, note)
    pub roots: Vec<(String, String, i64, String)>,
    /// Recent events, newest first: (unix seconds, level, message).
    pub events: Vec<(i64, String, String)>,
    pub rule_errors: Vec<String>,
    pub backend_note: String,
    pub change_journal: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    Hello { protocol: u32, version: String },
    Hits { hits: Vec<Hit> },
    /// A previously reported pending file was read; replaces its row.
    Resolved { path: String, hits: Vec<Hit> },
    Progress { roots: Vec<RootProgress> },
    Preview { png_base64: String, width: u32, height: u32, pages: u32, boxes: Vec<NormBox>, source: TextSource },
    Status { status: Status, failed: Vec<Failed> },
    Drives { drives: Vec<Drive> },
    Settings { settings: Box<Settings> },
    Cache { check: CacheCheck },
    Tags { tags: Vec<TagSummary>, total: u64 },
    Occurrences { items: Vec<Occurrence> },
    Revisions { groups: Vec<RevisionGroup> },
    Comparison { old: DrawingMeta, new: DrawingMeta, changes: Vec<TagChange>, unchanged: u32 },
    Related { items: Vec<Related> },
    DocInfo { metadata: DrawingMeta, pages: Vec<PageInfo>, provenance: Provenance, tags: u32 },
    LowConfidence { items: Vec<LowLine>, total: u64 },
    Image { png_base64: String, width: u32, height: u32 },
    Monitor { monitor: Box<Monitor> },
    History { items: Vec<HistoryItem> },
    Server { server: ServerStatus },
    Ok,
    /// The office server's part of a search, when it could not be asked.
    Notice { message: String },
    Done {
        total: usize,
        millis: u64,
        /// More results exist past `offset + limit`.
        #[serde(default)]
        more: bool,
    },
    Error { message: String },
}
