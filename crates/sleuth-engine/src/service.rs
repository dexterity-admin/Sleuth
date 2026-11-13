//! The engine that `sleuth-worker.exe` (and the office server) runs.
//!
//! Since 1.1 nothing large lives on the PC (see
//! `docs/design/office-store.md`): file names are in the in-memory catalog,
//! which one PC per share keeps current for the whole office; read
//! documents are in the parts pool on the shared folder. This file holds
//! the engine's state, its settings and status, searching and previews.
//! `indexing.rs` keeps the catalog, `reading.rs` reads documents, and
//! `views.rs` answers the tag, revision and correction pages.

use crate::cache::check_cache_with;
use crate::crawl::{WalkConfig, WalkEvent, norm_dir, walk_resumable};
use crate::derive::Deriver;
use crate::pool::Pool;
use crate::readjob::Job;
use crate::remote;
use crate::search::{attach_twins, search_index};
use crate::store::{Store, state};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use sleuth_core::analyzer::{MatchClass, SearchQuery, Term, parse_search};
use sleuth_core::kinds::FileKind;
use sleuth_core::paths::{DriveKind, DriveMap, canonical_path, host_address};
use sleuth_core::protocol::*;
use sleuth_core::settings::{Settings, data_dir};
use sleuth_office::{Catalog, Mark, RootFiles, RootState};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub type EventTx = tokio::sync::mpsc::UnboundedSender<Envelope<Event>>;

/// Unread documents a single deep search may queue for reading.
const DEEP_READ_LIMIT: usize = 400;
/// Most result rows kept for paging through one search.
const RESULT_CAP: usize = 5000;
/// Searches remembered with their results (dropped when anything changes).
const RESULT_MEMORY: usize = 12;
/// Said while the office index is opened after a start; the window searches
/// again by itself once it is ready.
pub const STILL_OPENING: &str = "The office index is still opening after the start; text inside documents will be added to these results by itself in a moment.";

/// Page images kept for the preview (a few MB each).
const PREVIEW_MEMORY: usize = 10;

/// A rendered page as the preview receives it.
pub(crate) struct Rendered {
    png_base64: String,
    width: u32,
    height: u32,
    pages: u32,
    page: u32,
}
/// How often the office server is asked how it is.
const OFFICE_INTERVAL: Duration = Duration::from_secs(60);
/// Lines of a text preview sent at once.
const TEXT_PREVIEW_LINES: usize = 2000;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn under(path_lower: &str, scopes: &[String]) -> bool {
    scopes.iter().any(|s| {
        let s = s.trim_end_matches('\\').to_lowercase();
        path_lower == s || path_lower.starts_with(&format!("{s}\\"))
    })
}

/// A walked root as the user knows it: `Z:\ (\\server\Project)`.
pub(crate) fn root_display(root: &str, drives: &DriveMap) -> String {
    let shown = drives.to_display(root);
    if shown.eq_ignore_ascii_case(root) { root.to_string() } else { format!("{shown} ({})", root.trim_end_matches('\\')) }
}

pub(crate) fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Reading folders as stored paths, without duplicates.
pub(crate) fn resolve_scopes(chosen: &[String], drives: &DriveMap, address: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in chosen {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        let norm = norm_dir(&canonical_path(&drives.to_unc(c), address));
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&norm)) {
            out.push(norm);
        }
    }
    out
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

pub(crate) struct Waiter {
    pub search_id: u64,
    pub query: SearchQuery,
    pub tx: EventTx,
}

#[derive(Default)]
pub(crate) struct Signal {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    pub fn raise(&self) {
        *self.flag.lock().unwrap() = true;
        self.cv.notify_all();
    }
    /// Wait until raised or `timeout`; clears the flag. True when raised.
    pub fn wait(&self, timeout: Duration) -> bool {
        let g = self.flag.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |raised| !*raised).unwrap();
        let raised = *g;
        *g = false;
        raised
    }
}

/// The office server as this PC sees it.
#[derive(Default)]
pub(crate) struct Office {
    pub client: Option<Arc<remote::Client>>,
    pub status: ServerStatus,
    /// Office document ids by lower-case path, learned from its results.
    pub docs: HashMap<String, i64>,
}

/// What the reading folders hold, refreshed by the stats thread.
#[derive(Default, Clone)]
pub(crate) struct CountSnap {
    pub read: u64,
    pub waiting: u64,
    pub failed: u64,
    pub ocr_waiting: u64,
    pub scopes: Vec<ScopeProgress>,
    pub kinds: Vec<KindCount>,
    pub fails: Vec<Failed>,
}

/// One share or local drive in the catalog.
pub(crate) struct RootInfo {
    pub root: String,
    /// One of this PC's own disks (its catalog is kept on this PC).
    pub local: bool,
    pub files: Arc<RootFiles>,
    pub mark: Mark,
    pub loaded: bool,
    /// As last read from the root's `state.json` (written by its walker).
    pub state: Option<RootState>,
    /// This PC is listing it right now.
    pub walking: bool,
}

/// A file waiting to be read.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub(crate) struct Queue {
    pub fast: VecDeque<Item>,
    pub ocr: VecDeque<Item>,
}

/// A file not to be tried again before `until` (it failed for now, another
/// PC holds it, or this PC cannot read its type).
#[derive(Debug, Clone)]
pub(crate) struct Retry {
    pub until: i64,
    pub attempts: u32,
    pub reason: String,
}

