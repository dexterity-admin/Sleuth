//! The engine that `sleuth-worker.exe` runs: background name indexing, the
//! PDF reading queue, searches, deep searches and page previews.

use crate::cache::{SharedCache, check_cache, hash_file};
use crate::crawl::{WalkConfig, WalkEvent, norm_dir, walk};
use crate::models;
use crate::ocr::OcrReader;
use crate::reader::{ReadOptions, read_pdf};
use crate::search::search_index;
use crate::store::{Store, state};
use crate::sys::{CpuMeter, idle_seconds, on_battery};
use anyhow::{Context, Result};
use base64::Engine as _;
use sleuth_core::analyzer::{Term, parse_query};
use sleuth_core::paths::{DriveKind, DriveMap};
use sleuth_core::protocol::{CacheCheck, CacheState, Envelope, Event, Failed, Hit, RootProgress, RootState, Status, TextSource};
use sleuth_core::settings::{Settings, data_dir};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub type EventTx = tokio::sync::mpsc::UnboundedSender<Envelope<Event>>;

/// Unread PDFs a single deep search may queue for reading.
const DEEP_READ_LIMIT: usize = 400;

fn now() -> i64 {
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

struct Waiter {
    search_id: u64,
    terms: Vec<Term>,
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
    /// Wait until raised or `timeout`; clears the flag.
    fn wait(&self, timeout: Duration) {
        let g = self.flag.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |raised| !*raised).unwrap();
        *g = false;
    }
}

pub struct Engine {
    pub settings: RwLock<Settings>,
    db_path: PathBuf,
    pub drives: RwLock<DriveMap>,
    status: Mutex<Status>,
    reader_signal: Signal,
    indexer_signal: Signal,
    cache_signal: Signal,
    cache: Mutex<CacheCheck>,
    waiting: Mutex<HashMap<String, Vec<Waiter>>>,
    searches: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    pages_read: Mutex<VecDeque<(Instant, u32)>>,
    pub shutdown: Arc<AtomicBool>,
    high_download_started: AtomicBool,
}

