//! The engine that `sleuth-worker.exe` (and the office server) runs:
//! background name indexing, the PDF reading queue, searches, deep searches,
//! page previews and the engineering workflows.

use crate::accel;
use crate::cache::{ClaimResult, SharedCache, check_cache, hash_file};
use crate::crawl::{Entry, WalkConfig, WalkEvent, list_with_timeout, norm_dir, walk_resumable};
use crate::derive::Deriver;
use crate::models;
use crate::ocr::{Backend, OcrOptions, OcrReader, config_signature};
use crate::reader::{ReadOptions, pipeline_fingerprint, read_pdf};
use crate::remote;
use crate::search::search_index;
use crate::store::{Scope, Store, state};
use crate::sys::{CpuMeter, idle_seconds, on_battery};
use crate::usn;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use sleuth_core::analyzer::{MatchClass, SearchQuery, Term, parse_search};
use sleuth_core::paths::{DriveKind, DriveMap};
use sleuth_core::protocol::*;
use sleuth_core::settings::{Settings, data_dir};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub type EventTx = tokio::sync::mpsc::UnboundedSender<Envelope<Event>>;

/// Unread PDFs a single deep search may queue for reading.
const DEEP_READ_LIMIT: usize = 400;
/// Most result rows kept for paging through one search.
const RESULT_CAP: usize = 5000;
/// Searches remembered with their results (invalidated by any index change).
const RESULT_MEMORY: usize = 12;
/// How often journaled volumes are checked for changes.
const JOURNAL_INTERVAL: Duration = Duration::from_secs(300);
/// How often the office server is asked how it is.
const OFFICE_INTERVAL: Duration = Duration::from_secs(60);

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn is_pdf(name: &str) -> bool {
    name.len() > 4 && name.get(name.len() - 4..).is_some_and(|ext| ext.eq_ignore_ascii_case(".pdf"))
}

fn under(path_lower: &str, scopes: &[String]) -> bool {
    scopes.iter().any(|s| {
        let s = s.trim_end_matches('\\').to_lowercase();
        path_lower == s || path_lower.starts_with(&format!("{s}\\"))
    })
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

struct Waiter {
    search_id: u64,
    query: SearchQuery,
    tx: EventTx,
}

#[derive(Default)]
struct Signal {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    fn raise(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }
    /// Wait until raised or `timeout`; clears the flag. True when raised.
    fn wait(&self, timeout: Duration) -> bool {
        let g = self.flag.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |raised| !*raised).unwrap();
        let raised = *g;
        *g = false;
        raised
    }
}

/// The office server as this PC sees it.
#[derive(Default)]
struct Office {
    client: Option<Arc<remote::Client>>,
    status: ServerStatus,
    /// Office document ids by lower-case path, learned from its results.
    docs: HashMap<String, i64>,
}

/// How a reading job ended.
enum Outcome {
    Read(u32),
    Reused,
    /// Another PC holds the claim; try again later.
    Waiting,
}

struct Reader {
    ocr: OcrReader,
    threads: usize,
    quality: String,
}

pub struct Engine {
    pub settings: RwLock<Settings>,
    db_path: PathBuf,
    pub drives: RwLock<DriveMap>,
    status: Mutex<Status>,
    reader_signal: Signal,
    indexer_signal: Signal,
    cache_signal: Signal,
    office_signal: Signal,
    cache: Mutex<CacheCheck>,
    waiting: Mutex<HashMap<String, Vec<Waiter>>>,
    searches: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    pages_read: Mutex<VecDeque<(Instant, u32)>>,
    pub shutdown: Arc<AtomicBool>,
    high_download_started: AtomicBool,
    /// Bumped on every committed index change; search memory checks it.
    generation: AtomicU64,
    results: Mutex<VecDeque<(String, u64, Arc<Vec<Hit>>)>>,
    latencies: Mutex<VecDeque<u64>>,
    searches_total: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    claims_waited: AtomicU64,
    deriver: RwLock<Arc<Deriver>>,
    backend_note: Mutex<String>,
    journal_note: Mutex<String>,
    office: Mutex<Office>,
    /// Forces a full walk on the next indexer pass.
    full_walk: AtomicBool,
    /// Runs as the office server: no office client, roots from settings.
    pub server_mode: bool,
}

impl Engine {
    pub fn new(settings: Settings) -> Arc<Engine> {
        Self::build(settings, false)
    }

    /// The engine inside `sleuth-server.exe`.
    pub fn new_server(settings: Settings) -> Arc<Engine> {
        Self::build(settings, true)
    }