pub struct Engine {
    pub settings: RwLock<Settings>,
    pub drives: RwLock<DriveMap>,
    /// Runs as the office server: never throttled, roots from settings.
    pub server_mode: bool,
    pub shutdown: Arc<AtomicBool>,
    pub(crate) data: PathBuf,
    /// This PC's name in the office store.
    pub(crate) pc: String,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) pool: Arc<Pool>,
    /// `local.db`: history, events, change-journal checkpoints.
    pub(crate) local: Mutex<Option<Store>>,
    pub(crate) status: Mutex<Status>,
    pub(crate) counts: Mutex<CountSnap>,
    pub(crate) cache: Mutex<CacheCheck>,
    pub(crate) reader_signal: Signal,
    pub(crate) indexer_signal: Signal,
    pub(crate) follow_signal: Signal,
    pub(crate) heartbeat_signal: Signal,
    /// The office index's parts were opened at least once since start.
    pub(crate) pool_ready: AtomicBool,
    pub(crate) cache_signal: Signal,
    pub(crate) office_signal: Signal,
    pub(crate) stats_signal: Signal,
    pub(crate) waiting: Mutex<HashMap<String, Vec<Waiter>>>,
    searches: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// (when, pages) of every document read here in the last hour.
    pub(crate) pages_read: Mutex<VecDeque<(Instant, u32)>>,
    results: Mutex<VecDeque<(String, (u64, u64), Arc<Vec<Hit>>)>>,
    /// The last pages rendered for the preview, by file, page, size and
    /// the file's size and date: going back to a result shows it at once.
    previews: Mutex<VecDeque<(String, Arc<Rendered>)>>,
    pub(crate) latencies: Mutex<VecDeque<u64>>,
    pub(crate) searches_total: AtomicU64,
    pub(crate) cache_hits: AtomicU64,
    pub(crate) cache_misses: AtomicU64,
    pub(crate) claims_waited: AtomicU64,
    pub(crate) deriver: RwLock<Arc<Deriver>>,
    pub(crate) backend_note: Mutex<String>,
    pub(crate) journal_note: Mutex<String>,
    pub(crate) office: Mutex<Office>,
    /// Server names and the addresses they were last seen at.
    pub(crate) hosts: Mutex<HashMap<String, String>>,
    pub(crate) roots: Mutex<HashMap<String, RootInfo>>,
    /// List every root again on the next indexer pass.
    pub(crate) full_walk: AtomicBool,
    /// List only this root again on the next pass ("Rescan this drive").
    pub(crate) walk_root: Mutex<Option<String>>,
    /// Stops a walk in progress (pause, shutdown, a lost lease).
    pub(crate) walk_cancel: Arc<AtomicBool>,
    /// Read without waiting for the first listing ("Read now").
    pub(crate) read_now_flag: AtomicBool,
    /// Unix seconds a timed full speed ends; 0 when not timed.
    pub(crate) speed_until: AtomicI64,
    /// Files someone asked for, read before the backlog.
    pub(crate) urgent: Mutex<VecDeque<Job>>,
    pub(crate) queue: Mutex<Queue>,
    pub(crate) retry: Mutex<HashMap<u64, Retry>>,
    /// Held while `work.db` is published and emptied, and while a
    /// correction is written to it, so neither loses the other's rows.
    pub(crate) work_gate: Mutex<()>,
    /// A correction changed `work.db` behind the reader loop's back.
    pub(crate) work_dirty: AtomicBool,
    pub(crate) pcs: Mutex<Vec<PcStatus>>,
    pub(crate) monitor_memo: Mutex<Option<(u64, Instant, Box<Monitor>)>>,
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
        crate::derive::set_current(Arc::clone(&deriver));
        let pc =std::env::var("SLEUTH_PC_NAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "this-pc".into());
        Arc::new(Engine {
            settings: RwLock::new(settings),
            drives: RwLock::new(DriveMap::default()),
            server_mode,
            shutdown: Arc::new(AtomicBool::new(false)),
            data: data_dir(),
            pc,
            catalog: Arc::new(Catalog::new()),
            pool: Arc::new(Pool::new()),
            local: Mutex::new(None),
            status: Mutex::new(Status { worker_version: env!("CARGO_PKG_VERSION").into(), reader_state: "starting".into(), ocr_backend: "CPU".into(), ..Default::default() }),
            counts: Mutex::new(CountSnap::default()),
            cache: Mutex::new(CacheCheck::default()),
            reader_signal: Signal::default(),
            indexer_signal: Signal::default(),
            follow_signal: Signal::default(),
            heartbeat_signal: Signal::default(),
            pool_ready: AtomicBool::new(false),
            cache_signal: Signal::default(),
            office_signal: Signal::default(),
            stats_signal: Signal::default(),
            waiting: Mutex::new(HashMap::new()),
            searches: Mutex::new(HashMap::new()),
            pages_read: Mutex::new(VecDeque::new()),
            results: Mutex::new(VecDeque::new()),
            previews: Mutex::new(VecDeque::new()),
            latencies: Mutex::new(VecDeque::new()),
            searches_total: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            claims_waited: AtomicU64::new(0),
            deriver: RwLock::new(deriver),
            backend_note: Mutex::new(String::new()),
            journal_note: Mutex::new(String::new()),
            office: Mutex::new(Office::default()),
            hosts: Mutex::new(HashMap::new()),
            roots: Mutex::new(HashMap::new()),
            full_walk: AtomicBool::new(false),
            walk_root: Mutex::new(None),
            walk_cancel: Arc::new(AtomicBool::new(false)),
            read_now_flag: AtomicBool::new(false),
            speed_until: AtomicI64::new(0),
            urgent: Mutex::new(VecDeque::new()),
            queue: Mutex::new(Queue::default()),
            retry: Mutex::new(HashMap::new()),
            work_gate: Mutex::new(()),
            work_dirty: AtomicBool::new(false),
            pcs: Mutex::new(Vec::new()),
            monitor_memo: Mutex::new(None),
        })
    }

    /// Run `f` on this PC's small bookkeeping database.
    pub(crate) fn with_local<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let g = self.local.lock().unwrap();
        match g.as_ref() {
            Some(s) => f(s),
            None => bail!("the worker is still starting"),
        }
    }

    /// Record something for the monitor page's event list.
    pub(crate) fn event(&self, level: &str, message: &str) {
        let _ = self.with_local(|s| {
            s.event(now(), level, message);
            Ok(())
        });
    }

    pub(crate) fn deriver(&self) -> Arc<Deriver> {
        Arc::clone(&self.deriver.read().unwrap())
    }

    /// What a search can see: the catalog and the pool as they are now.
    fn generation(&self) -> (u64, u64) {
        (self.catalog.generation(), self.pool.generation())
    }

    /// The office store's folder when it is reachable.
    pub(crate) fn store_dir(&self) -> Option<PathBuf> {
        let c = self.cache.lock().unwrap();
        (c.state == CacheState::Ok).then(|| PathBuf::from(&c.path))
    }

    /// A server name as its address, remembered so a name server that is
    /// down for a moment does not change how paths are written.
    pub(crate) fn address(&self, host: &str) -> Option<String> {
        let key = host.to_lowercase();
        match host_address(host) {
            Some(ip) => {
                let mut h = self.hosts.lock().unwrap();
                if h.get(&key) != Some(&ip) {
                    h.insert(key.clone(), ip.clone());
                    drop(h);
                    let _ = self.with_local(|s| s.set_meta(&format!("host:{key}"), &ip));
                }
                Some(ip)
            }
            None => self.hosts.lock().unwrap().get(&key).cloned(),
        }
    }

    /// This PC's drives with every share written by its server's address.
    pub fn current_drives(&self) -> DriveMap {
        let mut map = DriveMap::current();
        map.canonicalize(&|h| self.address(h));
        // Two letters for one share are one root.
        map.merge_aliases(|h| Some(h.to_string()));
        map
    }

    /// A path as it is stored: drive letter to share, server name to address.
    pub fn stored_path(&self, path: &str) -> String {
        let unc = self.drives.read().unwrap().to_unc(path);
        canonical_path(&unc, &|h| self.address(h))
    }

    pub fn start(self: &Arc<Self>) -> Result<()> {
        std::fs::create_dir_all(&self.data).with_context(|| format!("create {}", self.data.display()))?;
        let local = Store::open(&self.data.join("local.db")).context("open this PC's database")?;
        for (k, v) in local.meta_with_prefix("host:").unwrap_or_default() {
            self.hosts.lock().unwrap().insert(k, v);
        }
        local.set_projects(&self.settings.read().unwrap().projects)?;
        for e in &self.deriver().errors {
            local.event(now(), "warn", &format!("rule not used: {e}"));
        }
        *self.local.lock().unwrap() = Some(local);
        *self.drives.write().unwrap() = self.current_drives();
        {
            let s = self.settings.read().unwrap();
            let mut st = self.status.lock().unwrap();
            st.walk_paused = s.pause_walk;
            st.read_paused = s.pause_reading;
            st.speed = s.read_speed;
        }
        let spawn = |name: &str, f: fn(Arc<Engine>)| -> Result<()> {
            let e = Arc::clone(self);
            std::thread::Builder::new().name(name.into()).spawn(move || f(e))?;
            Ok(())
        };
        spawn("sleuth-store", Engine::cache_loop)?;
        spawn("sleuth-follow", Engine::follow_loop)?;
        spawn("sleuth-heartbeat", Engine::heartbeat_loop)?;
        spawn("sleuth-indexer", Engine::indexer_loop)?;
        spawn("sleuth-reader", Engine::reader_loop)?;
        spawn("sleuth-stats", Engine::stats_loop)?;
        if !self.server_mode {
            spawn("sleuth-office", Engine::office_loop)?;
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.walk_cancel.store(true, Ordering::SeqCst);
        for s in [&self.reader_signal, &self.indexer_signal, &self.follow_signal, &self.heartbeat_signal, &self.cache_signal, &self.office_signal, &self.stats_signal] {
            s.raise();
        }
        for flag in self.searches.lock().unwrap().values() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// List every drive again now (or only `root`).
    pub fn rescan_now(&self, root: Option<&str>) {
        match root.map(|r| self.stored_path(r)) {
            Some(r) => *self.walk_root.lock().unwrap() = Some(r),
            None => self.full_walk.store(true, Ordering::SeqCst),
        }
        self.indexer_signal.raise();
    }

    pub fn set_settings(&self, mut s: Settings) -> Result<()> {
        let old = self.settings.read().unwrap().clone();
        // a network path learned for another folder must not be tried for this one
        let norm = |d: &Option<String>| d.as_deref().map(|d| d.trim().trim_end_matches('\\').to_lowercase());
        if norm(&old.shared_cache_dir) != norm(&s.shared_cache_dir) {
            s.shared_cache_unc = None;
        } else if s.shared_cache_unc.is_none() {
            s.shared_cache_unc = old.shared_cache_unc.clone();
        }
        s.save()?;
        *self.settings.write().unwrap() = s.clone();
        self.cache_signal.raise();
        if old.projects != s.projects {
            self.with_local(|l| l.set_projects(&s.projects))?;
        }
        if old.tag_rules != s.tag_rules || old.field_rules != s.field_rules || old.discipline_prefixes != s.discipline_prefixes {
            let d = Arc::new(Deriver::new(&s));
            for e in &d.errors {
                self.event("warn", &format!("rule not used: {e}"));
            }
            crate::derive::set_current(Arc::clone(&d));
            *self.deriver.write().unwrap() = d;
            self.event("info", "tag or title-block rules changed; they apply to documents read from now on");
        }
        {
            let mut st = self.status.lock().unwrap();
            st.walk_paused = s.pause_walk;
            st.read_paused = s.pause_reading;
            st.speed = s.read_speed;
        }
        if old.pause_walk != s.pause_walk && s.pause_walk {
            self.walk_cancel.store(true, Ordering::SeqCst);
        }
        if old.server != s.server {
            self.office_signal.raise();
        }
        self.reader_signal.raise();
        self.indexer_signal.raise();
        self.stats_signal.raise();
        Ok(())
    }

    /// Pause or resume a stage on this PC; kept across restarts.
    pub fn set_paused(&self, stage: Stage, paused: bool) -> Result<()> {
        let mut s = self.settings.read().unwrap().clone();
        match stage {
            Stage::Walk => s.pause_walk = paused,
            Stage::Read => s.pause_reading = paused,
        }
        self.event("info", &format!("{} {} on this PC", if stage == Stage::Walk { "finding files" } else { "reading" }, if paused { "paused" } else { "resumed" }));
        self.set_settings(s)
    }

    /// Do a stage's work now.
    pub fn run_now(&self, stage: Stage, root: Option<&str>) -> Result<()> {
        match stage {
            Stage::Walk => {
                if self.settings.read().unwrap().pause_walk {
                    self.set_paused(Stage::Walk, false)?;
                }
                self.rescan_now(root);
            }
            Stage::Read => {
                if self.settings.read().unwrap().pause_reading {
                    self.set_paused(Stage::Read, false)?;
                }
                self.read_now_flag.store(true, Ordering::SeqCst);
                self.stats_signal.raise();
                self.reader_signal.raise();
            }
        }
        Ok(())
    }

    /// How hard this PC reads. A timed full speed is not saved.
    pub fn set_speed(&self, mode: SpeedMode, minutes: u32) -> Result<()> {
        if mode == SpeedMode::Full && minutes > 0 {
            self.speed_until.store(now() + minutes as i64 * 60, Ordering::SeqCst);
            self.reader_signal.raise();
            return Ok(());
        }
        self.speed_until.store(0, Ordering::SeqCst);
        let mut s = self.settings.read().unwrap().clone();
        s.read_speed = mode;
        self.set_settings(s)
    }

    /// The speed in force now (a timed full speed wins until it runs out).
    pub(crate) fn speed(&self) -> SpeedMode {
        let until = self.speed_until.load(Ordering::SeqCst);
        if until > now() {
            return SpeedMode::Full;
        }
        if until != 0 {
            self.speed_until.store(0, Ordering::SeqCst);
        }
        self.settings.read().unwrap().read_speed
    }

    pub(crate) fn set_reader_note(&self, note: &str) {
        let mut st = self.status.lock().unwrap();
        if st.reader_note != note {
            st.reader_note = note.into();
        }
    }

    pub(crate) fn set_reader_state(&self, s: &str, file: Option<String>) {
        let mut st = self.status.lock().unwrap();
        st.reader_state = s.into();
        st.current_file = file;
    }

    pub fn status(&self) -> Result<(Status, Vec<Failed>)> {
        let snap = self.counts.lock().unwrap().clone();
        let mut st = self.status.lock().unwrap().clone();
        st.files_indexed = self.catalog.file_count();
        st.pdfs_read = snap.read;
        st.pdfs_waiting = snap.waiting;
        st.pdfs_failed = snap.failed;
        st.ocr_waiting = snap.ocr_waiting;
        st.kinds = snap.kinds;
        let mut log = self.pages_read.lock().unwrap();
        while log.front().is_some_and(|(t, _)| t.elapsed() > Duration::from_secs(3600)) {
            log.pop_front();
        }
        st.pages_per_hour = log.iter().map(|(_, n)| *n as u64).sum();
        st.docs_per_hour = log.len() as u64;
        // This PC's pace: documents read in the last hour, over the time
        // since the first of them; unknown until a few were read.
        let mine_per_hour = match (log.front(), log.len()) {
            (Some((first, _)), n) if n >= 3 => n as f64 * 3600.0 / first.elapsed().as_secs_f64().max(60.0),
            _ => 0.0,
        };
        drop(log);
        // Time left is the whole office's: the waiting documents are shared
        // out among every PC that reads, so this PC's pace alone overstates it.
        let others: u64 = {
            let pcs = self.pcs.lock().unwrap();
            pcs.iter().skip(1).filter(|p| now() - p.last_seen < crate::indexing::ACTIVE_SECS).map(|p| p.docs_per_hour).sum()
        };
        let office = mine_per_hour + others as f64;
        st.office_docs_per_hour = office.round() as u64;
        st.reading_eta_secs = if snap.waiting > 0 && office >= 1.0 { (snap.waiting as f64 * 3600.0 / office) as u64 } else { 0 };
        let (roots, last_complete, first_walk) = self.root_progress();
        st.index.first_walk = first_walk;
        st.index.last_complete = last_complete;
        st.index.roots = roots;
        if st.index.phase.is_empty() {
            st.index.phase = "starting".into();
        }
        let mins = self.settings.read().unwrap().name_rescan_minutes.max(15) as i64;
        st.index.next_walk = if st.index.pass_started == 0 && last_complete > 0 { last_complete + mins * 60 } else { 0 };
        st.scopes = snap.scopes;
        st.speed = self.speed();
        st.speed_until = self.speed_until.load(Ordering::SeqCst);
        let cache = self.cache.lock().unwrap().clone();
        let ps = self.pool.stats();
        st.store = StoreStatus {
            state: match cache.state {
                CacheState::Ok => "ok",
                CacheState::NotSet => "not set",
                CacheState::NoAccess => "no access",
                CacheState::Offline => "offline",
                CacheState::Disabled => "disabled",
            }
            .into(),
            path: cache.path.clone(),
            message: cache.message.clone(),
            parts: ps.parts,
            bytes: ps.bytes,
            documents: ps.documents,
            unpublished: st.store.unpublished,
        };
        st.cache = cache;
        st.server = self.office.lock().unwrap().status.clone();
        Ok((st, snap.fails))
    }

    /// Every root as the status shows it, the newest complete listing over
    /// all of them, and whether any root holding a reading folder has never
    /// been listed to the end.
    fn root_progress(&self) -> (Vec<IndexRoot>, i64, bool) {
        let drives = self.drives.read().unwrap().clone();
        let first_walk = !self.reading_roots_listed();
        let roots = self.roots.lock().unwrap();
        let mut out: Vec<IndexRoot> = Vec::new();
        let mut newest = 0i64;
        for r in roots.values() {
            let (dirs, files) = self.catalog.root_counts(&r.root);
            let s = r.state.clone().unwrap_or_default();
            newest = newest.max(s.last_ok);
            let state = match s.phase.as_str() {
                "walking" if r.walking || now() - s.updated < 900 => "walking",
                "walking" => "partial",
                "done" => "done",
                "partial" => "partial",
                "offline" => "offline",
                _ if !r.loaded => "waiting",
                _ if s.last_ok > 0 => "done",
                _ => "waiting",
            };
            out.push(IndexRoot {
                display: root_display(&r.root, &drives),
                root: r.root.clone(),
                state: state.into(),
                folders: dirs,
                files,
                skipped: s.skipped,
                note: s.note.clone(),
                last_ok: s.last_ok,
                by: if r.local { String::new() } else { s.by.clone() },
            });
        }
        out.sort_by(|a, b| a.root.to_lowercase().cmp(&b.root.to_lowercase()));
        (out, newest, first_walk)
    }

    /// Check the office store folder every 2 minutes, and at once after
    /// settings change.
    fn cache_loop(self: Arc<Self>) {
        fn c_ok(s: &CacheState) -> bool {
            *s == CacheState::Ok
        }
        while !self.stopping() {
            let s = self.settings.read().unwrap().clone();
            let drives = self.current_drives();
            let fallback = s.shared_cache_dir.as_deref().and_then(|d| s.shared_cache_fallback(d));
            let c = check_cache_with(s.shared_cache_dir.as_deref(), fallback.as_deref(), s.shared_cache_enabled, &drives);
            self.learn_cache_unc(&s, &c);
            let was = self.cache.lock().unwrap().state;
            if c.state != CacheState::Ok && c.state != was {
                info!("office store {:?}: {}", c.state, c.message);
                self.event("warn", &format!("The office index folder is not usable: {}", c.message));
            }
            let changed = was != c.state;
            *self.cache.lock().unwrap() = c;
            if changed {
                self.follow_signal.raise();
                self.reader_signal.raise();
                if c_ok(&self.cache.lock().unwrap().state) {
                    self.event("info", "The office index folder is back; carrying on by itself");
                }
            }
            // Gone: look again every few seconds, so work resumes on its own
            // as soon as the folder is back (no restart, no clicking).
            let ok = c_ok(&self.cache.lock().unwrap().state);
            self.cache_signal.wait(Duration::from_secs(if ok { 120 } else { 10 }));
        }
    }

    /// Test a folder the user is about to choose.
    pub fn check_cache_dir(&self, dir: &str) -> CacheCheck {
        let fallback = self.settings.read().unwrap().shared_cache_fallback(dir);
        check_cache_with(Some(dir), fallback.as_deref(), true, &self.current_drives())
    }

    /// Remember the network path a drive-letter index folder led to while it
    /// worked, so a PC where that letter later goes missing still finds it.
    fn learn_cache_unc(&self, s: &Settings, c: &CacheCheck) {
        let Some(dir) = s.shared_cache_dir.as_deref().map(str::trim) else { return };
        let lettered = dir.chars().nth(1) == Some(':');
        let direct = !c.message.starts_with("working through");
        if c.state != CacheState::Ok || !lettered || !direct || !c.path.starts_with("\\\\") {
            return;
        }
        if s.shared_cache_unc.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&c.path)) {
            return;
        }
        let mut next = self.settings.read().unwrap().clone();
        if next.shared_cache_dir.as_deref().map(str::trim) != Some(dir) {
            return; // changed meanwhile
        }
        next.shared_cache_unc = Some(c.path.clone());
        match next.save() {
            Ok(()) => *self.settings.write().unwrap() = next,
            Err(e) => warn!("cannot remember the index folder's network path: {e:#}"),
        }
    }

    /// Roots in the catalog: every fixed and network drive minus excluded
    /// ones, or the configured folders' roots on the office server.
    pub(crate) fn walk_roots(&self) -> Vec<String> {
        let s = self.settings.read().unwrap();
        if let Some(r) = &s.index_roots {
            let drives = self.drives.read().unwrap();
            return resolve_scopes(r, &drives, &|h| self.address(h));
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
            .map(|d| norm_dir(&d.root()))
            // Two letters for one share: one root.
            .fold(Vec::<String>::new(), |mut out, r| {
                if !out.iter().any(|o| o.eq_ignore_ascii_case(&r)) {
                    out.push(r);
                }
                out
            })
    }

    /// Folders whose documents this engine reads, as stored paths: a mapped
    /// letter (`Z:\`) becomes its share, or nothing under it would ever be
    /// queued.
    pub(crate) fn read_scopes(&self) -> Vec<String> {
        let s = self.settings.read().unwrap();
        let drives = self.drives.read().unwrap();
        let chosen = match &s.index_roots {
            Some(r) => r.clone(),
            None => s.content_scopes.clone(),
        };
        resolve_scopes(&chosen, &drives, &|h| self.address(h))
    }

    pub(crate) fn office_roots(&self) -> Vec<String> {
        let o = self.office.lock().unwrap();
        if o.status.state == ServerState::NotConfigured { Vec::new() } else { o.status.roots.clone() }
    }

    // ---- office server ----------------------------------------------------

    fn office_loop(self: Arc<Self>) {
        while !self.stopping() {
            self.refresh_office();
            // Offline: try again soon, so the office results come back by
            // themselves as soon as the server answers again.
            let offline = self.office.lock().unwrap().status.state == ServerState::Offline;
            self.office_signal.wait(if offline { Duration::from_secs(15) } else { OFFICE_INTERVAL });
        }
    }

    fn office_client(&self, url: &str, token: &str, ca_file: &str) -> Result<remote::Client> {
        let s = self.settings.read().unwrap().clone();
        let drives = self.drives.read().unwrap().clone();
        // the folder that really works (it may be the network path, when the drive letter is not mapped)
        let store = self.store_dir().map(|p| p.display().to_string()).or(s.shared_cache_dir.clone());
        // A certificate file chosen by hand first; else the one saved when this
        // PC connected itself; else the one in the office index folder.
        if ca_file.trim().is_empty() && !s.server.ca_pem.trim().is_empty() {
            return remote::Client::new(url, token, s.server.ca_pem.as_bytes());
        }
        let ca = remote::ca_path(ca_file, store.as_deref(), &drives).ok_or_else(|| anyhow!("no server certificate file: set one, or put {} in the shared folder", remote::CA_FILE_NAME))?;
        let pem = std::fs::read(&ca).map_err(|e| anyhow!("cannot read the server certificate {}: {e}", ca.display()))?;
        remote::Client::new(url, token, &pem)
    }

    /// "Connect this PC": read the server's card in the office index folder
    /// (address, certificate, join code), ask the server for this PC's own
    /// access key, and save all three. Nothing is typed.
    pub fn join_server(&self) -> Result<ServerStatus> {
        let dir = self.store_dir().ok_or_else(|| anyhow!("the office index folder cannot be reached; Sleuth finds the office server through it"))?;
        let card = crate::cache::read_server_card(&dir).ok_or_else(|| anyhow!("no office server has left its details in {} yet; set up the server first (Sleuth Server window)", dir.display()))?;
        if card.ca_pem.trim().is_empty() || card.join_code.trim().is_empty() {
            bail!("the office server on {} does not let PCs connect themselves (it is older, or this is switched off in its window); type the access key from IT instead", card.pc);
        }
        let req = remote::JoinRequest { code: card.join_code.clone(), pc: self.pc.clone(), user: std::env::var("USERNAME").unwrap_or_default() };
        // By name first, then by each address the server listed.
        let mut last = None;
        for url in std::iter::once(&card.address).chain(card.addresses.iter()) {
            match remote::join(url, card.ca_pem.as_bytes(), &req) {
                Ok(r) => {
                    let mut s = self.settings.read().unwrap().clone();
                    s.server.url = url.clone();
                    s.server.ca_file = String::new();
                    s.server.ca_pem = card.ca_pem.clone();
                    s.server.token_protected = remote::protect(&r.key)?;
                    self.set_settings(s)?;
                    self.event("info", &format!("connected to the office server on {} as {}", card.pc, r.name));
                    self.refresh_office();
                    return Ok(self.office.lock().unwrap().status.clone());
                }
                Err(e) => {
                    let m = format!("{e:#}");
                    // Refused by the server itself: another address gives the same answer.
                    let refused = !(m.contains("cannot be reached") || m.contains("did not answer"));
                    last = Some(e);
                    if refused {
                        break;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("the office server could not be reached")))
    }

    fn refresh_office(&self) {
        let s = self.settings.read().unwrap().server.clone();
        if s.url.trim().is_empty() {
            let mut o = self.office.lock().unwrap();
            if o.status.state != ServerState::NotConfigured {
                *o = Office::default();
                drop(o);
                self.stats_signal.raise();
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
                    roots: c.roots.iter().map(|r| canonical_path(r, &|h| self.address(h))).collect(),
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
            self.stats_signal.raise();
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
            if s.server.url.trim() != url.trim() {
                // A certificate saved for another server does not belong to this one.
                s.server.ca_pem.clear();
            }
            s.server.url = url.trim().to_string();
            s.server.ca_file = ca_file.trim().to_string();
            if !token.trim().is_empty() {
                s.server.token_protected = remote::protect(token.trim())?;
            }
        }
        self.set_settings(s)
    }

    pub(crate) fn office(&self) -> Result<Arc<remote::Client>> {
        let o = self.office.lock().unwrap();
        match (&o.client, o.status.state) {
            (Some(c), ServerState::Connected | ServerState::Syncing) => Ok(Arc::clone(c)),
            (_, ServerState::NotConfigured) => bail!("no office server is set up"),
            _ => bail!("the office server is not available: {}", o.status.message),
        }
    }

    pub(crate) fn office_doc(&self, c: &remote::Client, path: &str) -> Result<i64> {
        if let Some(id) = self.office.lock().unwrap().docs.get(&path.to_lowercase()) {
            return Ok(*id);
        }
        let d = c.lookup(path)?;
        self.office.lock().unwrap().docs.insert(path.to_lowercase(), d.id);
        Ok(d.id)
    }

    // ---- searching ---------------------------------------------------------

    pub(crate) fn notify_waiters(&self, path: &str) {
        let Some(waiters) = self.waiting.lock().unwrap().remove(&path.to_lowercase()) else { return };
        let drives = self.drives.read().unwrap().clone();
        for w in waiters {
            let mut hits: Vec<Hit> = search_index(&self.pool, &self.catalog, &w.query, &[path.to_string()], 5, &drives).unwrap_or_default().into_iter().filter(|h| h.path.eq_ignore_ascii_case(path)).collect();
            attach_twins(&mut hits, &self.pool, &self.catalog, &drives);
            let _ = w.tx.send(Envelope { id: w.search_id, body: Event::Resolved { path: path.to_string(), hits } });
        }
    }

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

    /// Folders of a project by name, as stored paths.
    pub fn project_roots(&self, name: &str) -> Result<Vec<String>> {
        let s = self.settings.read().unwrap();
        let p = s.projects.iter().find(|p| p.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| anyhow!("no project called {name}"))?;
        Ok(p.roots.iter().map(|r| self.stored_path(r)).collect())
    }

    /// Ranked hits for `query` inside `roots` (empty = everywhere),
    /// remembered until the catalog or the pool changes.
    pub fn search_hits(&self, query: &str, roots: &[String]) -> Result<Arc<Vec<Hit>>> {
        let generation = self.generation();
        let key = format!("{query}\u{1}{}", roots.join("\u{2}"));
        if let Some((_, _, hits)) = self.results.lock().unwrap().iter().find(|(k, g, _)| *k == key && *g == generation) {
            return Ok(Arc::clone(hits));
        }
        let q = parse_search(query);
        let drives = self.drives.read().unwrap().clone();
        let roots: Vec<String> = roots.iter().map(|r| self.stored_path(r)).collect();
        let hits = Arc::new(search_index(&self.pool, &self.catalog, &q, &roots, RESULT_CAP, &drives)?);
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

    /// One page of a search's rows, each with the other files of its
    /// document, and every row of the search.
    pub fn search_page(&self, query: &str, roots: &[String], offset: usize, limit: usize) -> Result<(Vec<Hit>, Arc<Vec<Hit>>)> {
        let all = self.search_hits(query, roots)?;
        let mut page: Vec<Hit> = all.iter().skip(offset).take(limit).cloned().collect();
        let drives = self.drives.read().unwrap().clone();
        attach_twins(&mut page, &self.pool, &self.catalog, &drives);
        Ok((page, all))
    }

    /// Whether this search's rows are remembered (it answers at once).
    fn remembered(&self, query: &str, roots: &[String]) -> bool {
        let generation = self.generation();
        let key = format!("{query}\u{1}{}", roots.join("\u{2}"));
        self.results.lock().unwrap().iter().any(|(k, g, _)| *k == key && *g == generation)
    }

    /// The rows file names alone give, from the catalog in memory: shown
    /// while the read documents in the office folder are searched.
    fn names_first(&self, q: &SearchQuery, roots: &[String], limit: usize) -> Vec<Hit> {
        let drives = self.drives.read().unwrap().clone();
        let roots: Vec<String> = roots.iter().map(|r| self.stored_path(r)).collect();
        let mut hits = search_index(&Pool::new(), &self.catalog, q, &roots, limit, &drives).unwrap_or_default();
        for h in &mut hits {
            // Known to be read: say so, so the row does not look unread.
            if let Some(c) = self.pool.covered(&h.path).filter(|c| c.state == state::READ) {
                h.doc_id = Some(crate::pool::compose(c.slot, c.id));
            }
        }
        hits
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
        // The office server's part runs alongside.
        let office = if self.office_roots_usable() { self.office.lock().unwrap().client.clone() } else { None };
        let office_job = office.map(|c| {
            let (query, project) = (query.to_string(), project.map(str::to_string));
            std::thread::spawn(move || c.search(&query, project.as_deref(), limit, offset))
        });
        // File names answer from memory at once; the text inside the read
        // documents follows and replaces these rows.
        if offset == 0 && !self.remembered(query, &roots) {
            let names = self.names_first(&q, &roots, limit);
            if !names.is_empty() && !cancel.load(Ordering::SeqCst) {
                let _ = tx.send(Envelope { id, body: Event::Provisional { hits: names, millis: started.elapsed().as_millis() as u64 } });
            }
        }
        if offset == 0 && !self.pool_ready.load(Ordering::SeqCst) && self.store_dir().is_some() {
            let _ = tx.send(Envelope { id, body: Event::Notice { message: STILL_OPENING.into() } });
        }
        let (page, all) = self.search_page(query, &roots, offset, limit)?;
        self.record_latency(started.elapsed().as_millis() as u64);
        let mut more = all.len() > offset + limit;
        let mut total = page.len();
        let mut reported: HashSet<String> = all.iter().map(|h| h.path.to_lowercase()).collect();
        drop(all);
        if !page.is_empty() {
            let _ = tx.send(Envelope { id, body: Event::Hits { hits: page } });
        }
        // Search history is written after the results are out.
        if offset == 0 {
            let _ = self.with_local(|s| s.add_history(query, now()));
        }
        if let Some(job) = office_job {
            match job.join() {
                Ok(Ok(r)) if !cancel.load(Ordering::SeqCst) => {
                    let mut o = self.office.lock().unwrap();
                    for h in &r.hits {
                        if let Some(d) = h.doc_id {
                            if o.docs.len() > 20_000 {
                                o.docs.clear();
                            }
                            o.docs.insert(h.path.to_lowercase(), d);
                        }
                    }
                    drop(o);
                    // The server reads into the same office index: a
                    // document this PC already listed is not listed twice.
                    let fresh: Vec<Hit> = r.hits.into_iter().filter(|h| reported.insert(self.stored_path(&h.path).to_lowercase())).collect();
                    total += fresh.len();
                    more |= r.more;
                    if !fresh.is_empty() {
                        let _ = tx.send(Envelope { id, body: Event::Hits { hits: fresh } });
                    }
                }
                Ok(Err(e)) => {
                    let _ = tx.send(Envelope { id, body: Event::Notice { message: format!("Office server results unavailable: {e:#}. Showing the office index only.") } });
                }
                _ => {}
            }
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

    /// Walk `roots` live: report files whose name matches that the catalog
    /// did not have yet, and read unread documents found on the way.
    #[allow(clippy::too_many_arguments)]
    fn deep_search(&self, id: u64, q: &SearchQuery, roots: &[String], drives: &DriveMap, cancel: &Arc<AtomicBool>, reported: &mut HashSet<String>, tx: &EventTx) -> Result<usize> {
        let s = self.settings.read().unwrap().clone();
        let roots: Vec<String> = if roots.is_empty() { self.walk_roots() } else { roots.iter().map(|r| self.stored_path(r)).collect() };
        let label = |r: &str| drives.to_display(r).trim_end_matches('\\').to_string();
        let progress: Mutex<Vec<RootProgress>> = Mutex::new(
            roots.iter().map(|r| RootProgress { root: r.clone(), label: label(r), state: RootState_::Searching, folders_done: 0, files_seen: 0, hits: 0, skipped: vec![] }).collect(),
        );
        let found = Mutex::new((std::mem::take(reported), 0usize, 0usize)); // reported, new hits, queued
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
            first: vec![],
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
                    let kind = FileKind::of(&e.name);
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
                            kind,
                            category: kind.category(false),
                            twins: Vec::new(),
                        });
                    }
                    // Documents the office server reads are left to it.
                    if kind.readable() && !s.kinds_not_read.contains(&kind) && g.2 < DEEP_READ_LIMIT && !under(&lower, &office) && self.pool.state_of(&path, e.size, e.mtime).is_none_or(|st| st == state::NEEDS_OCR) {
                        g.2 += 1;
                        self.waiting.lock().unwrap().entry(lower.clone()).or_default().push(Waiter { search_id: id, query: q.clone(), tx: tx.clone() });
                        self.urgent.lock().unwrap().push_back(Job { path: path.clone(), size: e.size, mtime: e.mtime, urgent: true });
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
                if n > 0 || files > 0 {
                    self.reader_signal.raise();
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
                    p.state = RootState_::Offline;
                    p.skipped.push((label(&root), reason.clone()));
                });
                send_progress(true);
            }
            WalkEvent::RootDone { root } => {
                with_root(&root, &mut |p| p.state = RootState_::Done);
                send_progress(true);
            }
        });
        if cancel.load(Ordering::SeqCst) {
            for p in progress.lock().unwrap().iter_mut() {
                if p.state == RootState_::Searching {
                    p.state = RootState_::Stopped;
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

    /// Read one file right away, whatever the queue holds.
    pub fn read_now(&self, path: &str) -> Result<()> {
        let stored = self.stored_path(path);
        let md = std::fs::metadata(Path::new(&stored)).map_err(|e| anyhow!("cannot open {path}: {e}"))?;
        let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.retry.lock().unwrap().remove(&crate::pool::path_key(&stored));
        self.urgent.lock().unwrap().push_back(Job { path: stored, size: md.len(), mtime, urgent: true });
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

    /// Render a page of a PDF with its marks (also used by the office
    /// server for its clients).
    pub fn render_page(&self, path: &str, page: u32, query: &str, max_px: u32, tag: Option<&str>) -> Result<Event> {
        let stored = self.stored_path(path);
        if FileKind::of(&stored) != FileKind::Pdf {
            bail!("only PDFs have a page image; this file's text is shown instead");
        }
        let r = self.rendered(&stored, page, max_px.clamp(400, 3000))?;
        let (pages, page, width, height) = (r.pages, r.page, r.width, r.height);
        let terms: Vec<Term> = parse_search(query).all_terms();
        let norm = tag.map(sleuth_core::analyzer::norm_form);
        let marks = self.pool.with_file(&stored, |s, id| {
            if s.file_by_id(id)?.is_none_or(|f| f.read_state != state::READ) {
                return Ok(None);
            }
            let info = s.page_info(id)?;
            let ocr = info.iter().find(|p| p.page == page).is_some_and(|p| p.ocr_tiles > p.ocr_tiles_skipped);
            let mut boxes = s.page_boxes(id, page, &terms)?;
            if let Some(n) = &norm {
                for b in s.tag_boxes(id, page, n)? {
                    if !boxes.iter().any(|x| x.overlap(&b) > 0.9) {
                        boxes.push(b);
                    }
                }
            }
            Ok(Some((boxes, if ocr { TextSource::Ocr } else { TextSource::TextLayer })))
        })?;
        let (boxes, source) = marks.flatten().unwrap_or((Vec::new(), TextSource::Pending));
        Ok(Event::Preview { png_base64: r.png_base64.clone(), width, height, pages, boxes, source })
    }

    /// A page image for the preview, from the last few made when the file
    /// has not changed. PNG with fast compression: a 3000 px drawing is
    /// made in a fraction of the time of the default setting, and lines and
    /// text stay exact (no JPEG blur on a drawing).
    fn rendered(&self, stored: &str, page: u32, max_px: u32) -> Result<Arc<Rendered>> {
        let stamp = std::fs::metadata(stored).map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()))).unwrap_or((0, 0));
        let key = format!("{}|{page}|{max_px}|{}|{}", stored.to_lowercase(), stamp.0, stamp.1);
        if let Some((_, r)) = self.previews.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return Ok(Arc::clone(r));
        }
        let doc = crate::pdf::Doc::open(Path::new(stored))?;
        let pages = doc.page_count();
        let page = page.clamp(1, pages.max(1));
        let img = image::DynamicImage::ImageRgba8(doc.render_preview(page - 1, max_px)?).to_rgb8();
        let (width, height) = (img.width(), img.height());
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive).write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgb8)?;
        }
        let r = Arc::new(Rendered { png_base64: base64::engine::general_purpose::STANDARD.encode(png), width, height, pages, page });
        let mut cache = self.previews.lock().unwrap();
        cache.retain(|(k, _)| *k != key);
        cache.push_back((key, Arc::clone(&r)));
        while cache.len() > PREVIEW_MEMORY {
            cache.pop_front();
        }
        Ok(r)
    }

    /// One page (sheet, slide) of the text read from a file that has no
    /// page image, with the lines matching the query marked.
    pub fn text_preview(&self, path: &str, page: u32, query: &str, tag: Option<&str>) -> Result<Event> {
        let stored = self.stored_path(path);
        let kind = FileKind::of(&stored);
        let terms: Vec<Term> = parse_search(query).all_terms();
        let tag_term = tag.and_then(Term::new);
        let r = self.pool.with_file(&stored, |s, id| {
            if s.file_by_id(id)?.is_none_or(|f| f.read_state != state::READ) {
                return Ok(None);
            }
            let info = s.page_info(id)?;
            let pages = info.len().max(1) as u32;
            let page = page.clamp(1, pages);
            let label = info.iter().find(|p| p.page == page).map(|p| p.note.clone()).unwrap_or_default();
            let text = s.file_text(id)?;
            let all: Vec<&crate::text::Line> = text.pages.iter().filter(|p| p.page == page).flat_map(|p| p.lines.iter()).collect();
            let truncated = all.len() > TEXT_PREVIEW_LINES;
            let lines = all
                .into_iter()
                .take(TEXT_PREVIEW_LINES)
                .map(|l| TextLine { hit: terms.iter().chain(tag_term.iter()).any(|t| t.classify(&l.text, l.conf).is_some()), text: l.text.clone(), conf: l.conf })
                .collect();
            Ok(Some(Event::TextPreview { kind, pages, page, label, lines, truncated, source: TextSource::TextLayer }))
        })?;
        match r.flatten() {
            Some(e) => Ok(e),
            None => Ok(Event::TextPreview { kind, pages: 0, page: 0, label: String::new(), lines: Vec::new(), truncated: false, source: TextSource::Pending }),
        }
    }

    /// A rectangle of a page as PNG, with some margin, about `max_px` wide.
    pub fn crop(&self, path: &str, page: u32, b: NormBox, max_px: u32) -> Result<Event> {
        let doc = crate::pdf::Doc::open(Path::new(&self.stored_path(path)))?;
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
}

