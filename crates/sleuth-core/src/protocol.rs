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
use crate::kinds::{Category, FileKind};
use crate::paths::Drive;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};

/// The worker of this user (and, for development setups running several
/// workers side by side, of the instance named in `SLEUTH_INSTANCE`).
pub fn instance_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let name = match std::env::var("SLEUTH_INSTANCE") {
        Ok(i) if !i.trim().is_empty() => format!("{user}-{}", i.trim()),
        _ => user,
    };
    name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

pub fn pipe_name() -> String {
    format!(r"\\.\pipe\Sleuth-{}", instance_name())
}

/// 2: Sleuth 1.0 (projects, tags, revisions, corrections, office server).
/// 3: Sleuth 1.1 (office store, every file type, stage controls).
pub const PROTOCOL_VERSION: u32 = 3;

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
    /// Search the index (names and read documents). With `deep`, also walk
    /// `roots` live and read any unread documents found on the way.
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
    /// "Connect this PC": get this PC's own access key from the office
    /// server whose card is in the office index folder. Answers `Server`.
    JoinServer,
    /// Pause or resume one background stage on this PC. It stays paused
    /// across restarts until resumed; other PCs carry on.
    SetPaused { stage: Stage, paused: bool },
    /// Do a stage's work now: list every drive again (or only `root`), or
    /// start reading without waiting for the first listing to finish.
    RunNow {
        stage: Stage,
        #[serde(default)]
        root: Option<String>,
    },
    /// How hard this PC reads. `Full` for `minutes` (0 = until changed)
    /// ignores the busy-PC slowdown; `Gentle` never goes above the minimum.
    SetSpeed {
        mode: SpeedMode,
        #[serde(default)]
        minutes: u32,
    },
    /// The text read from a file that has no page image (spreadsheet, CAD
    /// model, email ...), one page (sheet, slide) at a time, matches marked.
    TextPreview {
        path: String,
        page: u32,
        query: String,
        #[serde(default)]
        tag: Option<String>,
    },
    Shutdown,
}

/// The two background stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Finding files: listing folders into the file-name catalog.
    Walk,
    /// Reading documents: text layers, extractors and OCR.
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpeedMode {
    /// As fast as the PC allows without getting in the user's way.
    #[default]
    Auto,
    /// The minimum only, whatever the PC is doing.
    Gentle,
    /// Every core, even while the PC is in use.
    Full,
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
    #[serde(default)]
    pub kind: FileKind,
    #[serde(default)]
    pub category: Category,
    /// The same document saved as other file types (the PDF, DWG and DGN of
    /// one drawing), shown on this row instead of as rows of their own.
    #[serde(default)]
    pub twins: Vec<Twin>,
}

