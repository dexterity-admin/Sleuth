//! Messages between the Sleuth window and `sleuth-worker.exe`.
//!
//! Transport: a per-user named pipe carrying one JSON object per line. The
//! window sends [`Envelope<Request>`]; the worker answers with any number of
//! [`Envelope<Event>`] carrying the same `id`, ending with [`Event::Done`] or
//! [`Event::Error`]. Unsolicited events (status changes) use `id = 0`.

use crate::analyzer::MatchClass;
use crate::paths::Drive;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};

pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let safe: String = user.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\Sleuth-{safe}")
}

pub const PROTOCOL_VERSION: u32 = 1;

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
        #[serde(default)]
        deep: bool,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Cancel { search_id: u64 },
    /// Render one page as PNG with the matched boxes.
    Preview { path: String, page: u32, query: String, max_px: u32 },
    Status,
    Drives,
    GetSettings,
    SetSettings { settings: Box<Settings> },
    RescanNow,
    /// Read one file right away (used by the "Read now" action).
    ReadNow { path: String },
    /// Test a shared cache folder before saving it.
    CheckCache { dir: String },
    Shutdown,
}

fn default_limit() -> usize {
    2000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSource {
    FileName,
    TextLayer,
    Ocr,
    /// Found by name during a deep search, waiting to be read.
    Pending,
}

/// Rectangle in page coordinates normalised to 0..1, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
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
    Ok,
    Done { total: usize, millis: u64 },
    Error { message: String },
}