// The deep-search progress enum shares its name with the catalog's root state.
use sleuth_core::protocol::RootState as RootState_;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_folders_by_letter_become_network_paths() {
        use sleuth_core::paths::Drive;
        let drives = DriveMap {
            drives: vec![Drive { letter: "Z:".into(), kind: DriveKind::Network, unc: Some(r"\\10.0.0.20\Project".into()) }, Drive { letter: "C:".into(), kind: DriveKind::Fixed, unc: None }],
        };
        let address = |h: &str| if h.eq_ignore_ascii_case("docsrv") { Some("10.0.0.20".to_string()) } else { Some(h.to_string()) };
        let chosen = vec![r"Z:\".to_string(), r"z:\J100".to_string(), r"\\Docsrv\Project\".to_string(), r"C:\Work".to_string(), " ".to_string()];
        let r = resolve_scopes(&chosen, &drives, &address);
        assert_eq!(r, vec![r"\\10.0.0.20\Project".to_string(), r"\\10.0.0.20\Project\J100".to_string(), r"C:\Work".to_string()]);
        // The files the catalog holds under the share are inside the folder.
        assert!(under(&r"\\10.0.0.20\Project\J100\iso.pdf".to_lowercase(), &r));
    }

    #[test]
    fn scopes_match_whole_folders() {
        let s = vec![r"\\srv\proj".to_string()];
        assert!(under(r"\\srv\proj\a.pdf", &s));
        assert!(!under(r"\\srv\project2\a.pdf", &s));
    }

    #[test]
    fn counts_read_with_thousands_separators() {
        assert_eq!(fmt_count(7_444_428), "7,444,428");
        assert_eq!(fmt_count(12), "12");
    }
}