/// Another file of the same document, and what makes it the same.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Twin {
    pub path: String,
    pub display_path: String,
    pub name: String,
    pub kind: FileKind,
    /// "same drawing number and revision in the title block", "same file
    /// name in the same folder" or "same file name in a folder beside it".
    pub why: String,
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
    /// Progress of the file-name index (the walk of every drive).
    #[serde(default)]
    pub index: IndexProgress,
    /// PDFs found outside the folders chosen for reading (names only).
    #[serde(default)]
    pub pdfs_unread: u64,
    /// Time left for the waiting documents at the whole office's pace
    /// (this PC's plus every PC seen working), in seconds; 0 = unknown.
    #[serde(default)]
    pub reading_eta_secs: u64,
    /// The office's pace that time is worked out from: documents an hour.
    #[serde(default)]
    pub office_docs_per_hour: u64,
    /// Plain-language reason for the reader's state, e.g. why it waits.
    #[serde(default)]
    pub reader_note: String,
    /// The reader runs with administrator rights (until it stops), so it can
    /// list and read folders that need them, on C:, D: and elsewhere.
    #[serde(default)]
    pub elevated: bool,
    /// Reading folders as chosen, each with its document counts.
    #[serde(default)]
    pub scopes: Vec<ScopeProgress>,
    /// Stages paused by the user on this PC.
    #[serde(default)]
    pub walk_paused: bool,
    #[serde(default)]
    pub read_paused: bool,
    #[serde(default)]
    pub speed: SpeedMode,
    /// Unix seconds a timed `Full` speed ends; 0 when not timed.
    #[serde(default)]
    pub speed_until: i64,
    /// Reader processes running now, per lane.
    #[serde(default)]
    pub fast_workers: u32,
    #[serde(default)]
    pub ocr_workers: u32,
    /// Documents read on this PC in the last hour.
    #[serde(default)]
    pub docs_per_hour: u64,
    /// Waiting documents already known to need OCR (the slow lane).
    #[serde(default)]
    pub ocr_waiting: u64,
    /// The office index in the shared folder.
    #[serde(default)]
    pub store: StoreStatus,
    /// Readable files in the reading folders by type.
    #[serde(default)]
    pub kinds: Vec<KindCount>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreStatus {
    /// "ok", "offline", "not set", "no access", "disabled".
    pub state: String,
    pub path: String,
    pub message: String,
    /// Search parts in use and their size on the share.
    pub parts: u32,
    pub bytes: u64,
    /// Documents readable through it (all PCs' readings).
    pub documents: u64,
    /// This PC's readings not yet published to the share.
    pub unpublished: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KindCount {
    pub kind: FileKind,
    pub found: u64,
    pub read: u64,
    pub waiting: u64,
    pub failed: u64,
}

/// One PC as the others see it (its heartbeat file in the office store).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PcStatus {
    pub name: String,
    pub user: String,
    pub version: String,
    /// "reading", "paused", "idle", "listing files" ...
    pub state: String,
    /// Shares this PC is listing right now.
    #[serde(default)]
    pub walking: Vec<String>,
    pub docs_per_hour: u64,
    pub pages_per_hour: u64,
    pub fast_workers: u32,
    pub ocr_workers: u32,
    /// What this PC can read beyond the built-in types: "office filter",
    /// "dgn filter", "dwg".
    #[serde(default)]
    pub abilities: Vec<String>,
    /// Unix seconds of its last heartbeat.
    pub last_seen: i64,
    #[serde(default)]
    pub server: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexProgress {
    /// "starting", "walking" (listing folders), "up to date".
    pub phase: String,
    /// Unix seconds the running walk started; 0 when none runs.
    pub pass_started: i64,
    /// Folders listed so far in the running walk.
    pub folders_listed: u64,
    /// Folders found but not listed yet in the running walk.
    pub folders_pending: u64,
    /// Files seen so far in the running walk.
    pub files_seen: u64,
    /// Unix seconds the last complete walk finished; 0 = never.
    pub last_complete: i64,
    /// Unix seconds the next scheduled walk starts; 0 = unknown.
    pub next_walk: i64,
    /// True until the first complete walk of this index finished.
    pub first_walk: bool,
    pub roots: Vec<IndexRoot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexRoot {
    /// Network path or drive root as walked.
    pub root: String,
    /// How the user sees it ("Z:\ (\\server\Project)").
    pub display: String,
    /// "waiting", "walking", "done", "partial", "offline".
    pub state: String,
    /// Folders and files listed under this root in the running (or last) walk.
    pub folders: u64,
    pub files: u64,
    /// Folders skipped (timeouts, no access) in the running walk.
    pub skipped: u64,
    pub note: String,
    /// Unix seconds this root last finished a complete walk; 0 = never.
    pub last_ok: i64,
    /// The PC listing it (or that listed it last); empty for this PC's own disks.
    #[serde(default)]
    pub by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeProgress {
    /// As written in settings ("Z:\").
    pub folder: String,
    /// Network path it resolves to, empty when the letter is not mapped.
    pub unc: String,
    pub pdfs: u64,
    pub read: u64,
    pub waiting: u64,
    pub failed: u64,
    /// "ok", "not mapped", "offline", "not indexed yet".
    pub note: String,
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
    /// The office server's connection card, when its setup left one in the
    /// index folder (`sleuth-server.json`): desktops fill the address from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office_server: Option<OfficeServerCard>,
}

/// Written by `sleuth-server setup` into the office index folder, next to the
/// CA certificate, so desktops find the server without anyone typing it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OfficeServerCard {
    /// `https://NAME:8455`
    pub address: String,
    /// Other addresses that reach the same server (by IP).
    pub addresses: Vec<String>,
    /// Computer that runs the server.
    pub pc: String,
    /// Unix seconds when it was written.
    pub at: i64,
    pub version: String,
    /// The server's certificate authority (PEM): a desktop that reads this
    /// card needs no certificate file. Public, not a secret.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca_pem: String,
    /// SHA-256 of `ca_pem`, shown so a person can compare it with the
    /// server's window.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca_sha256: String,
    /// Set when PCs may connect themselves: proves to the server that the
    /// PC asking can open the office index folder.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub join_code: String,
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageInfo {
    pub page: u32,
    /// PDF pages: searchable, scanned, hybrid, vector_outlined, sparse,
    /// damaged. Other files: sheet, slide, document, email, text, model, image.
    pub class: String,
    pub chars: u32,
    pub image_coverage: f32,
    pub ocr_tiles: u32,
    pub ocr_tiles_skipped: u32,
    pub ocr_millis: u64,
    /// For a sheet or slide, its name.
    pub note: String,
    /// Page size in points (1/72 inch); 0 when the file has no page size.
    #[serde(default)]
    pub width_pt: f32,
    #[serde(default)]
    pub height_pt: f32,
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
pub struct TextLine {
    pub text: String,
    /// The line matches the query (or carries the tag asked for).
    pub hit: bool,
    pub conf: f32,
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
    /// Every PC seen in the office store in the last day, this one first.
    #[serde(default)]
    pub pcs: Vec<PcStatus>,
    /// What this PC keeps on its own disk (settings, unpublished readings).
    #[serde(default)]
    pub local_bytes: u64,
    /// Documents the whole office still has to read, and how long that
    /// takes at the office's pace of the last hour (0 = unknown).
    #[serde(default)]
    pub office_waiting: u64,
    #[serde(default)]
    pub office_docs_per_hour: u64,
    #[serde(default)]
    pub office_eta_secs: u64,
    /// Pages the working PCs read in their last hour, added up.
    #[serde(default)]
    pub office_pages_per_hour: u64,
    /// PCs reading now (heartbeat in the last 20 minutes), this one included.
    #[serde(default)]
    pub office_readers: u32,
    /// PCs whose heartbeat counted towards the office's pace.
    #[serde(default)]
    pub office_pcs_counted: u32,
    /// Documents whose reading reached the office index in the last hour
    /// (by any PC or the server, whatever its version).
    #[serde(default)]
    pub office_arrived_last_hour: u64,
    /// How long the waiting documents would take at this PC's pace alone.
    #[serde(default)]
    pub this_eta_secs: u64,
    /// This PC's part of the office's pace, in percent.
    #[serde(default)]
    pub this_share_pct: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    Hello { protocol: u32, version: String },
    /// Rows found by file name alone, shown while the read documents are
    /// searched; the first `Hits` of the same search replaces them.
    Provisional { hits: Vec<Hit>, millis: u64 },
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
    /// One page (sheet, slide) of a file's text.
    TextPreview { kind: FileKind, pages: u32, page: u32, label: String, lines: Vec<TextLine>, truncated: bool, source: TextSource },
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