    fn build(settings: Settings, server_mode: bool) -> Arc<Engine> {
        let deriver = Arc::new(Deriver::new(&settings));
        Arc::new(Engine {
            settings: RwLock::new(settings),
            db_path: data_dir().join("index.db"),
            drives: RwLock::new(DriveMap::current()),
            status: Mutex::new(Status { worker_version: env!("CARGO_PKG_VERSION").into(), reader_state: "starting".into(), ocr_backend: "CPU".into(), ..Default::default() }),
            reader_signal: Signal::default(),
            indexer_signal: Signal::default(),
            cache_signal: Signal::default(),
            office_signal: Signal::default(),
            cache: Mutex::new(CacheCheck::default()),
            waiting: Mutex::new(HashMap::new()),
            searches: Mutex::new(HashMap::new()),
            pages_read: Mutex::new(VecDeque::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            high_download_started: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            results: Mutex::new(VecDeque::new()),
            latencies: Mutex::new(VecDeque::new()),
            searches_total: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            claims_waited: AtomicU64::new(0),
            deriver: RwLock::new(deriver),
            backend_note: Mutex::new(String::new()),
            journal_note: Mutex::new(String::new()),
            office: Mutex::new(Office::default()),
            full_walk: AtomicBool::new(true),
            server_mode,
        })
    }

    pub fn open_store(&self) -> Result<Store> {
        match Store::open(&self.db_path) {
            Ok(s) => Ok(s),
            Err(e) => {
                // Damaged or newer-format index: move it aside and start fresh.
                warn!("index unusable ({e:#}); rebuilding");
                let aside = self.db_path.with_extension(format!("broken-{}.db", now()));
                let _ = std::fs::rename(&self.db_path, aside);
                for ext in ["db-wal", "db-shm"] {
                    let _ = std::fs::remove_file(self.db_path.with_extension(ext));
                }
                Store::open(&self.db_path)
            }
        }
    }

    fn deriver(&self) -> Arc<Deriver> {
        Arc::clone(&self.deriver.read().unwrap())
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn start(self: &Arc<Self>) -> Result<()> {
        let store = self.open_store().context("open index")?;
        store.set_projects(&self.settings.read().unwrap().projects)?;
        for e in &self.deriver().errors {
            store.event(now(), "warn", &format!("rule not used: {e}"));
        }
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-indexer".into()).spawn(move || e.indexer_loop())?;
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-reader".into()).spawn(move || e.reader_loop())?;
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-cache".into()).spawn(move || e.cache_loop())?;
        if !self.server_mode {
            let e = Arc::clone(self);
            std::thread::Builder::new().name("sleuth-office".into()).spawn(move || e.office_loop())?;
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.reader_signal.raise();
        self.indexer_signal.raise();
        self.cache_signal.raise();
        self.office_signal.raise();
        for flag in self.searches.lock().unwrap().values() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn rescan_now(&self) {
        self.full_walk.store(true, Ordering::SeqCst);
        self.indexer_signal.raise();
    }

    pub fn set_settings(&self, s: Settings) -> Result<()> {
        s.save()?;
        let old = self.settings.read().unwrap().clone();
        *self.settings.write().unwrap() = s.clone();
        self.cache_signal.raise();
        let store = self.open_store()?;
        if old.projects != s.projects {
            store.set_projects(&s.projects)?;
        }
        if old.tag_rules != s.tag_rules || old.field_rules != s.field_rules || old.discipline_prefixes != s.discipline_prefixes {
            let d = Arc::new(Deriver::new(&s));
            for e in &d.errors {
                store.event(now(), "warn", &format!("rule not used: {e}"));
            }
            *self.deriver.write().unwrap() = d;
            store.event(now(), "info", "tag or title-block rules changed; read drawings are being parsed again (no OCR)");
            self.reader_signal.raise();
        }
        if old.content_scopes != s.content_scopes {
            store.apply_scopes(&s.content_scopes, &self.office_roots())?;
            self.reader_signal.raise();
        }
        if old.server != s.server {
            self.office_signal.raise();
        }
        self.changed();
        Ok(())
    }

    fn set_reader_state(&self, s: &str, file: Option<String>) {
        let mut st = self.status.lock().unwrap();
        st.reader_state = s.into();
        st.current_file = file;
    }

    pub fn status(&self) -> Result<(Status, Vec<Failed>)> {
        let store = self.open_store()?;
        let (all, read, waiting, failed) = store.counts()?;
        let mut st = self.status.lock().unwrap().clone();
        st.files_indexed = all;
        st.pdfs_read = read;
        st.pdfs_waiting = waiting;
        st.pdfs_failed = failed;
        let mut log = self.pages_read.lock().unwrap();
        while log.front().is_some_and(|(t, _)| t.elapsed() > Duration::from_secs(3600)) {
            log.pop_front();
        }
        st.pages_per_hour = log.iter().map(|(_, n)| *n as u64).sum();
        st.cache = self.cache.lock().unwrap().clone();
        st.server = self.office.lock().unwrap().status.clone();
        let fails = store.failures(50)?.into_iter().map(|(path, reason)| Failed { path, reason }).collect();
        Ok((st, fails))
    }

    /// Re-check the shared cache every 2 minutes, and at once after settings change.
    fn cache_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            let s = self.settings.read().unwrap().clone();
            let drives = DriveMap::current();
            let c = check_cache(s.shared_cache_dir.as_deref(), s.shared_cache_enabled, &drives);
            if c.state != CacheState::Ok {
                info!("shared cache {:?}: {}", c.state, c.message);
            }
            *self.cache.lock().unwrap() = c;
            self.cache_signal.wait(Duration::from_secs(120));
        }
    }

    /// Test a folder the user is about to choose.
    pub fn check_cache_dir(&self, dir: &str) -> CacheCheck {
        check_cache(Some(dir), true, &DriveMap::current())
    }

    /// Folders to walk: every fixed and network drive minus excluded ones,
    /// or the configured folders on the office server.
    fn walk_roots(&self) -> Vec<String> {
        let s = self.settings.read().unwrap();
        if let Some(r) = &s.index_roots {
            return r.clone();
        }
        let drives = self.drives.read().unwrap();
        let excluded: Vec<String> = s.excluded.iter().map(|x| x.trim_end_matches('\\').to_lowercase()).collect();
        drives
            .drives
            .iter()
            .filter(|d| matches!(d.kind, DriveKind::Fixed | DriveKind::Network))
            .filter(|d| {
                let letter = d.letter.to_lowercase();
                let unc = d.unc.as_deref().unwrap_or("").trim_end_matches('\\').to_lowercase();
                !excluded.iter().any(|x| *x == letter || (!unc.is_empty() && *x == unc))
            })
            .map(|d| d.root())
            .collect()
    }

    /// Folders whose PDFs this engine reads.
    fn read_scopes(&self) -> Vec<String> {
        let s = self.settings.read().unwrap();
        match &s.index_roots {
            Some(r) => r.clone(),
            None => s.content_scopes.clone(),
        }
    }

    fn office_roots(&self) -> Vec<String> {
        let o = self.office.lock().unwrap();
        if o.status.state == ServerState::NotConfigured { Vec::new() } else { o.status.roots.clone() }
    }

    /// Record one folder listing. Returns how many PDFs were queued.
    fn ingest(&self, store: &Store, dir: &str, entries: &[Entry], pass: i64, scopes: &[String], office: &[String]) -> Result<usize> {
        store.conn.execute_batch("BEGIN")?;
        let r = (|| -> Result<usize> {
            let parent = Path::new(dir).parent().map(|p| norm_dir(&p.to_string_lossy()));
            store.touch_dir(dir, parent.as_deref().filter(|p| *p != dir), pass)?;
            let mut queued = 0;
            for e in entries {
                let path = join(dir, &e.name);
                if e.is_dir {
                    store.touch_dir(&path, Some(dir), pass)?;
                    continue;
                }
                let wanted = if !is_pdf(&e.name) {
                    state::NOT_PDF
                } else {
                    let lower = path.to_lowercase();
                    if under(&lower, office) {
                        state::OFFICE
                    } else if under(&lower, scopes) {
                        state::WAITING
                    } else {
                        state::UNREAD
                    }
                };
                let (_, changed) = store.upsert_file(&path, dir, &e.name, e.size, e.mtime, pass, wanted)?;
                if changed && wanted == state::WAITING {
                    queued += 1;
                }
            }
            store.reconcile_dir(dir, pass, now())?;
            Ok(queued)
        })();
        match r {
            Ok(q) => {
                store.conn.execute_batch("COMMIT")?;
                Ok(q)
            }
            Err(e) => {
                let _ = store.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ---- background name index -------------------------------------------

    fn indexer_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        let mut last_full: Option<Instant> = None;
        while !self.shutdown.load(Ordering::SeqCst) {
            *self.drives.write().unwrap() = DriveMap::current();
            let mins = self.settings.read().unwrap().name_rescan_minutes.max(15) as u64;
            let due = self.full_walk.swap(false, Ordering::SeqCst) || last_full.is_none_or(|t| t.elapsed() >= Duration::from_secs(mins * 60));
            // A bug in one pass must not stop indexing for the rest of the session.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.index_pass(due)));
            match r {
                Ok(Ok(full)) => {
                    if full {
                        last_full = Some(Instant::now());
                    }
                }
                Ok(Err(e)) => warn!("name index pass failed: {e:#}"),
                Err(_) => warn!("name index pass crashed; retrying next pass"),
            }
            let journaled = self.journal_note.lock().unwrap().starts_with("in use");
            let wait = if journaled { JOURNAL_INTERVAL.min(Duration::from_secs(mins * 60)) } else { Duration::from_secs(mins * 60) };
            if self.indexer_signal.wait(wait) {
                self.full_walk.store(true, Ordering::SeqCst);
            }
        }
    }

    /// One pass: journaled volumes by their changes, the rest walked in full
    /// when `due`. Returns whether a full walk ran.
    fn index_pass(&self, due: bool) -> Result<bool> {
        let s = self.settings.read().unwrap().clone();
        let roots = self.walk_roots();
        let store = self.open_store()?;
        let scopes = self.read_scopes();
        let office = self.office_roots();
        let drives = self.drives.read().unwrap().clone();
        let mut full: Vec<String> = Vec::new();
        let mut restart_points: Vec<(String, String, i64)> = Vec::new();
        let mut journal_used = Vec::new();
        let mut journal_off = Vec::new();
        for r in &roots {
            let local_fixed = drives.drives.iter().any(|d| d.kind == DriveKind::Fixed && d.root().eq_ignore_ascii_case(r));
            if !local_fixed {
                full.push(r.clone());
                continue;
            }
            let letter = r.trim_end_matches('\\').to_string();
            match usn::changes(&letter, store.checkpoint(&letter)?) {
                usn::Outcome::Unavailable(why) => {
                    journal_off.push(format!("{letter} {why}"));
                    full.push(r.clone());
                }
                usn::Outcome::Restart { journal_id, next_usn, why } => {
                    store.event(now(), "info", &format!("{letter}: full walk ({why})"));
                    restart_points.push((letter, journal_id, next_usn));
                    full.push(r.clone());
                }
                usn::Outcome::Changes { shallow, deep, journal_id, next_usn } => {
                    journal_used.push(letter.clone());
                    let n = shallow.len() + deep.len();
                    self.apply_journal(&store, &shallow, &deep, &scopes, &office, &s)?;
                    store.set_checkpoint(&letter, &journal_id, next_usn, now())?;
                    if n > 0 {
                        info!("{letter}: {n} changed folder(s) from the change journal");
                    }
                }
            }
        }
        *self.journal_note.lock().unwrap() = if !journal_used.is_empty() {
            format!("in use on {}", journal_used.join(", "))
        } else if !journal_off.is_empty() {
            format!("not used: {}; local disks are walked instead", journal_off.join("; "))
        } else {
            "no local NTFS disks indexed".into()
        };
        // Journaled volumes needing a restart are walked even when not due:
        // their checkpoint is only valid after a complete walk.
        let restart_roots: Vec<String> = restart_points.iter().map(|(l, _, _)| format!("{l}\\")).collect();
        let walk_now: Vec<String> = if due { full } else { full.into_iter().filter(|r| restart_roots.iter().any(|x| x.eq_ignore_ascii_case(r))).collect() };
        if walk_now.is_empty() {
            return Ok(false);
        }
        info!("name index: walking {} roots", walk_now.len());
        let completed = self.walk_full(&store, &walk_now, &scopes, &office, &s)?;
        for (letter, id, usn) in restart_points {
            if completed.iter().any(|r| r.eq_ignore_ascii_case(&format!("{letter}\\"))) {
                store.set_checkpoint(&letter, &id, usn, now())?;
            }
        }
        let purged = store.purge_gone(now())?;
        if purged > 0 {
            info!("forgot {purged} removed files after the grace period");
            self.changed();
        }
        Ok(due)
    }

    /// List changed folders again; walk folders created or moved in.
    fn apply_journal(&self, store: &Store, shallow: &[String], deep: &[String], scopes: &[String], office: &[String], s: &Settings) -> Result<()> {
        let pass = now();
        let excluded: Vec<String> = s.excluded.iter().map(|x| x.to_lowercase()).collect();
        for dir in shallow {
            let d = norm_dir(dir);
            if excluded.iter().any(|x| d.to_lowercase().starts_with(x.as_str())) {
                continue;
            }
            match list_with_timeout(&d, Duration::from_secs(10)) {
                Ok(entries) => {
                    self.ingest(store, &d, &entries, pass, scopes, office)?;
                }
                // Gone since: its parent's listing (also journaled) reconciles it.
                Err(e) => info!("journal: {d} not listed: {e}"),
            }
        }
        if !deep.is_empty() {
            self.walk_full(store, deep, scopes, office, s)?;
        }
        if !shallow.is_empty() || !deep.is_empty() {
            self.changed();
            self.reader_signal.raise();
        }
        Ok(())
    }

    /// Walk roots completely (resuming an interrupted pass). Returns the
    /// roots that finished.
    fn walk_full(&self, store: &Store, roots: &[String], scopes: &[String], office: &[String], s: &Settings) -> Result<Vec<String>> {
        // Resume a pass interrupted less than a day ago.
        let pass = match store.get_meta("pass")?.and_then(|v| v.split_once('|').map(|(a, b)| (a.parse::<i64>().ok(), b == "done"))) {
            Some((Some(p), false)) if now() - p < 86_400 => {
                info!("resuming the walk started {} s ago", now() - p);
                p
            }
            _ => now(),
        };
        store.set_meta("pass", &format!("{pass}|running"))?;
        let cfg = WalkConfig {
            per_server: 2,
            local_threads: 2,
            listing_timeout: Duration::from_secs(s.listing_timeout_secs.max(5) as u64),
            excluded: s.excluded.iter().map(|x| x.to_lowercase()).collect(),
            background: true,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<WalkEvent>(256);
        let resume_store = Mutex::new(self.open_store()?);
        let done = |d: &str| resume_store.lock().unwrap().listed_in_pass(d, pass).ok().flatten();
        let completed = Mutex::new(Vec::new());
        // The listing thread writes through its own connection.
        let sink_store = Mutex::new(self.open_store()?);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                let store = &*sink_store.lock().unwrap();
                let mut since_wake = 0usize;
                let mut since_bump = Instant::now();
                for ev in rx {
                    match ev {
                        WalkEvent::Listed { dir, entries, .. } => match self.ingest(store, &dir, &entries, pass, scopes, office) {
                            Ok(q) => since_wake += q,
                            Err(e) => warn!("index {dir}: {e:#}"),
                        },
                        WalkEvent::Skipped { root, dir, reason } => {
                            info!("skipped {dir}: {reason}");
                            let _ = store.set_root_scan(&root, "partial", None, &format!("{dir}: {reason}"));
                        }
                        WalkEvent::RootOffline { root, reason } => {
                            info!("offline {root}: {reason}");
                            let _ = store.set_root_scan(&root, "offline", None, &reason);
                            store.event(now(), "warn", &format!("{root} offline: {reason}"));
                        }
                        WalkEvent::RootDone { root } => {
                            let _ = store.set_root_scan(&root, "ok", Some(now()), "");
                            completed.lock().unwrap().push(root);
                        }
                    }
                    if since_wake >= 20 {
                        self.reader_signal.raise();
                        since_wake = 0;
                    }
                    if since_bump.elapsed() > Duration::from_secs(2) {
                        self.changed();
                        since_bump = Instant::now();
                    }
                }
                if since_wake > 0 {
                    self.reader_signal.raise();
                }
                self.changed();
            });
            walk_resumable(roots, &cfg, &self.shutdown, &done, &|ev| {
                let _ = tx.send(ev);
            });
            drop(tx);
        });
        if !self.shutdown.load(Ordering::SeqCst) {
            store.set_meta("pass", &format!("{pass}|done"))?;
        }
        Ok(completed.into_inner().unwrap())
    }

    // ---- office server ----------------------------------------------------

    fn office_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            self.refresh_office();
            self.office_signal.wait(OFFICE_INTERVAL);
        }
    }

    fn office_client(&self, url: &str, token: &str, ca_file: &str) -> Result<remote::Client> {
        let s = self.settings.read().unwrap().clone();
        let drives = self.drives.read().unwrap().clone();
        let ca = remote::ca_path(ca_file, s.shared_cache_dir.as_deref(), &drives).ok_or_else(|| anyhow!("no server certificate file: set one, or put {} in the shared cache folder", remote::CA_FILE_NAME))?;
        let pem = std::fs::read(&ca).map_err(|e| anyhow!("cannot read the server certificate {}: {e}", ca.display()))?;
        remote::Client::new(url, token, &pem)
    }

    fn refresh_office(&self) {
        let s = self.settings.read().unwrap().server.clone();
        if s.url.trim().is_empty() {
            let mut o = self.office.lock().unwrap();
            if o.status.state != ServerState::NotConfigured {
                *o = Office::default();
                drop(o);
                self.reapply_scopes();
            }
            return;
        }
        let client = remote::unprotect(&s.token_protected).and_then(|t| self.office_client(&s.url, &t, &s.ca_file));
        let (client, result) = match client {
            Ok(c) => {
                let c = Arc::new(c);
                let r = c.capabilities();
                (Some(c), r)
            }
            Err(e) => (None, Err(e)),
        };
        let mut o = self.office.lock().unwrap();
        let before = o.status.roots.clone();
        o.client = client;
        match result {
            Ok(c) => {
                o.status = ServerStatus {
                    state: if c.syncing { ServerState::Syncing } else { ServerState::Connected },
                    message: format!("signed in as {} ({})", c.user, c.role),
                    version: c.server_version,
                    roots: c.roots,
                    projects: c.projects,
                    last_ok: now(),
                };
            }
            Err(e) => {
                let m = format!("{e:#}");
                let refused = ["certificate", "access key", "API version", "not allowed", "401", "403", "https://", "PEM"].iter().any(|k| m.contains(k));
                o.status.state = if refused { ServerState::Degraded } else { ServerState::Offline };
                o.status.message = m;
            }
        }
        let after = o.status.roots.clone();
        drop(o);
        if before != after {
            self.reapply_scopes();
        }
    }

    fn reapply_scopes(&self) {
        let scopes = self.read_scopes();
        let office = self.office_roots();
        if let Ok(store) = self.open_store()
            && store.apply_scopes(&scopes, &office).is_ok()
        {
            self.changed();
            self.reader_signal.raise();
        }
    }

    /// Test a server address and key; `token` empty means the saved key.
    pub fn check_server(&self, url: &str, token: &str, ca_file: &str) -> ServerStatus {
        let token = if token.trim().is_empty() { remote::unprotect(&self.settings.read().unwrap().server.token_protected).unwrap_or_default() } else { token.to_string() };
        let r = self.office_client(url, &token, ca_file).and_then(|c| c.capabilities());
        match r {
            Ok(c) => ServerStatus {
                state: if c.syncing { ServerState::Syncing } else { ServerState::Connected },
                message: format!("signed in as {} ({}); {} folder(s), {} project(s)", c.user, c.role, c.roots.len(), c.projects.len()),
                version: c.server_version,
                roots: c.roots,
                projects: c.projects,
                last_ok: now(),
            },
            Err(e) => ServerStatus { state: ServerState::Degraded, message: format!("{e:#}"), ..Default::default() },
        }
    }

    pub fn set_server(&self, url: &str, token: &str, ca_file: &str) -> Result<()> {
        let mut s = self.settings.read().unwrap().clone();
        if url.trim().is_empty() {
            s.server = Default::default();
        } else {
            s.server.url = url.trim().to_string();
            s.server.ca_file = ca_file.trim().to_string();
            if !token.trim().is_empty() {
                s.server.token_protected = remote::protect(token.trim())?;
            }
        }
        self.set_settings(s)
    }

    fn office(&self) -> Result<Arc<remote::Client>> {
        let o = self.office.lock().unwrap();
        match (&o.client, o.status.state) {
            (Some(c), ServerState::Connected | ServerState::Syncing) => Ok(Arc::clone(c)),
            (_, ServerState::NotConfigured) => bail!("no office server is set up"),
            _ => bail!("the office server is not available: {}", o.status.message),
        }
    }

    fn office_doc(&self, c: &remote::Client, path: &str) -> Result<i64> {
        if let Some(id) = self.office.lock().unwrap().docs.get(&path.to_lowercase()) {
            return Ok(*id);
        }
        let d = c.lookup(path)?;
        self.office.lock().unwrap().docs.insert(path.to_lowercase(), d.id);
        Ok(d.id)
    }

    // ---- PDF reading queue ------------------------------------------------

    fn reader_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        let mut store = match self.open_store() {
            Ok(s) => s,
            Err(e) => {
                warn!("reader cannot open index: {e:#}");
                return;
            }
        };
        let mut ocr: Option<Reader> = None;
        let mut last_used = Instant::now();
        let mut meter = CpuMeter::default();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        meter.others_percent();
        let owner = format!("{}:{}", std::env::var("COMPUTERNAME").unwrap_or_default(), std::process::id());
        while !self.shutdown.load(Ordering::SeqCst) {
            let s = self.settings.read().unwrap().clone();
            if !self.server_mode && on_battery() {
                self.set_reader_state("paused: on battery", None);
                self.reader_signal.wait(Duration::from_secs(60));
                continue;
            }
            let idle = if self.server_mode { u64::MAX } else { idle_seconds() };
            let others = meter.others_percent();
            if !self.server_mode && others > s.busy_cpu_percent as f32 && idle < 120 {
                self.set_reader_state("paused: PC busy", None);
                self.reader_signal.wait(Duration::from_secs(5));
                continue;
            }
            let job = match store.claim_next(now(), &owner) {
                Ok(Some(j)) => j,
                Ok(None) => {
                    // Idle: parse read drawings again if the rules changed (no OCR).
                    if self.rederive_some(&mut store) {
                        continue;
                    }
                    self.set_reader_state("idle", None);
                    if ocr.is_some() && last_used.elapsed() > Duration::from_secs(120) {
                        ocr = None; // free the model memory
                        self.status.lock().unwrap().ocr_model.clear();
                    }
                    self.reader_signal.wait(Duration::from_secs(30));
                    continue;
                }
                Err(e) => {
                    warn!("reader queue: {e:#}");
                    self.reader_signal.wait(Duration::from_secs(10));
                    continue;
                }
            };
            // Use most of the CPU when nobody is at the PC, half otherwise.
            let threads = if idle > 300 { cores.saturating_sub(1).max(1) } else { (cores / 2).max(1) };
            self.set_reader_state("reading", Some(job.path.clone()));
            let result = self.read_one(&mut store, &job.path, job.id, &s, &mut ocr, threads);
            last_used = Instant::now();
            match result {
                Ok(Outcome::Read(pages)) => {
                    self.pages_read.lock().unwrap().push_back((Instant::now(), pages));
                    self.changed();
                    self.notify_waiters(&store, &job.path);
                }
                Ok(Outcome::Reused) => {
                    self.changed();
                    self.notify_waiters(&store, &job.path);
                }
                Ok(Outcome::Waiting) => {}
                Err(e) if self.shutdown.load(Ordering::SeqCst) => {
                    let _ = store.release(job.id);
                    info!("stopped while reading {}: {e:#}", job.path);
                }
                Err(e) if is_transient(&e) => {
                    let reason = friendly(&e);
                    match store.defer(job.id, &reason, now(), 5) {
                        Ok(true) => info!("will retry {} later: {e:#}", job.path),
                        _ => {
                            warn!("giving up on {}: {e:#}", job.path);
                            self.notify_waiters(&store, &job.path);
                        }
                    }
                }
                Err(e) => {
                    let reason = friendly(&e);
                    warn!("could not read {}: {e:#}", job.path);
                    let _ = store.save_failure(job.id, &reason, now());
                    store.event(now(), "warn", &format!("could not read {}: {reason}", job.path));
                    self.notify_waiters(&store, &job.path);
                }
            }
        }
    }

    /// Re-derive a batch of files made with other rules. True if any were done.
    fn rederive_some(&self, store: &mut Store) -> bool {
        let d = self.deriver();
        let ids = store.stale_derived(&d.hash, 50).unwrap_or_default();
        if ids.is_empty() {
            return false;
        }
        self.set_reader_state("parsing tags", None);
        for id in ids {
            if let Err(e) = store.rederive(id, &d) {
                warn!("parse tags of file {id}: {e:#}");
                // Mark it so a bad file does not stop the others.
                let _ = store.conn.execute("UPDATE files SET derived=?2 WHERE id=?1", rusqlite::params![id, d.hash]);
            }
        }
        self.changed();
        true
    }

    /// Settings of the OCR pipeline, and its fingerprint.
    fn pipeline(&self, s: &Settings, threads: usize) -> Result<(models::ModelSet, bool, OcrOptions, String)> {
        let (set, fetch_high) = models::choose(s.ocr_quality)?;
        let opts = OcrOptions { threads, ..Default::default() };
        let d = self.deriver();
        let title = if s.ocr_missing_title_block { d.fields_hash.clone() } else { "off".into() };
        let fp = pipeline_fingerprint(&set.name, &set.hash, &config_signature(&opts), s.ocr_dpi, &title);
        Ok((set, fetch_high, opts, fp))
    }

    fn read_one(&self, store: &mut Store, path: &str, id: i64, s: &Settings, ocr: &mut Option<Reader>, threads: usize) -> Result<Outcome> {
        let hash = hash_file(Path::new(path)).map_err(|e| anyhow!("cannot open file: {e}"))?;
        let (set, fetch_high, mut opts, fp) = self.pipeline(s, threads)?;
        let deriver = self.deriver();
        let prov = |obtained: &str, model: &str, backend: &str| Provenance {
            pipeline: fp.clone(),
            reader_version: crate::text::READER_VERSION,
            model: model.into(),
            model_hash: if model.is_empty() { String::new() } else { set.hash.clone() },
            backend: backend.into(),
            dpi: s.ocr_dpi,
            read_by: std::env::var("COMPUTERNAME").unwrap_or_default(),
            read_at: now(),
            obtained: obtained.into(),
        };
        // Same content already read here (copied, moved or renamed file).
        if let Some(other) = store.read_copy_by_hash(&hash, id)? {
            let mut text = store.file_text(other)?;
            text.provenance.obtained = "copy of an identical file".into();
            store.save_read(id, &hash, &text, now(), &deriver)?;
            return Ok(Outcome::Reused);
        }
        let shared = {
            let c = self.cache.lock().unwrap();
            (c.state == CacheState::Ok).then(|| SharedCache::new(c.path.clone()))
        };
        if let Some(c) = &shared
            && let Some(mut text) = c.get(&hash, &fp)
        {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            text.provenance.obtained = "shared cache".into();
            store.save_read(id, &hash, &text, now(), &deriver)?;
            return Ok(Outcome::Reused);
        }
        let claim = match shared.as_ref().map(|c| c.claim(&hash, &fp)) {
            Some(ClaimResult::HeldBy { host, until }) => {
                self.claims_waited.fetch_add(1, Ordering::Relaxed);
                let until = until.min(now() + 900).max(now() + 60);
                store.wait_until(id, until, &format!("being read by {host}; its result will be shared"))?;
                return Ok(Outcome::Waiting);
            }
            Some(ClaimResult::Mine(c)) => Some(c),
            _ => None,
        };
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        let quality = format!("{:?}/{:?}", s.ocr_quality, s.ocr_backend);
        if ocr.as_ref().is_some_and(|r| r.threads != threads || r.quality != quality) {
            *ocr = None;
        }
        for attempt in 0..2 {
            if ocr.is_none() {
                self.set_reader_state("loading OCR models", Some(path.to_string()));
                if fetch_high && !self.high_download_started.swap(true, Ordering::SeqCst) {
                    std::thread::spawn(|| match models::download_high() {
                        Ok(()) => info!("high-accuracy OCR models downloaded"),
                        Err(e) => warn!("could not download high-accuracy models: {e:#}"),
                    });
                }
                let choice = if attempt == 0 { accel::choose(s.ocr_backend, &set, threads) } else { accel::Choice { backend: Backend::Cpu, label: "CPU".into(), note: "CPU after a DirectML failure".into() } };
                opts.backend = choice.backend;
                let built = OcrReader::build(&set, &opts).or_else(|e| {
                    if opts.backend == Backend::Cpu {
                        return Err(e);
                    }
                    accel::record_failure(&format!("{e:#}"));
                    opts.backend = Backend::Cpu;
                    OcrReader::build(&set, &opts)
                })?;
                {
                    let mut st = self.status.lock().unwrap();
                    st.ocr_model = built.model.clone();
                    st.ocr_backend = if opts.backend == Backend::Cpu { "CPU".into() } else { choice.label.clone() };
                }
                *self.backend_note.lock().unwrap() = choice.note;
                *ocr = Some(Reader { ocr: built, threads, quality: quality.clone() });
                self.set_reader_state("reading", Some(path.to_string()));
            }
            let reader = &ocr.as_ref().unwrap().ocr;
            let stop = &self.shutdown;
            let renewed = std::cell::Cell::new(Instant::now());
            let cancel = || {
                if renewed.get().elapsed() > Duration::from_secs(60) {
                    renewed.set(Instant::now());
                    if let Some(c) = &claim {
                        c.renew();
                    }
                    let _ = store.renew_lease(id, now());
                }
                stop.load(Ordering::SeqCst)
            };
            let has_dwg = |lines: &[crate::text::Line]| deriver.has_drawing_number(lines);
            let opts_read = ReadOptions { dpi: s.ocr_dpi, cancel: &cancel, title_block_ocr: s.ocr_missing_title_block, has_drawing_number: &has_dwg };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_pdf(Path::new(path), Some(reader), &opts_read)))
                .map_err(|_| anyhow!("the reader crashed on this file"))
                .and_then(|r| r);
            match r {
                Ok(mut text) => {
                    let pages = text.pages.len() as u32;
                    let backend = reader.opts.backend.label();
                    text.provenance = prov("read here", &text.model.clone(), if text.model.is_empty() { "" } else { backend });
                    store.save_read(id, &hash, &text, now(), &deriver)?;
                    if let Some(c) = &shared
                        && let Err(e) = c.put(&hash, &fp, &text)
                    {
                        warn!("shared cache write failed: {e:#}");
                    }
                    return Ok(Outcome::Read(pages));
                }
                // The graphics card failed: never use it again here, and read
                // this file again on the CPU.
                Err(e) if attempt == 0 && reader.opts.backend == Backend::DirectMl && !self.shutdown.load(Ordering::SeqCst) && format!("{e:#}").contains("OCR failed") => {
                    accel::record_failure(&format!("{e:#}"));
                    store.event(now(), "warn", &format!("DirectML failed reading {path}; switched to the CPU"));
                    *ocr = None;
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow!("could not read with OCR"))
    }

    fn notify_waiters(&self, store: &Store, path: &str) {
        let Some(waiters) = self.waiting.lock().unwrap().remove(&path.to_lowercase()) else { return };
        let drives = self.drives.read().unwrap().clone();
        for w in waiters {
            let hits: Vec<Hit> = search_index(store, &w.query, &[path.to_string()], 5, &drives)
                .unwrap_or_default()
                .into_iter()
                .filter(|h| h.path.eq_ignore_ascii_case(path))
                .collect();
            let _ = w.tx.send(Envelope { id: w.search_id, body: Event::Resolved { path: path.to_string(), hits } });
        }
    }

    // ---- searching ---------------------------------------------------------

    pub fn cancel_search(&self, id: u64) {
        if let Some(flag) = self.searches.lock().unwrap().remove(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        let mut w = self.waiting.lock().unwrap();
        for list in w.values_mut() {
            list.retain(|x| x.search_id != id);
        }
        w.retain(|_, v| !v.is_empty());
    }

    /// Folders of a project by name.
    pub fn project_roots(&self, name: &str) -> Result<Vec<String>> {
        let s = self.settings.read().unwrap();
        let p = s.projects.iter().find(|p| p.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| anyhow!("no project called {name}"))?;
        Ok(p.roots.clone())
    }

    /// Ranked hits from this index for `query` inside `roots` (empty =
    /// everywhere), remembered until the index changes.
    pub fn search_hits(&self, query: &str, roots: &[String]) -> Result<Arc<Vec<Hit>>> {
        let generation = self.generation.load(Ordering::SeqCst);
        let key = format!("{query}\u{1}{}", roots.join("\u{2}"));
        if let Some((_, _, hits)) = self.results.lock().unwrap().iter().find(|(k, g, _)| *k == key && *g == generation) {
            return Ok(Arc::clone(hits));
        }
        let q = parse_search(query);
        let drives = self.drives.read().unwrap().clone();
        let store = self.open_store()?;
        let hits = Arc::new(search_index(&store, &q, roots, RESULT_CAP, &drives)?);
        let mut mem = self.results.lock().unwrap();
        mem.retain(|(k, g, _)| *g == generation && *k != key);
        mem.push_back((key, generation, Arc::clone(&hits)));
        while mem.len() > RESULT_MEMORY {
            mem.pop_front();
        }
        Ok(hits)
    }

    fn record_latency(&self, ms: u64) {
        self.searches_total.fetch_add(1, Ordering::Relaxed);
        let mut l = self.latencies.lock().unwrap();
        l.push_back(ms);
        while l.len() > 500 {
            l.pop_front();
        }
    }

    /// Runs on a blocking thread; streams events to `tx`.
    #[allow(clippy::too_many_arguments)]
    pub fn search(&self, id: u64, query: &str, roots: &[String], project: Option<&str>, deep: bool, limit: usize, offset: usize, tx: &EventTx) -> Result<()> {
        let started = Instant::now();
        let q = parse_search(query);
        if q.is_empty() {
            let _ = tx.send(Envelope { id, body: Event::Done { total: 0, millis: 0, more: false } });
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.searches.lock().unwrap().insert(id, Arc::clone(&cancel));
        let roots: Vec<String> = match project.filter(|p| !p.trim().is_empty()) {
            Some(p) => self.project_roots(p)?,
            None => roots.to_vec(),
        };
        if offset == 0 {
            let _ = self.open_store().and_then(|s| s.add_history(query, now()));
        }
        // The office server's part runs alongside.
        let office = if self.office_roots_usable() { self.office.lock().unwrap().client.clone() } else { None };
        let office_job = office.map(|c| {
            let (tx, query, project) = (tx.clone(), query.to_string(), project.map(str::to_string));
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                let r = c.search(&query, project.as_deref(), limit, offset);
                if cancel.load(Ordering::SeqCst) {
                    return None;
                }
                match r {
                    Ok(r) => {
                        let docs: Vec<(String, i64)> = r.hits.iter().filter_map(|h| h.doc_id.map(|d| (h.path.to_lowercase(), d))).collect();
                        if !r.hits.is_empty() {
                            let _ = tx.send(Envelope { id, body: Event::Hits { hits: r.hits.clone() } });
                        }
                        Some(Ok((docs, r.hits.len(), r.more)))
                    }
                    Err(e) => {
                        let _ = tx.send(Envelope { id, body: Event::Notice { message: format!("Office results unavailable: {e:#}. Showing this PC's results only.") } });
                        Some(Err(()))
                    }
                }
            })
        });
        let all = self.search_hits(query, &roots)?;
        self.record_latency(started.elapsed().as_millis() as u64);
        let page: Vec<Hit> = all.iter().skip(offset).take(limit).cloned().collect();
        let mut more = all.len() > offset + limit;
        let mut total = page.len();
        let mut reported: HashSet<String> = all.iter().map(|h| h.path.to_lowercase()).collect();
        if !page.is_empty() {
            let _ = tx.send(Envelope { id, body: Event::Hits { hits: page } });
        }
        if let Some(job) = office_job
            && let Ok(Some(Ok((docs, n, office_more)))) = job.join()
        {
            let mut o = self.office.lock().unwrap();
            for (p, d) in docs {
                if o.docs.len() > 20_000 {
                    o.docs.clear();
                }
                o.docs.insert(p, d);
            }
            total += n;
            more |= office_more;
        }
        if deep && offset == 0 && !cancel.load(Ordering::SeqCst) {
            let drives = self.drives.read().unwrap().clone();
            total += self.deep_search(id, &q, &roots, &drives, &cancel, &mut reported, tx)?;
        }
        self.searches.lock().unwrap().remove(&id);
        let _ = tx.send(Envelope { id, body: Event::Done { total, millis: started.elapsed().as_millis() as u64, more } });
        Ok(())
    }

    fn office_roots_usable(&self) -> bool {
        matches!(self.office.lock().unwrap().status.state, ServerState::Connected | ServerState::Syncing | ServerState::Offline)
    }

    #[allow(clippy::too_many_arguments)]
    fn deep_search(&self, id: u64, q: &SearchQuery, roots: &[String], drives: &DriveMap, cancel: &Arc<AtomicBool>, reported: &mut HashSet<String>, tx: &EventTx) -> Result<usize> {
        let s = self.settings.read().unwrap().clone();
        let roots: Vec<String> = if roots.is_empty() { self.walk_roots() } else { roots.iter().map(|r| drives.to_unc(r)).collect() };
        let label = |r: &str| drives.to_display(r).trim_end_matches('\\').to_string();
        let progress: Mutex<Vec<RootProgress>> = Mutex::new(
            roots.iter().map(|r| RootProgress { root: r.clone(), label: label(r), state: RootState::Searching, folders_done: 0, files_seen: 0, hits: 0, skipped: vec![] }).collect(),
        );
        let found = Mutex::new((std::mem::take(reported), 0usize, 0usize)); // reported, new hits, queued
        let store = Mutex::new(self.open_store()?);
        let last_sent = Mutex::new(Instant::now() - Duration::from_secs(1));
        let send_progress = |force: bool| {
            let mut last = last_sent.lock().unwrap();
            if force || last.elapsed() > Duration::from_millis(300) {
                *last = Instant::now();
                let _ = tx.send(Envelope { id, body: Event::Progress { roots: progress.lock().unwrap().clone() } });
            }
        };
        let with_root = |root: &str, f: &mut dyn FnMut(&mut RootProgress)| {
            if let Some(p) = progress.lock().unwrap().iter_mut().find(|p| p.root == root) {
                f(p);
            }
        };
        // A file name matches when every term of any one group is in it.
        let name_class = |name: &str| -> Option<(MatchClass, String)> {
            q.groups
                .iter()
                .filter_map(|g| {
                    let d: Vec<_> = g.iter().map(|t| t.explain(name, 1.0)).collect();
                    if d.iter().all(|x| x.is_some()) { d.into_iter().flatten().max_by_key(|x| x.class).map(|x| (x.class, x.why)) } else { None }
                })
                .min_by_key(|(c, _)| *c)
                .filter(|(c, _)| *c != MatchClass::Uncertain)
        };
        let cfg = WalkConfig {
            per_server: s.listings_per_server.max(1) as usize,
            local_threads: 6,
            listing_timeout: Duration::from_secs(s.listing_timeout_secs.max(5) as u64),
            excluded: s.excluded.iter().map(|x| x.to_lowercase()).collect(),
            background: false,
        };
        let office = self.office_roots();
        walk_resumable(&roots, &cfg, cancel, &|_| None, &|ev| match ev {
            WalkEvent::Listed { root, dir, entries } => {
                let mut new_hits = Vec::new();
                let mut files = 0u64;
                for e in entries.iter().filter(|e| !e.is_dir) {
                    files += 1;
                    let path = join(&dir, &e.name);
                    let lower = path.to_lowercase();
                    let mut g = found.lock().unwrap();
                    if g.0.contains(&lower) {
                        continue;
                    }
                    if let Some((class, why)) = name_class(&e.name) {
                        g.0.insert(lower.clone());
                        g.1 += 1;
                        new_hits.push(Hit {
                            display_path: drives.to_display(&path),
                            path: path.clone(),
                            name: e.name.clone(),
                            page: None,
                            found: e.name.clone(),
                            class,
                            source: TextSource::FileName,
                            confidence: 1.0,
                            hits: 1,
                            size: e.size,
                            modified: e.mtime,
                            why: format!("In the file name: {}", why.to_lowercase()),
                            origin: Origin::Local,
                            doc_id: None,
                            drawing_no: None,
                            revision: None,
                        });
                    }
                    // PDFs the office server reads are left to it.
                    if is_pdf(&e.name) && g.2 < DEEP_READ_LIMIT && !under(&lower, &office) {
                        let st = store.lock().unwrap();
                        let row = st.file_by_path(&path).ok().flatten();
                        let needs = match &row {
                            None => true,
                            Some(r) => !matches!(r.read_state, state::READ | state::FAILED),
                        };
                        if needs {
                            if row.is_none() {
                                let _ = st.upsert_file(&path, &dir, &e.name, e.size, e.mtime, 0, state::WAITING);
                            }
                            if st.request_read(&path).unwrap_or(false) {
                                g.2 += 1;
                                self.waiting.lock().unwrap().entry(lower.clone()).or_default().push(Waiter { search_id: id, query: q.clone(), tx: tx.clone() });
                            }
                        }
                    }
                }
                let n = new_hits.len() as u64;
                with_root(&root, &mut |p| {
                    p.folders_done += 1;
                    p.files_seen += files;
                    p.hits += n;
                });
                if !new_hits.is_empty() {
                    let _ = tx.send(Envelope { id, body: Event::Hits { hits: new_hits } });
                }
                send_progress(false);
            }
            WalkEvent::Skipped { root, dir, reason } => {
                with_root(&root, &mut |p| {
                    if p.skipped.len() < 200 {
                        p.skipped.push((drives.to_display(&dir), reason.clone()));
                    }
                });
            }
            WalkEvent::RootOffline { root, reason } => {
                with_root(&root, &mut |p| {
                    p.state = RootState::Offline;
                    p.skipped.push((label(&root), reason.clone()));
                });
                send_progress(true);
            }
            WalkEvent::RootDone { root } => {
                with_root(&root, &mut |p| p.state = RootState::Done);
                send_progress(true);
            }
        });
        if cancel.load(Ordering::SeqCst) {
            for p in progress.lock().unwrap().iter_mut() {
                if p.state == RootState::Searching {
                    p.state = RootState::Stopped;
                }
            }
        }
        send_progress(true);
        let (rep, new_hits, queued) = found.into_inner().unwrap();
        *reported = rep;
        if queued > 0 {
            self.reader_signal.raise();
        }
        Ok(new_hits)
    }

    pub fn read_now(&self, path: &str) -> Result<()> {
        let drives = self.drives.read().unwrap().clone();
        let unc = drives.to_unc(path);
        let store = self.open_store()?;
        if store.file_by_path(&unc)?.is_none() {
            let p = Path::new(&unc);
            let md = std::fs::metadata(p)?;
            let dir = p.parent().map(|d| norm_dir(&d.to_string_lossy())).unwrap_or_default();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
            store.upsert_file(&unc, &dir, &name, md.len(), mtime, 0, state::WAITING)?;
        }
        store.request_read(&unc)?;
        self.reader_signal.raise();
        Ok(())
    }

    // ---- preview -----------------------------------------------------------

    pub fn preview(&self, path: &str, page: u32, query: &str, max_px: u32, tag: Option<&str>, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let id = self.office_doc(&c, path)?;
            let r = c.page(id, page, query, tag, max_px)?;
            return Ok(Event::Preview { png_base64: r.png_base64, width: r.width, height: r.height, pages: r.pages, boxes: r.boxes, source: r.source });
        }
        self.render_page(path, page, query, max_px, tag)
    }

    /// Render a page of a file in this index with its marks (also used by
    /// the office server for its clients).
    pub fn render_page(&self, path: &str, page: u32, query: &str, max_px: u32, tag: Option<&str>) -> Result<Event> {
        let doc = crate::pdf::Doc::open(Path::new(path))?;
        let pages = doc.page_count();
        let page = page.clamp(1, pages.max(1));
        let img = doc.render_preview(page - 1, max_px.clamp(400, 3000))?;
        let (width, height) = (img.width(), img.height());
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        let store = self.open_store()?;
        let terms: Vec<Term> = parse_search(query).all_terms();
        let (mut boxes, source) = match store.file_by_path(path)? {
            Some(f) if f.read_state == state::READ => {
                let info = store.page_info(f.id)?;
                let ocr = info.iter().find(|p| p.page == page).is_some_and(|p| p.ocr_tiles > p.ocr_tiles_skipped);
                (store.page_boxes(f.id, page, &terms)?, if ocr { TextSource::Ocr } else { TextSource::TextLayer })
            }
            _ => (Vec::new(), TextSource::Pending),
        };
        if let (Some(t), Some(f)) = (tag, store.file_by_path(path)?) {
            for b in store.tag_boxes(f.id, page, &sleuth_core::analyzer::norm_form(t))? {
                if !boxes.iter().any(|x| x.overlap(&b) > 0.9) {
                    boxes.push(b);
                }
            }
        }
        Ok(Event::Preview { png_base64: base64::engine::general_purpose::STANDARD.encode(png), width, height, pages, boxes, source })
    }

    /// A rectangle of a page as PNG, with some margin, about `max_px` wide.
    pub fn crop(&self, path: &str, page: u32, b: NormBox, max_px: u32) -> Result<Event> {
        let doc = crate::pdf::Doc::open(Path::new(path))?;
        let page = page.clamp(1, doc.page_count().max(1));
        let (pw, ph) = doc.page_size(page - 1)?;
        let pad_x = (b.h * ph / pw).max(0.01) * 1.5;
        let pad_y = b.h.max(0.005) * 1.2;
        let (x0, y0) = ((b.x - pad_x).max(0.0), (b.y - pad_y).max(0.0));
        let (x1, y1) = ((b.x + b.w + pad_x).min(1.0), (b.y + b.h + pad_y).min(1.0));
        let want = max_px.clamp(200, 2400) as f32;
        let scale = (want / ((x1 - x0) * pw).max(1.0)).min(600.0 / 72.0);
        let (w, h) = (((x1 - x0) * pw * scale).ceil() as i32, ((y1 - y0) * ph * scale).ceil() as i32);
        let img = doc.render_tile(page - 1, scale, (x0 * pw * scale) as i32, (y0 * ph * scale) as i32, w.max(1), h.max(1))?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        Ok(Event::Image { png_base64: base64::engine::general_purpose::STANDARD.encode(png), width: w as u32, height: h as u32 })
    }

    // ---- engineering workflows ------------------------------------------------

    fn scope_for(&self, project: Option<&str>) -> Result<Scope> {
        Ok(match project.filter(|p| !p.trim().is_empty()) {
            Some(p) => Scope::roots(&self.project_roots(p)?),
            None => Scope::default(),
        })
    }

    pub fn tags(&self, category: Option<&str>, contains: Option<&str>, project: Option<&str>, limit: usize, offset: usize, office: bool) -> Result<Event> {
        let (tags, total) = if office {
            self.office()?.tags(category, contains, project, limit, offset)?
        } else {
            self.open_store()?.tags(category, contains, &self.scope_for(project)?, limit.min(2000), offset)?
        };
        Ok(Event::Tags { tags, total })
    }

    pub fn occurrences(&self, norm: &str, project: Option<&str>, office: bool) -> Result<Event> {
        let items = if office {
            self.office()?.occurrences(norm, project)?
        } else {
            let drives = self.drives.read().unwrap().clone();
            self.open_store()?.occurrences(norm, &self.scope_for(project)?, &drives)?
        };
        Ok(Event::Occurrences { items })
    }

    pub fn revisions(&self, contains: Option<&str>, project: Option<&str>, limit: usize, office: bool) -> Result<Event> {
        let groups = if office {
            self.office()?.revisions(contains, project, limit)?
        } else {
            let drives = self.drives.read().unwrap().clone();
            let order = self.settings.read().unwrap().revision_order;
            self.open_store()?.revision_groups(contains, &self.scope_for(project)?, order, limit.min(2000), &drives)?
        };
        Ok(Event::Revisions { groups })
    }

    pub fn compare(&self, old: &str, new: &str, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let (a, b) = (self.office_doc(&c, old)?, self.office_doc(&c, new)?);
            let r = c.compare(a, b)?;
            return Ok(Event::Comparison { old: r.old, new: r.new, changes: r.changes, unchanged: r.unchanged });
        }
        let store = self.open_store()?;
        let (changes, unchanged) = store.compare(old, new)?;
        let meta = |p: &str| -> Result<DrawingMeta> { store.file_by_path(p)?.map(|f| store.metadata(f.id)).transpose().map(Option::unwrap_or_default) };
        Ok(Event::Comparison { old: meta(old)?, new: meta(new)?, changes, unchanged })
    }

    pub fn related(&self, path: &str, office: bool) -> Result<Event> {
        let items = if office {
            let c = self.office()?;
            let id = self.office_doc(&c, path)?;
            c.related(id)?
        } else {
            let drives = self.drives.read().unwrap().clone();
            self.open_store()?.related(path, &Scope::default(), &drives)?
        };
        Ok(Event::Related { items })
    }

    pub fn doc_info(&self, path: &str, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let id = self.office_doc(&c, path)?;
            let d = c.document(id)?;
            return Ok(Event::DocInfo { metadata: d.metadata, pages: d.pages, provenance: d.provenance, tags: d.tags });
        }
        let store = self.open_store()?;
        let f = store.file_by_path(path)?.ok_or_else(|| anyhow!("not in the index yet"))?;
        let tags: u32 = store.conn.query_row("SELECT COUNT(*) FROM tag_occurrences WHERE file_id=?1", [f.id], |r| r.get(0))?;
        Ok(Event::DocInfo { metadata: store.metadata(f.id)?, pages: store.page_info(f.id)?, provenance: store.provenance(f.id)?, tags })
    }

    pub fn low_confidence(&self, path: Option<&str>, limit: usize, offset: usize) -> Result<Event> {
        let drives = self.drives.read().unwrap().clone();
        let (items, total) = self.open_store()?.low_confidence(path, limit.min(500), offset, &drives)?;
        Ok(Event::LowConfidence { items, total })
    }

    pub fn correct(&self, line_id: i64, text: &str) -> Result<Event> {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        let mut store = self.open_store()?;
        let cid = store.correct(line_id, text, &user, now(), &self.deriver())?;
        store.event(now(), "info", &format!("{user} corrected a reading to \"{}\" (correction {cid})", text.trim()));
        self.changed();
        Ok(Event::Ok)
    }

    pub fn uncorrect(&self, correction_id: i64) -> Result<Event> {
        let mut store = self.open_store()?;
        store.uncorrect(correction_id, &self.deriver())?;
        self.changed();
        Ok(Event::Ok)
    }

    pub fn crop_line(&self, line_id: i64, max_px: u32) -> Result<Event> {
        let (path, page, b) = self.open_store()?.line_place(line_id)?.ok_or_else(|| anyhow!("that line is no longer in the index"))?;
        self.crop(&path, page, b, max_px)
    }

    pub fn history(&self) -> Result<Event> {
        let items = self.open_store()?.history()?.into_iter().map(|(query, at, pinned)| HistoryItem { query, at, pinned }).collect();
        Ok(Event::History { items })
    }

    pub fn pin(&self, query: &str, pinned: bool) -> Result<Event> {
        self.open_store()?.pin(query, pinned, now())?;
        Ok(Event::Ok)
    }

    pub fn monitor(&self) -> Result<Monitor> {
        let (status, _) = self.status()?;
        let store = self.open_store()?;
        let one = |sql: &str| -> u64 { store.conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64 };
        let mut by_class: Vec<(String, u64)> = {
            let mut st = store.conn.prepare("SELECT class, COUNT(*) FROM pages p JOIN files f ON f.id=p.file_id WHERE f.read_state=2 GROUP BY class")?;
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?
        };
        by_class.sort_by(|a, b| b.1.cmp(&a.1));
        let db_bytes = ["db", "db-wal"].iter().filter_map(|e| std::fs::metadata(self.db_path.with_extension(e)).ok()).map(|m| m.len()).sum();
        let (p50, p95) = {
            let mut l: Vec<u64> = self.latencies.lock().unwrap().iter().copied().collect();
            l.sort_unstable();
            let at = |q: f64| if l.is_empty() { 0 } else { l[((l.len() - 1) as f64 * q).round() as usize] };
            (at(0.5), at(0.95))
        };
        let mut errors = self.deriver().errors.clone();
        errors.truncate(20);
        Ok(Monitor {
            status,
            db_bytes,
            pages_total: one("SELECT COUNT(*) FROM pages p JOIN files f ON f.id=p.file_id WHERE f.read_state=2"),
            pages_by_class: by_class,
            ocr_millis_per_page: one("SELECT CAST(AVG(ocr_ms) AS INTEGER) FROM pages WHERE ocr_tiles>ocr_skipped"),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            cache_claims_waited: self.claims_waited.load(Ordering::Relaxed),
            search_p50_ms: p50,
            search_p95_ms: p95,
            searches: self.searches_total.load(Ordering::Relaxed),
            tags: one("SELECT COUNT(DISTINCT tag_id) FROM tag_occurrences"),
            tag_occurrences: one("SELECT COUNT(*) FROM tag_occurrences"),
            drawings_with_metadata: one("SELECT COUNT(*) FROM drawing_metadata WHERE drawing_no IS NOT NULL"),
            corrections: one("SELECT COUNT(*) FROM corrections WHERE stale=0"),
            roots: store.root_scans()?,
            events: store.events(60)?,
            rule_errors: errors,
            backend_note: self.backend_note.lock().unwrap().clone(),
            change_journal: self.journal_note.lock().unwrap().clone(),
        })
    }

    /// Queue state for the office server's job list.
    pub fn jobs(&self, failed: bool, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let store = self.open_store()?;
        let st_code = if failed { state::FAILED } else { state::WAITING };
        let mut st = store.conn.prepare("SELECT id, path, COALESCE(read_error,'') FROM files WHERE read_state=?1 ORDER BY priority DESC, mtime DESC LIMIT ?2")?;
        let rows = st.query_map(rusqlite::params![st_code, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Take a waiting file out of the queue (office server admin).
    pub fn cancel_job(&self, file_id: i64) -> Result<bool> {
        let store = self.open_store()?;
        let n = store.conn.execute("UPDATE files SET read_state=?2, read_error='cancelled by an administrator', lease_until=NULL WHERE id=?1 AND read_state=?3", rusqlite::params![file_id, state::FAILED, state::WAITING])?;
        Ok(n > 0)
    }

    pub fn file_path(&self, id: i64) -> Result<Option<String>> {
        Ok(self.open_store()?.file_by_id(id)?.filter(|f| f.read_state != state::GONE).map(|f| f.path))
    }

    pub fn file_id(&self, path: &str) -> Result<Option<i64>> {
        Ok(self.open_store()?.file_by_path(path)?.filter(|f| f.read_state != state::GONE).map(|f| f.id))
    }

    /// Whether the first full walk and the reading queue are finished.
    pub fn syncing(&self) -> bool {
        let pass_done = self.open_store().ok().and_then(|s| s.get_meta("pass").ok().flatten()).is_some_and(|v| v.ends_with("|done"));
        let waiting = self.open_store().ok().and_then(|s| s.counts().ok()).map(|c| c.2).unwrap_or(0);
        !pass_done || waiting > 0
    }
}

/// Errors that may go away by themselves: the file could not be opened or
/// read (network share unreachable, file locked by another program).
fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()) || format!("{e:#}").contains("cannot open file")
}

fn friendly(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    if s.contains("password") {
        "password protected".into()
    } else if s.contains("damaged") {
        "damaged or not a PDF".into()
    } else if let Some(rest) = s.strip_prefix("cannot open file: ") {
        format!("could not be opened: {}", rest.chars().take(160).collect::<String>())
    } else {
        s.chars().take(200).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_names_with_non_ascii() {
        assert!(is_pdf("P1001-ISO.PDF"));
        assert!(!is_pdf("Price list €.xlsx"));
        assert!(!is_pdf("€€"));
        assert!(is_pdf("Ölleitung €.pdf"));
    }

    #[test]
    fn scopes_match_whole_folders() {
        let s = vec![r"\\srv\proj".to_string()];
        assert!(under(r"\\srv\proj\a.pdf", &s));
        assert!(!under(r"\\srv\project2\a.pdf", &s));
    }
}