impl Engine {
    pub fn new(settings: Settings) -> Arc<Engine> {
        Arc::new(Engine {
            settings: RwLock::new(settings),
            db_path: data_dir().join("index.db"),
            drives: RwLock::new(DriveMap::current()),
            status: Mutex::new(Status { worker_version: env!("CARGO_PKG_VERSION").into(), reader_state: "starting".into(), ..Default::default() }),
            reader_signal: Signal::default(),
            indexer_signal: Signal::default(),
            cache_signal: Signal::default(),
            cache: Mutex::new(CacheCheck::default()),
            waiting: Mutex::new(HashMap::new()),
            searches: Mutex::new(HashMap::new()),
            pages_read: Mutex::new(VecDeque::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            high_download_started: AtomicBool::new(false),
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

    pub fn start(self: &Arc<Self>) -> Result<()> {
        self.open_store().context("open index")?;
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-indexer".into()).spawn(move || e.indexer_loop())?;
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-reader".into()).spawn(move || e.reader_loop())?;
        let e = Arc::clone(self);
        std::thread::Builder::new().name("sleuth-cache".into()).spawn(move || e.cache_loop())?;
        Ok(())
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.reader_signal.raise();
        self.indexer_signal.raise();
        self.cache_signal.raise();
        for flag in self.searches.lock().unwrap().values() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn rescan_now(&self) {
        self.indexer_signal.raise();
    }

    pub fn set_settings(&self, s: Settings) -> Result<()> {
        s.save()?;
        let scopes_changed = self.settings.read().unwrap().content_scopes != s.content_scopes;
        *self.settings.write().unwrap() = s.clone();
        self.cache_signal.raise();
        if scopes_changed {
            self.open_store()?.apply_scopes(&s.content_scopes)?;
            self.reader_signal.raise();
        }
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
        let fails = store.failures(50)?.into_iter().map(|(path, reason)| Failed { path, reason }).collect();
        Ok((st, fails))
    }

    /// Roots of every fixed and network drive, minus whole drives the user excluded.
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

    fn walk_roots(&self) -> Vec<String> {
        let drives = self.drives.read().unwrap();
        let excluded: Vec<String> = self.settings.read().unwrap().excluded.iter().map(|x| x.trim_end_matches('\\').to_lowercase()).collect();
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

    // ---- background name index -------------------------------------------

    fn indexer_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        while !self.shutdown.load(Ordering::SeqCst) {
            *self.drives.write().unwrap() = DriveMap::current();
            let roots = self.walk_roots();
            info!("name index: walking {} roots", roots.len());
            // A bug in one pass must not stop indexing for the rest of the session.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.index_names(&roots))) {
                Ok(Ok(())) => info!("name index pass finished"),
                Ok(Err(e)) => warn!("name index pass failed: {e:#}"),
                Err(_) => warn!("name index pass crashed; retrying next pass"),
            }
            let mins = self.settings.read().unwrap().name_rescan_minutes.max(15);
            self.indexer_signal.wait(Duration::from_secs(mins as u64 * 60));
        }
    }

    fn index_names(&self, roots: &[String]) -> Result<()> {
        let s = self.settings.read().unwrap().clone();
        let pass = now();
        let cfg = WalkConfig {
            per_server: 2,
            local_threads: 2,
            listing_timeout: Duration::from_secs(s.listing_timeout_secs.max(5) as u64),
            excluded: s.excluded.iter().map(|x| x.to_lowercase()).collect(),
            background: true,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<WalkEvent>(256);
        let store = self.open_store()?;
        let scopes = s.content_scopes.clone();
        let reader_signal = &self.reader_signal;
        std::thread::scope(|sc| {
            sc.spawn(move || {
                let mut since_wake = 0usize;
                for ev in rx {
                    if let WalkEvent::Listed { dir, entries, .. } = ev {
                        let r = (|| -> Result<usize> {
                            store.conn.execute_batch("BEGIN")?;
                            let parent = Path::new(&dir).parent().map(|p| norm_dir(&p.to_string_lossy()));
                            store.touch_dir(&dir, parent.as_deref().filter(|p| *p != dir), pass)?;
                            let mut queued = 0;
                            for e in &entries {
                                let path = if dir.ends_with('\\') { format!("{dir}{}", e.name) } else { format!("{dir}\\{}", e.name) };
                                if e.is_dir {
                                    store.touch_dir(&path, Some(&dir), pass)?;
                                } else {
                                    let scope = is_pdf(&e.name).then(|| under(&path.to_lowercase(), &scopes));
                                    let (_, changed) = store.upsert_file(&path, &dir, &e.name, e.size, e.mtime, pass, scope)?;
                                    if changed && scope == Some(true) {
                                        queued += 1;
                                    }
                                }
                            }
                            store.reconcile_dir(&dir, pass)?;
                            store.conn.execute_batch("COMMIT")?;
                            Ok(queued)
                        })();
                        match r {
                            Ok(q) => since_wake += q,
                            Err(e) => {
                                let _ = store.conn.execute_batch("ROLLBACK");
                                warn!("index {dir}: {e:#}");
                            }
                        }
                        if since_wake >= 20 {
                            reader_signal.raise();
                            since_wake = 0;
                        }
                    }
                }
                if since_wake > 0 {
                    reader_signal.raise();
                }
            });
            walk(roots, &cfg, &self.shutdown, &|ev| {
                match &ev {
                    WalkEvent::Skipped { dir, reason, .. } => info!("skipped {dir}: {reason}"),
                    WalkEvent::RootOffline { root, reason } => info!("offline {root}: {reason}"),
                    _ => {}
                }
                let _ = tx.send(ev);
            });
            drop(tx);
        });
        Ok(())
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
        let mut ocr: Option<(OcrReader, usize, String)> = None;
        let mut last_used = Instant::now();
        let mut meter = CpuMeter::default();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        meter.others_percent();
        while !self.shutdown.load(Ordering::SeqCst) {
            let s = self.settings.read().unwrap().clone();
            if on_battery() {
                self.set_reader_state("paused: on battery", None);
                self.reader_signal.wait(Duration::from_secs(60));
                continue;
            }
            let idle = idle_seconds();
            let others = meter.others_percent();
            if others > s.busy_cpu_percent as f32 && idle < 120 {
                self.set_reader_state("paused: PC busy", None);
                self.reader_signal.wait(Duration::from_secs(5));
                continue;
            }
            let job = match store.next_to_read(now()) {
                Ok(Some(j)) => j,
                Ok(None) => {
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
            let quality_key = format!("{:?}", s.ocr_quality);
            if ocr.as_ref().is_some_and(|(_, t, q)| *t != threads || *q != quality_key) {
                ocr = None;
            }
            self.set_reader_state("reading", Some(job.path.clone()));
            let result = self.read_one(&mut store, &job.path, job.id, s.ocr_dpi, &s, &mut ocr, threads, &quality_key);
            last_used = Instant::now();
            match result {
                Ok(pages) => {
                    self.pages_read.lock().unwrap().push_back((Instant::now(), pages));
                    self.notify_waiters(&store, &job.path, job.id);
                }
                Err(e) if self.shutdown.load(Ordering::SeqCst) => {
                    let _ = store.requeue(job.id);
                    info!("stopped while reading {}: {e:#}", job.path);
                }
                Err(e) if is_transient(&e) => {
                    let reason = friendly(&e);
                    match store.defer(job.id, &reason, now(), 5) {
                        Ok(true) => info!("will retry {} later: {e:#}", job.path),
                        _ => {
                            warn!("giving up on {}: {e:#}", job.path);
                            self.notify_waiters(&store, &job.path, job.id);
                        }
                    }
                }
                Err(e) => {
                    let reason = friendly(&e);
                    warn!("could not read {}: {e:#}", job.path);
                    let _ = store.save_failure(job.id, &reason, now());
                    self.notify_waiters(&store, &job.path, job.id);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_one(
        &self,
        store: &mut Store,
        path: &str,
        id: i64,
        dpi: u32,
        s: &Settings,
        ocr: &mut Option<(OcrReader, usize, String)>,
        threads: usize,
        quality_key: &str,
    ) -> Result<u32> {
        let hash = hash_file(Path::new(path)).map_err(|e| anyhow::anyhow!("cannot open file: {e}"))?;
        // Same content already read here (copied or renamed file).
        if let Some(other) = store.read_copy_by_hash(&hash, id)? {
            let text = store.file_text(other)?;
            store.save_read(id, &hash, &text, now())?;
            return Ok(0);
        }
        let shared = {
            let c = self.cache.lock().unwrap();
            (c.state == CacheState::Ok).then(|| SharedCache::new(c.path.clone()))
        };
        let _ = s;
        if let Some(c) = &shared
            && let Some(text) = c.get(&hash)
        {
            store.save_read(id, &hash, &text, now())?;
            return Ok(0);
        }
        if ocr.is_none() {
            self.set_reader_state("loading OCR models", Some(path.to_string()));
            let (set, fetch_high) = models::choose(s.ocr_quality)?;
            if fetch_high && !self.high_download_started.swap(true, Ordering::SeqCst) {
                std::thread::spawn(|| match models::download_high() {
                    Ok(()) => info!("high-accuracy OCR models downloaded"),
                    Err(e) => warn!("could not download high-accuracy models: {e:#}"),
                });
            }
            let reader = OcrReader::new(&set, threads)?;
            self.status.lock().unwrap().ocr_model = reader.model.clone();
            *ocr = Some((reader, threads, quality_key.to_string()));
            self.set_reader_state("reading", Some(path.to_string()));
        }
        let reader = &ocr.as_ref().unwrap().0;
        let stop = &self.shutdown;
        let cancel = || stop.load(Ordering::SeqCst);
        let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_pdf(Path::new(path), Some(reader), &ReadOptions { dpi, cancel: &cancel })))
            .map_err(|_| anyhow::anyhow!("the reader crashed on this file"))??;
        let pages = text.pages.len() as u32;
        store.save_read(id, &hash, &text, now())?;
        if let Some(c) = &shared
            && !text.model.is_empty()
            && let Err(e) = c.put(&hash, &text)
        {
            warn!("shared cache write failed: {e:#}");
        }
        Ok(pages)
    }

    fn notify_waiters(&self, store: &Store, path: &str, file_id: i64) {
        let Some(waiters) = self.waiting.lock().unwrap().remove(&path.to_lowercase()) else { return };
        let drives = self.drives.read().unwrap().clone();
        for w in waiters {
            let hits: Vec<Hit> = search_index(store, &w.terms, &[path.to_string()], 5, &drives)
                .unwrap_or_default()
                .into_iter()
                .filter(|h| h.path.eq_ignore_ascii_case(path))
                .collect();
            let _ = file_id;
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

    /// Runs on a blocking thread; streams events to `tx`.
    pub fn search(&self, id: u64, query: &str, roots: &[String], deep: bool, limit: usize, tx: &EventTx) -> Result<()> {
        let started = Instant::now();
        let terms = parse_query(query);
        if terms.is_empty() {
            let _ = tx.send(Envelope { id, body: Event::Done { total: 0, millis: 0 } });
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.searches.lock().unwrap().insert(id, Arc::clone(&cancel));
        let drives = self.drives.read().unwrap().clone();
        let store = self.open_store()?;
        let hits = search_index(&store, &terms, roots, limit, &drives)?;
        let mut reported: HashSet<String> = hits.iter().map(|h| h.path.to_lowercase()).collect();
        let mut total = hits.len();
        if !hits.is_empty() {
            let _ = tx.send(Envelope { id, body: Event::Hits { hits } });
        }
        if deep && !cancel.load(Ordering::SeqCst) {
            total += self.deep_search(id, &terms, roots, &drives, &cancel, &mut reported, tx)?;
        }
        self.searches.lock().unwrap().remove(&id);
        let _ = tx.send(Envelope { id, body: Event::Done { total, millis: started.elapsed().as_millis() as u64 } });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn deep_search(&self, id: u64, terms: &[Term], roots: &[String], drives: &DriveMap, cancel: &Arc<AtomicBool>, reported: &mut HashSet<String>, tx: &EventTx) -> Result<usize> {
        let s = self.settings.read().unwrap().clone();
        let roots: Vec<String> = if roots.is_empty() { self.walk_roots() } else { roots.iter().map(|r| drives.to_unc(r)).collect() };
        let label = |r: &str| drives.to_display(r).trim_end_matches('\\').to_string();
        let progress: Mutex<Vec<RootProgress>> = Mutex::new(
            roots
                .iter()
                .map(|r| RootProgress { root: r.clone(), label: label(r), state: RootState::Searching, folders_done: 0, files_seen: 0, hits: 0, skipped: vec![] })
                .collect(),
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
        let cfg = WalkConfig {
            per_server: s.listings_per_server.max(1) as usize,
            local_threads: 6,
            listing_timeout: Duration::from_secs(s.listing_timeout_secs.max(5) as u64),
            excluded: s.excluded.iter().map(|x| x.to_lowercase()).collect(),
            background: false,
        };
        walk(&roots, &cfg, cancel, &|ev| {
            match ev {
                WalkEvent::Listed { root, dir, entries } => {
                    let mut new_hits = Vec::new();
                    let mut files = 0u64;
                    for e in entries.iter().filter(|e| !e.is_dir) {
                        files += 1;
                        let path = if dir.ends_with('\\') { format!("{dir}{}", e.name) } else { format!("{dir}\\{}", e.name) };
                        let lower = path.to_lowercase();
                        let classes: Vec<_> = terms.iter().map(|t| t.classify(&e.name, 1.0)).collect();
                        let name_class = if classes.iter().all(|c| c.is_some()) { classes.into_iter().flatten().max() } else { None };
                        let mut g = found.lock().unwrap();
                        if g.0.contains(&lower) {
                            continue;
                        }
                        if let Some(class) = name_class.filter(|c| *c != sleuth_core::analyzer::MatchClass::Uncertain) {
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
                            });
                        }
                        if is_pdf(&e.name) && g.2 < DEEP_READ_LIMIT {
                            let st = store.lock().unwrap();
                            let row = st.file_by_path(&path).ok().flatten();
                            let needs = match &row {
                                None => true,
                                Some(r) => r.read_state != state::READ && r.read_state != state::FAILED,
                            };
                            if needs {
                                if row.is_none() {
                                    let _ = st.upsert_file(&path, &dir, &e.name, e.size, e.mtime, 0, Some(true));
                                }
                                if st.request_read(&path).unwrap_or(false) {
                                    g.2 += 1;
                                    self.waiting.lock().unwrap().entry(lower.clone()).or_default().push(Waiter { search_id: id, terms: terms.to_vec(), tx: tx.clone() });
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
            store.upsert_file(&unc, &dir, &name, md.len(), mtime, 0, Some(true))?;
        }
        store.request_read(&unc)?;
        self.reader_signal.raise();
        Ok(())
    }

    // ---- preview -----------------------------------------------------------

    pub fn preview(&self, path: &str, page: u32, query: &str, max_px: u32) -> Result<Event> {
        let doc = crate::pdf::Doc::open(Path::new(path))?;
        let pages = doc.page_count();
        let page = page.clamp(1, pages.max(1));
        let img = doc.render_preview(page - 1, max_px.clamp(400, 3000))?;
        let (width, height) = (img.width(), img.height());
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        let store = self.open_store()?;
        let terms = parse_query(query);
        let (boxes, source) = match store.file_by_path(path)? {
            Some(f) if f.read_state == state::READ => {
                let ft = store.file_text(f.id)?;
                let src = ft.pages.iter().find(|p| p.page == page).and_then(|p| p.lines.iter().find(|l| l.source == TextSource::Ocr)).map(|_| TextSource::Ocr).unwrap_or(TextSource::TextLayer);
                (store.page_boxes(f.id, page, &terms)?, src)
            }
            _ => (Vec::new(), TextSource::Pending),
        };
        Ok(Event::Preview { png_base64: base64::engine::general_purpose::STANDARD.encode(png), width, height, pages, boxes, source })
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
}
